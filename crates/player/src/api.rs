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

// Type aliases to keep the field declarations readable.
type PendingCommands = Arc<Mutex<HashMap<usize, tokio::sync::oneshot::Sender<CommandResponse>>>>;
type PendingEvents = Arc<
    Mutex<
        Vec<(
            Box<dyn Fn(&EventData) -> bool + 'static + Send>,
            tokio::sync::oneshot::Sender<EventData>,
        )>,
    >,
>;

pub struct MpvPlayer {
    process: tokio::process::Child,
    socket: OwnedWriteHalf,
    last_cmd_id: usize,
    pending_commands: PendingCommands,
    pending_events: PendingEvents,
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
        // Ensure the directory for the IPC socket exists.
        const IPC_SOCK: &str = "/tmp/run/mpv-ipc.sock";
        tokio::fs::create_dir_all("/tmp/run").await.ok();

        // Explicitly forward display/runtime vars so mpv always gets a video
        // output even when systemd's PassEnvironment hasn't propagated them yet.
        let mut cmd = tokio::process::Command::new("mpv");
        for var in &["WAYLAND_DISPLAY", "DISPLAY", "XDG_RUNTIME_DIR"] {
            if let Ok(val) = std::env::var(var) {
                cmd.env(var, val);
            }
        }
        let mut process = cmd
            .arg(format!("--input-ipc-server={IPC_SOCK}"))
            .arg("--keep-open=yes")
            // Use PipeWire directly; fall back to PulseAudio compat, then ALSA.
            // This prevents mpv from grabbing the ALSA device exclusively.
            .arg("--ao=pipewire,pulse,alsa")
            .arg("--vo=wlshm")
            .arg(init_file)
            .stdin(Stdio::null())
            .process_group(0)
            .kill_on_drop(true)
            .spawn()
            .expect("failed to spawn mpv");

        // Wait up to 2 seconds for the IPC socket to appear.
        let mut socket = None;
        for _ in 0..20 {
            match UnixStream::connect(IPC_SOCK).await {
                Ok(conn) => {
                    socket = Some(conn);
                    break;
                }
                Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
            }
        }

        let Some(socket) = socket else {
            process.kill().await.expect("failed to kill mpv");
            return Err(std::io::Error::new(
                ErrorKind::NotConnected,
                format!("could not connect to mpv IPC socket at {IPC_SOCK}"),
            ));
        };

        let pending_commands: PendingCommands = Arc::new(Mutex::new(HashMap::new()));
        let pending_events: PendingEvents = Arc::new(Mutex::new(Vec::new()));

        let (read_half, write_half) = socket.into_split();

        // Detach the recv loop — it holds Arc clones and will stop naturally
        // when the socket closes (i.e. when mpv exits).
        tokio::spawn(Self::recv_loop(
            read_half,
            pending_commands.clone(),
            pending_events.clone(),
        ));

        let mut player = Self {
            process,
            socket: write_half,
            last_cmd_id: 0,
            pending_commands,
            pending_events,
        };

        player
            .send_cmd(vec!["observe_property".json(), 1.json(), "time-pos".json()])
            .await?;

        Ok(player)
    }

    pub fn is_running(&mut self) -> bool {
        match self.process.try_wait() {
            Ok(Some(_)) => false, // process has exited
            Ok(None) => true,     // process is still running
            Err(e) => {
                warn!("Failed to check mpv process status: {e}");
                false
            }
        }
    }

    async fn recv_loop(
        socket: OwnedReadHalf,
        pending_commands: PendingCommands,
        pending_events: PendingEvents,
    ) {
        #[derive(serde::Deserialize)]
        #[serde(untagged)]
        enum RecvMsg {
            Command(CommandResponse),
            Event(EventData),
        }

        let mut reader = tokio::io::BufReader::new(socket);
        let mut buf = String::new();

        loop {
            buf.clear();
            match reader.read_line(&mut buf).await {
                Ok(0) => return, // socket closed
                Ok(_) => {}
                Err(e) => {
                    warn!("mpv IPC read error: {e}");
                    return;
                }
            }

            match serde_json::from_str(&buf) {
                Ok(RecvMsg::Command(cmd)) => {
                    if cmd.error == "success" {
                        debug!("mpv command response: {cmd:?}");
                    } else {
                        warn!("mpv command error response: {cmd:?}");
                    }
                    match pending_commands.lock().remove(&cmd.request_id) {
                        Some(tx) => {
                            let id = cmd.request_id;
                            if tx.send(cmd).is_err() {
                                warn!("Caller for command {id} dropped before receiving response");
                            }
                        }
                        None => {
                            debug!(
                                "Unexpected command response (not waiting for this ID): {cmd:?}"
                            );
                        }
                    }
                }
                Ok(RecvMsg::Event(evt)) => {
                    // time-pos property-change events are very frequent; don't log them.
                    let is_timepos =
                        evt.event == "property-change" && evt.name.as_deref() == Some("time-pos");
                    if !is_timepos {
                        debug!("mpv event: {evt:?}");
                    }

                    let mut waiting = pending_events.lock();
                    let mut i = 0;
                    while i < waiting.len() {
                        if (waiting[i].0)(&evt) {
                            let (_, tx) = waiting.remove(i);
                            if tx.send(evt.clone()).is_err() {
                                warn!("Event subscriber dropped before receiving event");
                            }
                            // don't increment i — the next element shifted into position i
                        } else {
                            i += 1;
                        }
                    }
                }
                Err(e) => {
                    warn!("Failed to deserialize mpv message {buf:?}: {e}");
                }
            }
        }
    }

    #[instrument(skip(self))]
    async fn send<T: serde::Serialize + std::fmt::Debug>(
        &mut self,
        msg: T,
    ) -> Result<(), std::io::Error> {
        let serialized = serde_json::to_string(&msg).expect("failed to serialize mpv message");
        self.socket.write_all(serialized.as_bytes()).await?;
        self.socket.write_all(b"\n").await?;
        self.socket.flush().await?;
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
        self.last_cmd_id += 1;
        let id = self.last_cmd_id;
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.pending_commands.lock().insert(id, tx);
        self.send(CommandMsg {
            command: command.into_iter().map(Into::into).collect(),
            request_id: id,
            asynk: true,
        })
        .await?;
        Ok(rx
            .await
            .expect("recv_loop closed before responding to command"))
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
        self.send_cmd(vec!["loadfile".json(), path.json()]).await?;
        Ok(())
    }

    /// Set the volume gain offset in dB (mpv `volume-gain` property).
    /// Positive values amplify, negative attenuate.  0.0 = no adjustment.
    #[instrument(skip(self))]
    pub async fn set_volume_gain(&mut self, gain_db: f64) -> Result<(), std::io::Error> {
        self.send_cmd(vec![
            "set_property".json(),
            "volume-gain".json(),
            gain_db.json(),
        ])
        .await?;
        Ok(())
    }

    #[instrument(skip(self))]
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

    #[instrument(skip(self, predicate))]
    pub async fn wait_for_event(
        &self,
        predicate: impl Fn(&EventData) -> bool + 'static + Send,
    ) -> Result<EventData, std::io::Error> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.pending_events.lock().push((Box::new(predicate), tx));
        rx.await.map_err(|_| {
            std::io::Error::new(
                ErrorKind::Other,
                "mpv event loop closed before the expected event arrived",
            )
        })
    }

    #[instrument(skip(self))]
    pub async fn save_state(&mut self) -> Result<WatchLaterState, std::io::Error> {
        let path = self.get_path().await?;
        let time = self.get_playback_time().await?;
        debug!("Saving state: path={path}, time={time}");
        Ok(WatchLaterState {
            path: path.into(),
            time,
        })
    }

    #[instrument(skip(self))]
    pub async fn restore_state(&mut self, state: &WatchLaterState) -> Result<(), std::io::Error> {
        info!("Restoring state: {}", state.path.display());
        self.send_cmd(vec!["stop".json()]).await?;
        self.loadfile(state.path.to_string_lossy().as_ref()).await?;

        info!("Waiting for file to load");
        self.wait_for_event(|e| {
            debug!("restore_state waiting, got event: {e:?}");
            e.event == "file-loaded"
                || (e.event == "property-change" && e.name.as_deref() == Some("time-pos"))
        })
        .await?;

        // Give mpv a moment to settle before seeking.
        // TODO: replace with a more reliable readiness signal if one exists.
        tokio::time::sleep(Duration::from_secs(1)).await;

        info!("Seeking to saved position {:.1}s", state.time);
        self.set_playback_time(state.time).await
    }
}

trait ToJson {
    fn json(self) -> serde_json::Value;
}
impl<T: Into<serde_json::Value>> ToJson for T {
    fn json(self) -> serde_json::Value {
        self.into()
    }
}
