use async_trait::async_trait;

use crate::smartplug::SmartPlug;

/// Abstraction over whatever mechanism controls TV power.
#[async_trait]
pub trait TvPower: Send + Sync {
    async fn turn_on(&self) -> Result<(), String>;
    async fn turn_off(&self) -> Result<(), String>;
}

// ---------------------------------------------------------------------------
// Smart plug implementation
// ---------------------------------------------------------------------------

#[async_trait]
impl TvPower for SmartPlug {
    async fn turn_on(&self) -> Result<(), String> {
        self.set_state(true).await.map_err(|e| e.to_string())
    }

    async fn turn_off(&self) -> Result<(), String> {
        self.set_state(false).await.map_err(|e| e.to_string())
    }
}

// ---------------------------------------------------------------------------
// Dry-run (debugging): logs actions without touching the TV
// ---------------------------------------------------------------------------

pub struct DryRunTv;

#[async_trait]
impl TvPower for DryRunTv {
    async fn turn_on(&self) -> Result<(), String> {
        tracing::info!("[dry-run] TV would be turned ON");
        Ok(())
    }

    async fn turn_off(&self) -> Result<(), String> {
        tracing::info!("[dry-run] TV would be turned OFF");
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// CEC implementation (delegates to cec-client binary)
// ---------------------------------------------------------------------------

pub struct CecTv;

#[async_trait]
impl TvPower for CecTv {
    async fn turn_on(&self) -> Result<(), String> {
        // "on 0" powers on the TV (address 0 = TV)
        run_cec("on 0").await
    }

    async fn turn_off(&self) -> Result<(), String> {
        // "standby 0" puts the TV into standby
        run_cec("standby 0").await
    }
}

async fn run_cec(cmd: &str) -> Result<(), String> {
    tracing::info!("Sending CEC command: {cmd}");
    // cec-client with -s reads one command and exits — pipe the command via sh
    let output = tokio::process::Command::new("sh")
        .arg("-c")
        .arg(format!("echo '{cmd}' | cec-client -s -d 1"))
        .output()
        .await
        .map_err(|e| format!("Failed to run cec-client: {e}"))?;

    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "cec-client failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        ))
    }
}

// ---------------------------------------------------------------------------
// Combined: try primary, log error and try fallback
// ---------------------------------------------------------------------------

pub struct CombinedTv {
    pub primary: Box<dyn TvPower>,
    pub fallback: Box<dyn TvPower>,
}

#[async_trait]
impl TvPower for CombinedTv {
    async fn turn_on(&self) -> Result<(), String> {
        if let Err(e) = self.primary.turn_on().await {
            tracing::warn!("Primary TV turn_on failed: {e}, trying fallback");
            self.fallback.turn_on().await
        } else {
            Ok(())
        }
    }

    async fn turn_off(&self) -> Result<(), String> {
        if let Err(e) = self.primary.turn_off().await {
            tracing::warn!("Primary TV turn_off failed: {e}, trying fallback");
            self.fallback.turn_off().await
        } else {
            Ok(())
        }
    }
}
