use std::{
    io::{BufRead, BufReader},
    os::unix::net::UnixStream,
};

use api_types::LidState;

pub struct LidSubscriber {
    connection: BufReader<UnixStream>,
}

impl LidSubscriber {
    pub fn new() -> Result<Self, std::io::Error> {
        Ok(Self {
            connection: BufReader::new(UnixStream::connect("/tmp/run/lid-status.sock")?),
        })
    }
}

impl Iterator for LidSubscriber {
    type Item = LidState;
    fn next(&mut self) -> Option<Self::Item> {
        let mut buf = String::new();
        self.connection.read_line(&mut buf).ok()?;
        serde_json::from_str(&buf).ok()
    }
}

#[cfg(feature = "tokio")]
pub struct AsyncLidSubscriber {
    connection: tokio::io::BufReader<tokio::net::UnixStream>,
}

#[cfg(feature = "tokio")]
impl AsyncLidSubscriber {
    pub async fn new() -> Result<Self, std::io::Error> {
        Ok(Self {
            connection: tokio::io::BufReader::new(
                tokio::net::UnixStream::connect("/tmp/run/lid-status.sock").await?,
            ),
        })
    }
}

#[cfg(feature = "tokio")]
impl AsyncLidSubscriber {
    pub async fn next(&mut self) -> Option<LidState> {
        use tokio::io::AsyncBufReadExt;
        let mut buf = String::new();
        self.connection.read_line(&mut buf).await.ok()?;
        serde_json::from_str(&buf).ok()
    }
}
