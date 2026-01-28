use std::{
    env, fs,
    process::{Child, Stdio},
    time::Duration,
};

use nix::unistd::Pid;
use tokio::{io::AsyncBufReadExt, net::UnixStream, runtime::Builder};
use utils::setup_wayland_env;

struct ChildProcess {
    name: String,
    child: tokio::process::Child,
}

impl ChildProcess {
    async fn wait(&mut self) {
        self.child.wait().await.expect("failed to wait for child");
    }

    fn is_running(&mut self) -> bool {
        self.child.try_wait().expect("failed to wait process");
        self.child.id().is_some()
    }
}

impl Drop for ChildProcess {
    fn drop(&mut self) {
        let running = self.is_running();
        tracing::info!("dropping child {} (still running? {running})", self.name,);
    }
}

trait IntoChild {
    fn into_child(&mut self, name: &str) -> ChildProcess;
}

impl IntoChild for tokio::process::Command {
    fn into_child(&mut self, name: &str) -> ChildProcess {
        self.kill_on_drop(true);

        let mut child = self
            // .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("failed to spawn child");
        tracing::info!("spawned child {}", name);
        let stdout = tokio::io::BufReader::new(child.stdout.take().expect("no stdout"));
        let stderr = tokio::io::BufReader::new(child.stderr.take().expect("no stderr"));

        {
            let name = name.to_owned();
            tokio::spawn(async move {
                let mut stdout = stdout.lines();
                let span = tracing::info_span!("stdout", name = name);
                while let Some(line) = stdout.next_line().await.expect("failed to read stdout") {
                    let _enter = span.enter();
                    tracing::info!("{line}");
                }

                tracing::info!("{name} stdout closed");
            });
        }

        {
            let name = name.to_owned();
            tokio::spawn(async move {
                let mut stderr = stderr.lines();
                let span = tracing::error_span!("stderr", name = name);
                while let Some(line) = stderr.next_line().await.expect("failed to read stderr") {
                    let _enter = span.enter();
                    tracing::error!("{line}");
                }

                tracing::error!("{name} stderr closed");
            });
        }

        ChildProcess {
            name: name.to_string(),
            child,
        }
    }
}

async fn wait_for_socket(path: &str) -> Result<(), std::io::Error> {
    for _ in 0..100 {
        // try to connect to unix socket
        match UnixStream::connect(path).await {
            Ok(_) => return Ok(()),
            Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::TimedOut,
        "timed out waiting for socket",
    ))
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::fmt().init();

    // Step 0: add ./target/debug and ./target/release to PATH
    for i in 0..30 {
        if let Err(why) = steal_graphical_session_env() {
            tracing::error!("failed to steal graphical session env: {why}");
            tokio::time::sleep(Duration::from_millis(500)).await;
            continue;
        }
        break;
    }

    // if let Err(why) = setup_wayland_env() {
    //     tracing::error!("failed to setup wayland env: {why}");
    // }

    unsafe {
        let path = std::env::current_dir().unwrap().join("target/release");
        let path = path.display();
        std::env::set_var(
            "PATH",
            format!("{path}:{PATH}", PATH = std::env::var("PATH").unwrap()),
        );
        let path = std::env::current_dir().unwrap().join("target/debug");
        let path = path.display();
        std::env::set_var(
            "PATH",
            format!("{path}:{PATH}", PATH = std::env::var("PATH").unwrap()),
        );
        std::env::set_var(
            "PATH",
            format!("/opt:{PATH}", PATH = std::env::var("PATH").unwrap()),
        );
    }

    // Step 1: lid publisher
    let mut lid = spawn_lid_publisher();
    // wait for lid to be ready
    wait_for_socket("/tmp/run/lid-status.sock")
        .await
        .expect("failed to wait for lid");

    // Step 2: stat controllers
    let mut brightness = spawn_brightness_control();
    let mut volume = spawn_volume_control();
    let mut smartplug = spawn_smartplug_control();

    // Step 5: player
    let mut player = spawn_player();

    loop {
        tokio::select! {
            _ = volume.wait() => {
                tracing::error!("volume process exited, spawning new one");
                volume = spawn_volume_control();
            }
            _ = smartplug.wait() => {
                tracing::error!("smartplug process exited, spawning new one");
                smartplug = spawn_smartplug_control();
            }
            _ = brightness.wait() => {
                tracing::error!("brightness process exited, spawning new one");
                brightness = spawn_brightness_control();
            }
            _ = player.wait() => {
                tracing::error!("player process exited, spawning new one");
                player = spawn_player();
            }
            _ = lid.wait() => {
                tracing::error!("lid process exited !! For safety, exiting !!");
                graceful_quit(vec![smartplug, brightness, player]).await;
                return;
            }
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("received ctrl-c, exiting");
                graceful_quit(vec![smartplug, brightness, player, lid]).await;
                return;
            }
        }
    }
}

async fn graceful_quit(mut children: Vec<ChildProcess>) {
    for child in children.iter() {
        child.child.id().iter().for_each(|id| {
            tracing::info!("killing child {id} from {} with a SIGINT", child.name);
            nix::sys::signal::kill(Pid::from_raw(*id as i32), nix::sys::signal::SIGINT).unwrap();
        });
    }

    tokio::time::sleep(Duration::from_secs(1)).await;
    children.retain_mut(|v| v.is_running());
    if children.is_empty() {
        return;
    }

    tracing::warn!(
        "{} children still running: {:?}",
        children.len(),
        children.iter().map(|v| v.name.clone()).collect::<Vec<_>>()
    );
    for child in children {
        // Try terminating it again for 3 seconds
        child.child.id().iter().for_each(|id| {
            tracing::info!("killing child {id} from {} with a SIGTERM", child.name);
            nix::sys::signal::kill(Pid::from_raw(*id as i32), nix::sys::signal::SIGTERM).unwrap();
        });

        tokio::time::sleep(Duration::from_secs(3)).await;
        // if it's still not dead, kill it with a SIGKILL
        child.child.id().iter().for_each(|id| {
            tracing::info!("killing child {id} from {} with a SIGKILL", child.name);
            nix::sys::signal::kill(Pid::from_raw(*id as i32), nix::sys::signal::SIGKILL).unwrap();
        })
    }
}

fn spawn_volume_control() -> ChildProcess {
    tokio::process::Command::new("volume-control")
        .arg("--volume")
        .arg("60")
        .into_child("volume-control")
}

fn spawn_brightness_control() -> ChildProcess {
    tokio::process::Command::new("sudo")
        .arg("env")
        .arg(format!("PATH={}", std::env::var("PATH").unwrap()))
        .arg("brightness-control")
        .into_child("brightness-control")
}

fn spawn_smartplug_control() -> ChildProcess {
    tokio::process::Command::new("smartplug-control").into_child("smartplug-control")
}

fn spawn_player() -> ChildProcess {
    tokio::process::Command::new("player")
        .arg("--play-state")
        .arg("/srv/play-state.json")
        .arg("--playlist")
        .arg("/srv/playlist.json")
        .into_child("player")
}

fn spawn_lid_publisher() -> ChildProcess {
    tokio::process::Command::new("lid-publisher")
        // .arg("--simulate")
        .into_child("lid-publisher")
}

/// Retrieves the environment variables from the running GNOME graphical session.
/// This function finds the PID of gnome-shell (oldest instance), reads its environment from /proc,
/// and assigns those to the current process's env.
fn steal_graphical_session_env() -> Result<(), String> {
    // Get current UID from /proc/self/status
    let self_status = fs::read_to_string("/proc/self/status").map_err(|e| e.to_string())?;
    let mut current_uid: Option<u32> = None;
    for line in self_status.lines() {
        if line.starts_with("Uid:") {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 2 {
                if let Ok(uid) = parts[1].parse::<u32>() {
                    current_uid = Some(uid);
                    break;
                }
            }
        }
    }
    let current_uid = current_uid.ok_or("Could not find current UID.".to_string())?;

    // Scan /proc for gnome-shell processes owned by the current user
    let mut pids: Vec<u32> = Vec::new();
    let proc_dir = fs::read_dir("/proc").map_err(|e| e.to_string())?;
    for entry in proc_dir {
        let entry = entry.map_err(|e| e.to_string())?;
        let file_name = entry.file_name();
        let pid_str = file_name.to_string_lossy();
        if let Ok(pid) = pid_str.parse::<u32>() {
            let path = entry.path();
            let comm_path = path.join("comm");
            if let Ok(comm) = fs::read_to_string(&comm_path) {
                if comm.trim() == "gnome-shell" {
                    let status_path = path.join("status");
                    if let Ok(status) = fs::read_to_string(&status_path) {
                        let mut proc_uid: Option<u32> = None;
                        for line in status.lines() {
                            if line.starts_with("Uid:") {
                                let parts: Vec<&str> = line.split_whitespace().collect();
                                if parts.len() >= 2 {
                                    if let Ok(uid) = parts[1].parse::<u32>() {
                                        proc_uid = Some(uid);
                                        break;
                                    }
                                }
                            }
                        }
                        if let Some(uid) = proc_uid {
                            if uid == current_uid {
                                pids.push(pid);
                            }
                        }
                    }
                }
            }
        }
    }

    if pids.is_empty() {
        return Err(
            "Could not find gnome-shell PID. Ensure a GNOME session is running.".to_string(),
        );
    }

    // Select the oldest (smallest) PID
    let pid = *pids.iter().min().unwrap();

    // Read and parse /proc/<pid>/environ
    let environ_path = format!("/proc/{}/environ", pid);
    let environ_bytes = fs::read(&environ_path).map_err(|e| e.to_string())?;

    for slice in environ_bytes.split(|&b| b == 0) {
        if !slice.is_empty() {
            let s = String::from_utf8_lossy(slice).to_string();
            if let Some((key, value)) = s.split_once('=') {
                unsafe {
                    env::set_var(key, value);
                }
            }
        }
    }

    Ok(())
}
