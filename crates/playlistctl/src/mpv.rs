//! A long-lived mpv window driven over its JSON IPC socket.
//!
//! Preview used to spawn a fresh mpv per keypress, which took long enough to
//! break the rhythm of scrubbing for a dialogue scene. One instance stays up
//! instead, and its playback position and the TUI cursor are the same value
//! seen from two ends: seeking here moves the window, seeking in the window
//! moves the cursor here.
//!
//! Deliberately not reusing `player::MpvPlayer`: that one is built around the
//! appliance's needs (a supervised child that must outlive failures) and pulls
//! in nightly features this tool does not otherwise need.

use std::io::{ErrorKind, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

/// How long after issuing a seek to ignore mpv's reported position.
///
/// mpv keeps reporting the old position until the seek lands. Without this the
/// next poll would snap the cursor back and a held arrow key would fight the
/// window it is driving.
const SEEK_SETTLE: Duration = Duration::from_millis(500);

pub struct Mpv {
    child: Child,
    sock: UnixStream,
    /// Partial line left over from the last non-blocking read.
    pending: String,
    /// The file currently loaded, so re-previewing the same one does not reload.
    pub loaded: Option<String>,
    /// Last position mpv reported, in seconds.
    pub time_pos: f64,
    pub paused: bool,
    seeked_at: Option<Instant>,
}

impl Mpv {
    /// Start mpv idle with a window up, and connect to its IPC socket.
    pub fn spawn(sock_path: &str) -> Result<Self, String> {
        // A stale socket from a previous run would be connected to successfully
        // and then never answer.
        let _ = std::fs::remove_file(sock_path);
        if let Some(dir) = Path::new(sock_path).parent() {
            let _ = std::fs::create_dir_all(dir);
        }

        let child = Command::new("mpv")
            .arg(format!("--input-ipc-server={sock_path}"))
            .arg("--idle=yes")
            .arg("--force-window=yes")
            // Match the calibrated listening conditions: unity gain, so what is
            // heard here is what the Pi plays at the same system volume.
            .arg("--volume=100")
            .arg("--no-terminal")
            .arg("--keep-open=yes")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("failed to start mpv: {e}"))?;

        // mpv creates the socket a moment after exec.
        let mut sock = None;
        for _ in 0..100 {
            if let Ok(s) = UnixStream::connect(sock_path) {
                sock = Some(s);
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let sock = sock.ok_or_else(|| format!("mpv did not create {sock_path}"))?;
        sock.set_nonblocking(true).map_err(|e| format!("mpv socket: {e}"))?;

        let mut mpv = Self {
            child,
            sock,
            pending: String::new(),
            loaded: None,
            time_pos: 0.0,
            paused: true,
            seeked_at: None,
        };
        // Observing beats polling: mpv pushes a property-change event on every
        // update, including seeks made in the window itself, which is what
        // makes the cursor follow the user rather than only lead them.
        mpv.send(&json!({"command": ["observe_property", 1, "time-pos"]}))?;
        mpv.send(&json!({"command": ["observe_property", 2, "pause"]}))?;
        Ok(mpv)
    }

    fn send(&mut self, msg: &Value) -> Result<(), String> {
        let mut line = msg.to_string();
        line.push('\n');
        self.sock.write_all(line.as_bytes()).map_err(|e| format!("mpv socket: {e}"))
    }

    pub fn load(&mut self, url: &str, path_key: &str) -> Result<(), String> {
        self.send(&json!({"command": ["loadfile", url]}))?;
        self.loaded = Some(path_key.to_string());
        Ok(())
    }

    pub fn seek(&mut self, secs: f64) -> Result<(), String> {
        self.seeked_at = Some(Instant::now());
        self.time_pos = secs;
        self.send(&json!({"command": ["seek", secs, "absolute"]}))
    }

    pub fn set_paused(&mut self, paused: bool) -> Result<(), String> {
        self.send(&json!({"command": ["set_property", "pause", paused]}))
    }

    /// Loop playback over the marked segment, or clear the loop when `None`.
    ///
    /// Comparing two takes by ear needs repetition, and mpv's A-B loop gives it
    /// for free once the marks are mirrored into the player.
    pub fn set_loop(&mut self, span: Option<(f64, f64)>) -> Result<(), String> {
        match span {
            Some((a, b)) => {
                self.send(&json!({"command": ["set_property", "ab-loop-a", a]}))?;
                self.send(&json!({"command": ["set_property", "ab-loop-b", b]}))
            }
            None => {
                self.send(&json!({"command": ["set_property", "ab-loop-a", "no"]}))?;
                self.send(&json!({"command": ["set_property", "ab-loop-b", "no"]}))
            }
        }
    }

    /// Drain pending events. Returns true if the reported position moved.
    pub fn poll(&mut self) -> bool {
        let mut buf = [0u8; 8192];
        loop {
            match self.sock.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => self.pending.push_str(&String::from_utf8_lossy(&buf[..n])),
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(_) => break,
            }
        }

        let mut moved = false;
        // Keep any trailing fragment: a read can split a JSON line in half.
        let complete = match self.pending.rfind('\n') {
            Some(i) => self.pending.drain(..=i).collect::<String>(),
            None => return false,
        };
        for line in complete.lines() {
            let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
            if v["event"] != json!("property-change") {
                continue;
            }
            match v["name"].as_str() {
                Some("time-pos") => {
                    let settling =
                        self.seeked_at.is_some_and(|t| t.elapsed() < SEEK_SETTLE);
                    if let Some(t) = v["data"].as_f64()
                        && !settling
                    {
                        self.seeked_at = None;
                        if (t - self.time_pos).abs() > 0.05 {
                            self.time_pos = t;
                            moved = true;
                        }
                    }
                }
                Some("pause") => {
                    if let Some(p) = v["data"].as_bool() {
                        self.paused = p;
                    }
                }
                _ => {}
            }
        }
        moved
    }
}

impl Drop for Mpv {
    fn drop(&mut self) {
        // The window is ours; leaving it behind on quit would strand a process
        // holding an SFTP connection open.
        let _ = self.send(&json!({"command": ["quit"]}));
        let _ = self.child.wait();
    }
}
