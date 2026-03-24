/// A swaynag-based debug overlay.
///
/// Calling [`Overlay::show`] kills the previous swaynag process (if any) and
/// spawns a new one with the given message.  When [`Overlay`] is dropped the
/// current swaynag process is killed automatically.
pub struct Overlay {
    child: Option<tokio::process::Child>,
}

impl Overlay {
    pub fn new() -> Self {
        Self { child: None }
    }

    /// Replace the currently displayed message (if any) with `message`.
    pub async fn show(&mut self, message: &str) {
        self.kill().await;
        match tokio::process::Command::new("swaynag")
            .args(["-m", message])
            .kill_on_drop(true)
            .spawn()
        {
            Ok(child) => self.child = Some(child),
            Err(e) => tracing::warn!("Failed to spawn swaynag: {e}"),
        }
    }

    /// Close the overlay immediately.
    pub async fn hide(&mut self) {
        self.kill().await;
    }

    async fn kill(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill().await;
            let _ = child.wait().await;
        }
    }
}

impl Drop for Overlay {
    fn drop(&mut self) {
        // Best-effort synchronous kill on drop (no async here).
        // `kill_on_drop(true)` on the Child handles the actual signal.
        let _ = self.child.take();
    }
}
