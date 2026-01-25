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
