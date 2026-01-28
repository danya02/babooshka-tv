use std::{
    collections::HashMap,
    io::ErrorKind,
    process::Stdio,
    sync::{Arc, nonpoison::Mutex},
    time::Duration,
};

use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt},
    net::{
        UnixStream,
        unix::{OwnedReadHalf, OwnedWriteHalf},
    },
};
use tracing::{debug, info, instrument, warn};

use crate::watch_later::WatchLaterState;

pub struct MpvPlayer {
    process: tokio::process::Child,
    socket: OwnedWriteHalf,
    recv_loop: tokio::task::JoinHandle<()>,
    last_cmd_id: usize,
    waiting_command_statuses:
        Arc<Mutex<HashMap<usize, tokio::sync::oneshot::Sender<CommandResponse>>>>,
    waiting_events: Arc<
        Mutex<
            Vec<(
                Box<dyn Fn(&EventData) -> bool + 'static + Send>,
                tokio::sync::oneshot::Sender<EventData>,
            )>,
        >,
    >,
}

#[derive(serde::Serialize, Debug)]
struct CommandMsg {
    command: Vec<serde_json::Value>,
    request_id: usize,
    #[serde(rename = "async")]
    asynk: bool,
}

#[derive(serde::Deserialize, Debug)]
pub struct CommandResponse {
    pub request_id: usize,
    pub error: String,
    pub data: Option<serde_json::Value>,
}

#[derive(serde::Deserialize, Debug, Clone)]
pub struct EventData {
    pub id: Option<usize>,
    pub event: String,
    pub data: Option<serde_json::Value>,
    pub name: Option<String>,
}

impl MpvPlayer {
    pub async fn new(init_file: &str) -> Result<Self, std::io::Error> {
        let mut process = tokio::process::Command::new("mpv");
        let process = process
            .arg("--input-ipc-server=/tmp/run/mpv-ipc.sock")
            .arg("--keep-open=yes")
            .arg("--fullscreen")
            .arg(init_file)
            .stdin(Stdio::null())
            .process_group(0)
            .kill_on_drop(true);
        // let process = unsafe {
        //     process.pre_exec(|| {
        //         libc::signal(libc::SIGINT, libc::SIG_IGN);
        //         Ok(())
        //     })
        // };

        let mut process = process.spawn().expect("failed to spawn mpv");

        let mut socket = None;
        for _ in 0..20 {
            match UnixStream::connect("/tmp/run/mpv-ipc.sock").await {
                Ok(conn) => {
                    socket = Some(conn);
                    break;
                }
                Err(_) => {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            }
        }

        let Some(socket) = socket else {
            // could not connect, probably the process failed to start
            process.kill().await.expect("failed to kill mpv");
            return Err(std::io::Error::new(
                ErrorKind::NotConnected,
                "could not connect to mpv",
            ));
        };

        let waiting_command_statuses = Arc::new(Mutex::new(HashMap::new()));
        let waiting_events = Arc::new(Mutex::new(Vec::new()));

        let (rx, tx) = socket.into_split();

        let recv_loop = {
            let waiting_command_statuses = waiting_command_statuses.clone();
            let waiting_events = waiting_events.clone();
            tokio::spawn(MpvPlayer::recv_loop(
                rx,
                waiting_command_statuses,
                waiting_events,
            ))
        };

        let mut out = Self {
            process,
            socket: tx,
            recv_loop,
            waiting_command_statuses,
            waiting_events,
            last_cmd_id: 0,
        };

        out.send_cmd(vec!["observe_property".json(), 1.json(), "time-pos".json()])
            .await?;

        Ok(out)
    }

    pub fn is_running(&mut self) -> bool {
        self.process.try_wait().expect("failed to wait process");
        self.process.id().is_some()
    }

    async fn recv_loop(
        socket: OwnedReadHalf,
        waiting_command_statuses: Arc<
            Mutex<HashMap<usize, tokio::sync::oneshot::Sender<CommandResponse>>>,
        >,
        waiting_events: Arc<
            Mutex<
                Vec<(
                    Box<dyn Fn(&EventData) -> bool + 'static + Send>,
                    tokio::sync::oneshot::Sender<EventData>,
                )>,
            >,
        >,
    ) {
        let mut reader = tokio::io::BufReader::new(socket);
        let mut buf = String::new();

        #[derive(serde::Deserialize)]
        #[serde(untagged)]
        enum RecvMsg {
            Command(CommandResponse),
            Event(EventData),
        }

        loop {
            let count = reader
                .read_line(&mut buf)
                .await
                .expect("failed to read line");
            if count == 0 {
                return;
            }

            match serde_json::from_str(&buf) {
                Ok(RecvMsg::Command(cmd)) => {
                    if cmd.error == "success" {
                        debug!("mpv sent command response: {cmd:?}");
                    } else {
                        warn!("mpv sent command response: {cmd:?}");
                    }
                    if let Some(tx) = waiting_command_statuses.lock().remove(&cmd.request_id) {
                        let id = cmd.request_id;
                        if let Err(_) = tx.send(cmd) {
                            warn!("Listener for command id {id} dropped before receiving response");
                        };
                    } else {
                        println!(
                            "got unexpected command response (we aren't waiting for this ID): {cmd:#?}"
                        );
                    }
                }
                Ok(RecvMsg::Event(evt)) => {
                    let mut waiting = waiting_events.lock();

                    // property-change events for time-pos are spammy, skip them for display
                    if !(evt.event == "property-change"
                        && evt.name.as_ref().is_some_and(|v| v == "time-pos"))
                    {
                        debug!(
                            "mpv sent event: {evt:?}, checking it against {} current predicates",
                            waiting.len()
                        );
                    }
                    let mut removed = 0;
                    for idx in 0..waiting.len() {
                        let predicate = &waiting[idx - removed].0;
                        if predicate(&evt) {
                            debug!("event matches predicate at index {idx}");
                            let (_, tx) = waiting.remove(idx);
                            removed += 1;
                            tx.send(evt.clone()).expect("failed to send event");
                        } else {
                            debug!("event does not match predicate at index {idx}");
                        }
                    }
                }
                Err(e) => {
                    println!("failed to deserialize message {buf:?}: {e}");
                }
            }
            buf.clear();
        }
    }

    #[instrument(skip(self))]
    async fn send<T: serde::Serialize + std::fmt::Debug>(
        &mut self,
        msg: T,
    ) -> Result<(), std::io::Error> {
        let msg = serde_json::to_string(&msg).expect("failed to serialize message");
        self.socket.write_all(msg.as_bytes()).await?;
        self.socket.write(b"\n").await?;
        self.socket.flush().await?;
        debug!("sent command");
        Ok(())
    }

    #[instrument(skip(self))]
    pub async fn send_quit(&mut self) -> Result<(), std::io::Error> {
        self.send(CommandMsg {
            command: vec!["quit".into()],
            request_id: 0,
            asynk: false,
        })
        .await
    }

    #[instrument(skip(self))]
    pub async fn send_cmd(
        &mut self,
        command: Vec<impl Into<serde_json::Value> + std::fmt::Debug>,
    ) -> Result<CommandResponse, std::io::Error> {
        let id = self.last_cmd_id + 1;
        self.last_cmd_id += 1;
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.waiting_command_statuses.lock().insert(id, tx);
        self.send(CommandMsg {
            command: command.into_iter().map(|s| s.into()).collect(),
            request_id: id,
            asynk: true,
        })
        .await?;
        Ok(rx.await.expect("recv_loop closed"))
    }

    #[instrument(skip(self))]
    pub async fn osd_text(&mut self, text: &str) -> Result<(), std::io::Error> {
        self.send(CommandMsg {
            command: vec!["show-text".into(), text.into()],
            request_id: 0,
            asynk: false,
        })
        .await
    }

    #[instrument(skip(self))]
    pub async fn set_paused(&mut self, paused: bool) -> Result<(), std::io::Error> {
        self.send_cmd(vec!["set_property".json(), "pause".json(), paused.json()])
            .await?;
        Ok(())
    }

    #[instrument(skip(self))]
    pub async fn get_path(&mut self) -> Result<String, std::io::Error> {
        let response = self
            .send_cmd(vec!["get_property".json(), "path".json()])
            .await?;
        Ok(response
            .data
            .unwrap_or_default()
            .as_str()
            .unwrap_or_default()
            .to_string())
    }

    #[instrument(skip(self))]
    pub async fn get_playback_time(&mut self) -> Result<f64, std::io::Error> {
        let response = self
            .send_cmd(vec!["get_property".json(), "time-pos/full".json()])
            .await?;
        Ok(response
            .data
            .unwrap_or_default()
            .as_f64()
            .unwrap_or_default())
    }

    #[instrument(skip(self))]
    pub async fn set_playback_time(&mut self, time: f64) -> Result<(), std::io::Error> {
        self.send_cmd(vec!["set_property".json(), "time-pos".json(), time.json()])
            .await?;
        Ok(())
    }

    #[instrument(skip(self))]
    pub async fn loadfile(&mut self, path: &str) -> Result<(), std::io::Error> {
        debug!("loading media");
        self.send_cmd(vec!["loadfile".json(), path.json()]).await?;
        Ok(())
    }

    #[instrument(skip(self, event_match))]
    pub async fn wait_for_event(
        &self,
        event_match: impl Fn(&EventData) -> bool + 'static + Send,
    ) -> Result<EventData, std::io::Error> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.waiting_events.lock().push((Box::new(event_match), tx));
        debug!("event subscription created");
        let event = rx.await.map_err(|_| {
            std::io::Error::new(
                ErrorKind::Other,
                "event receiver dropped -- probably event loop crashed or player exited",
            )
        })?;
        debug!("event received: {event:?}");
        Ok(event)
    }

    #[instrument(skip(self))]
    pub async fn wait_for_event_by_name(&self, name: &str) -> Result<EventData, std::io::Error> {
        let name = name.to_string();
        debug!("waiting for event by name");
        let ev = self
            .wait_for_event(move |e| (e.name.as_ref()).is_some_and(|n| name == *n))
            .await;
        debug!("event received: {:?}", ev);
        ev
    }

    #[instrument(skip(self))]
    pub async fn save_state(&mut self) -> Result<WatchLaterState, std::io::Error> {
        let path = self.get_path().await?;
        let time = self.get_playback_time().await?;
        debug!("Saving state: path: {path}, time: {time}");
        Ok(WatchLaterState {
            path: path.into(),
            time,
        })
    }

    #[instrument(skip(self))]
    pub async fn restore_state(&mut self, state: &WatchLaterState) -> Result<(), std::io::Error> {
        info!("stopping current playback");
        self.send_cmd(vec!["stop".json()]).await?;

        info!("commanding load file {}", state.path.display());
        self.loadfile(state.path.to_string_lossy().as_ref()).await?;
        info!("waiting for file to load");
        self.wait_for_event(|e| {
            warn!("event: {e:?}");
            e.event == "file-loaded"
                || (e.event == "property-change")
                    && e.name.as_ref().is_some_and(|n| n == "time-pos")
        })
        .await?;

        // TODO: how to ensure that the player is ready?
        tokio::time::sleep(Duration::from_secs(1)).await;

        info!("Setting playback position to {}", state.time);
        self.set_playback_time(state.time).await?;
        Ok(())
    }

    pub async fn get_file_duration(&mut self) -> Result<f64, std::io::Error> {
        let response = self
            .send_cmd(vec!["get_property".json(), "duration/full".json()])
            .await?;
        Ok(response
            .data
            .unwrap_or_default()
            .as_f64()
            .unwrap_or_default())
    }
}

trait ToJson {
    fn json(self) -> serde_json::Value;
}
impl<T> ToJson for T
where
    T: Into<serde_json::Value>,
{
    fn json(self) -> serde_json::Value {
        self.into()
    }
}
