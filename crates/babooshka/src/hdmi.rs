use std::{path::PathBuf, time::Duration};

/// Abstraction over "how do we know the TV is ready to display?"
///
/// Implementations:
/// - [`DrmPoll`]: polls a `/sys/class/drm/.../status` file for "connected"
/// - [`FixedTimeout`]: simply waits a fixed duration
/// - [`CecReady`]: watches CEC bus for `ROUTING_CHANGE` from TV, with timeout fallback
///
/// Swap implementations via the `--hdmi-detect` CLI flag.
#[async_trait::async_trait]
pub trait HdmiReady: Send + Sync {
    /// Resolves when the TV is considered ready to display.
    async fn wait_until_ready(&self);
}

// ---------------------------------------------------------------------------
// DRM sysfs polling
// ---------------------------------------------------------------------------

pub struct DrmPoll {
    /// Path to the DRM connector status file, e.g.
    /// `/sys/class/drm/card1-HDMI-A-1/status`
    pub path: PathBuf,
    pub poll_interval: Duration,
}

#[async_trait::async_trait]
impl HdmiReady for DrmPoll {
    async fn wait_until_ready(&self) {
        loop {
            match tokio::fs::read_to_string(&self.path).await {
                Ok(content) if content.trim() == "connected" => {
                    tracing::info!("HDMI connected ({})", self.path.display());
                    return;
                }
                Ok(content) => {
                    tracing::debug!(
                        "HDMI not yet connected (status: {:?}), polling again",
                        content.trim()
                    );
                }
                Err(e) => {
                    tracing::warn!("Could not read DRM status file {}: {e}", self.path.display());
                }
            }
            tokio::time::sleep(self.poll_interval).await;
        }
    }
}

// ---------------------------------------------------------------------------
// CEC ROUTING_CHANGE detection
// ---------------------------------------------------------------------------

/// Watches the CEC bus for the HDMI hotplug cycle that Samsung TVs perform
/// while booting: the physical address briefly goes to f.f.f.f (disconnect)
/// then returns to 4.0.0.0 (reconnect).  Waits 3s after reconnect for the
/// display to settle, then proceeds.  Falls back to `timeout` if the cycle
/// never completes.
pub struct CecReady {
    /// Path to the CEC device, e.g. `/dev/cec0`
    pub device: PathBuf,
    /// Give up and proceed after this long even if no ROUTING_CHANGE arrives.
    pub timeout: Duration,
}

#[async_trait::async_trait]
impl HdmiReady for CecReady {
    async fn wait_until_ready(&self) {
        tracing::info!(
            "Waiting for CEC ROUTING_CHANGE from TV (timeout {}s)",
            self.timeout.as_secs()
        );

        let device = self.device.clone();
        let result = tokio::time::timeout(self.timeout, async move {
            // Run `cec-ctl --monitor` as root and scan its output line by line.
            let mut child = tokio::process::Command::new("sudo")
                .args([
                    "cec-ctl",
                    "-d",
                    device.to_str().unwrap_or("/dev/cec0"),
                    "--monitor",
                ])
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::null())
                .kill_on_drop(true)
                .spawn();

            let mut child = match child {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!("Failed to spawn cec-ctl monitor: {e}");
                    return;
                }
            };

            let stdout = child.stdout.take().unwrap();
            let mut lines = tokio::io::BufReader::new(stdout).lines();

            use tokio::io::AsyncBufReadExt;
            // Track whether we've seen a disconnect first (PA: f.f.f.f),
            // then wait for the reconnect (PA: 4.0.0.0). This is the
            // HDMI hotplug cycle that Samsung TVs do while booting.
            let mut saw_disconnect = false;
            while let Ok(Some(line)) = lines.next_line().await {
                let trimmed = line.trim();
                if !trimmed.is_empty() {
                    tracing::debug!("cec-ctl: {trimmed}");
                }
                if line.contains("PA: f.f.f.f") {
                    tracing::debug!("HDMI disconnected — waiting for reconnect");
                    saw_disconnect = true;
                } else if saw_disconnect && line.contains("PA: 4.0.0.0") {
                    tracing::info!("HDMI reconnected after TV boot — waiting 3s for display to settle");
                    tokio::time::sleep(Duration::from_secs(3)).await;
                    tracing::info!("TV is ready");
                    return;
                }
            }
        })
        .await;

        match result {
            Ok(()) => {}
            Err(_) => {
                tracing::warn!(
                    "CEC ROUTING_CHANGE not received within {}s, proceeding anyway",
                    self.timeout.as_secs()
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Fixed timeout
// ---------------------------------------------------------------------------

pub struct FixedTimeout {
    pub duration: Duration,
}

#[async_trait::async_trait]
impl HdmiReady for FixedTimeout {
    async fn wait_until_ready(&self) {
        tracing::info!(
            "Waiting fixed timeout of {}s for TV to boot",
            self.duration.as_secs_f32()
        );
        tokio::time::sleep(self.duration).await;
    }
}
