//! All file and process access goes through SSH to the media host.
//!
//! The videos live on an NFS export (`10.22.0.60:/srv`) that is only exported to
//! `10.22.0.0/24`. This machine reaches the host over ZeroTier from a different
//! subnet, so neither a kernel mount nor a userspace NFS client can be used —
//! both are refused by the same server-side export ACL. SSH is the one channel
//! that works, and it is also the fast one: running ffmpeg on the media host
//! reads from local disk (~190x realtime) instead of dragging whole files across
//! the VPN (~7.5x realtime).

use std::io::Write;
use std::process::{Command, Stdio};

/// Quote a string for safe interpolation into a remote `sh -c` command line.
///
/// Movie paths contain spaces and Cyrillic; single-quoting is the only form that
/// needs no other escaping, with `'` itself spliced in as `'\''`.
pub fn shq(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

#[derive(Debug, Clone)]
pub struct Remote {
    /// `user@host` as accepted by ssh(1); resolved through the user's ssh config.
    pub host: String,
}

impl Remote {
    pub fn new(host: impl Into<String>) -> Self {
        Self { host: host.into() }
    }

    /// Run a shell command on the media host, returning stdout.
    ///
    /// stderr is folded into the error message rather than the output, because
    /// ffmpeg writes its analysis to stderr and callers that want it say so
    /// explicitly via [`Remote::run_capture_stderr`].
    pub fn run(&self, cmd: &str) -> Result<String, String> {
        let out = Command::new("ssh")
            .arg("-o")
            .arg("BatchMode=yes")
            .arg(&self.host)
            .arg(cmd)
            .output()
            .map_err(|e| format!("failed to spawn ssh: {e}"))?;
        if !out.status.success() {
            return Err(format!(
                "remote command failed ({}): {}",
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// Run a shell command and return stdout and stderr merged.
    ///
    /// ffmpeg reports `ebur128` measurements on stderr, so measurement passes
    /// need both streams.
    pub fn run_capture_stderr(&self, cmd: &str) -> Result<String, String> {
        let out = Command::new("ssh")
            .arg("-o")
            .arg("BatchMode=yes")
            .arg(&self.host)
            .arg(cmd)
            .output()
            .map_err(|e| format!("failed to spawn ssh: {e}"))?;
        let mut s = String::from_utf8_lossy(&out.stdout).into_owned();
        s.push_str(&String::from_utf8_lossy(&out.stderr));
        if !out.status.success() {
            return Err(format!("remote command failed ({}): {}", out.status, s.trim()));
        }
        Ok(s)
    }

    pub fn read_file(&self, path: &str) -> Result<Option<String>, String> {
        // `|| true` plus a sentinel keeps a missing file from looking like an
        // ssh transport failure, which callers would otherwise have to guess at.
        let out = self.run(&format!(
            "if [ -f {p} ]; then cat {p}; else echo __PLAYLISTCTL_MISSING__; fi",
            p = shq(path)
        ))?;
        if out.trim_end() == "__PLAYLISTCTL_MISSING__" {
            return Ok(None);
        }
        Ok(Some(out))
    }

    /// Write a file atomically: stream to a temp file in the same directory,
    /// then rename over the target.
    ///
    /// babooshka reads `playlist.json` and `gain_db.json` while running, so a
    /// partially written file must never be observable. `mv` within one
    /// filesystem is atomic, so a reader sees either the old or the new file.
    pub fn write_file_atomic(&self, path: &str, contents: &str) -> Result<(), String> {
        let tmp = format!("{path}.playlistctl.tmp");
        let cmd = format!(
            "cat > {t} && mv -f {t} {p}",
            t = shq(&tmp),
            p = shq(path)
        );
        let mut child = Command::new("ssh")
            .arg("-o")
            .arg("BatchMode=yes")
            .arg(&self.host)
            .arg(&cmd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("failed to spawn ssh: {e}"))?;
        child
            .stdin
            .take()
            .expect("stdin was piped")
            .write_all(contents.as_bytes())
            .map_err(|e| format!("failed to stream file to remote: {e}"))?;
        let out = child
            .wait_with_output()
            .map_err(|e| format!("ssh did not exit cleanly: {e}"))?;
        if !out.status.success() {
            return Err(format!(
                "atomic write of {path} failed ({}): {}",
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        Ok(())
    }

    /// List every file under `root`, recursively and sorted, as absolute paths.
    ///
    /// Deliberately unfiltered: filtering by extension here is what let a DVD
    /// rip sit on disk invisibly. The UI classifies what it cannot play and
    /// lets it be dismissed explicitly, so nothing disappears silently.
    pub fn scan_files(&self, root: &str) -> Result<Vec<String>, String> {
        let out = self.run(&format!("find {r} -type f -printf '%p\\n' | sort", r = shq(root)))?;
        Ok(out.lines().map(str::to_owned).filter(|l| !l.is_empty()).collect())
    }

    /// Build an `sftp://` URL for local playback of a remote file.
    ///
    /// mpv and ffmpeg are both built against libssh here, so they can open these
    /// directly — this is how segment preview works without any mount.
    pub fn sftp_url(&self, path: &str) -> String {
        format!("sftp://{}{}", self.host, path)
    }
}
