//! Software control socket — allows overriding the switch state in software.
//!
//! Listens on a Unix socket for newline-terminated commands:
//!   `on\n`     — acts as if the switch went HIGH (active)
//!   `off\n`    — acts as if the switch went LOW (inactive)
//!   `skip\n`   — advance to the next playlist item immediately
//!   `rewind\n` — seek the current file back to the beginning
//!
//! Example:
//!   echo on     | nc -U /tmp/run/babooshka-control.sock
//!   echo off    | nc -U /tmp/run/babooshka-control.sock
//!   echo skip   | nc -U /tmp/run/babooshka-control.sock
//!   echo rewind | nc -U /tmp/run/babooshka-control.sock

use std::{path::Path, sync::Arc};

use tokio::{
    io::{AsyncBufReadExt, BufReader},
    net::UnixListener,
    sync::{Notify, watch},
};

/// Spawn a task that listens on `socket_path` and forwards commands to the
/// appropriate senders.  The task runs until the process exits; errors are
/// logged and the socket continues accepting connections.
pub fn spawn(
    socket_path: impl AsRef<Path> + Send + 'static,
    tx: watch::Sender<bool>,
    skip_notify: Arc<Notify>,
    rewind_notify: Arc<Notify>,
) {
    tokio::spawn(async move {
        let path = socket_path.as_ref();
        // Remove stale socket from a previous run.
        let _ = tokio::fs::remove_file(path).await;
        tokio::fs::create_dir_all(path.parent().unwrap_or(Path::new("/tmp")))
            .await
            .ok();

        let listener = match UnixListener::bind(path) {
            Ok(l) => {
                tracing::info!("Control socket listening on {}", path.display());
                l
            }
            Err(e) => {
                tracing::warn!("Could not bind control socket {}: {e}", path.display());
                return;
            }
        };

        loop {
            let stream = match listener.accept().await {
                Ok((stream, _)) => stream,
                Err(e) => {
                    tracing::warn!("Control socket accept error: {e}");
                    continue;
                }
            };

            let mut lines = BufReader::new(stream).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                match line.trim() {
                    "on" => {
                        tracing::info!("Control socket: switch override → ON");
                        let _ = tx.send(true);
                    }
                    "off" => {
                        tracing::info!("Control socket: switch override → OFF");
                        let _ = tx.send(false);
                    }
                    "skip" => {
                        tracing::info!("Control socket: skip requested");
                        skip_notify.notify_one();
                    }
                    "rewind" => {
                        tracing::info!("Control socket: rewind requested");
                        rewind_notify.notify_one();
                    }
                    other => {
                        tracing::warn!("Control socket: unknown command {:?}", other);
                    }
                }
            }
        }
    });
}
