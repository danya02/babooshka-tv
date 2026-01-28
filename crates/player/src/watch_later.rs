use std::{io::ErrorKind, path::PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WatchLaterState {
    pub path: PathBuf,
    pub time: f64,
}

impl WatchLaterState {
    pub fn new(path: PathBuf) -> Self {
        Self { path, time: 0.0 }
    }
}

pub async fn load_from_file(play_state: &PathBuf) -> Result<WatchLaterState, std::io::Error> {
    let watch_later_text = match tokio::fs::read_to_string(&play_state).await {
        Ok(s) => s,
        Err(why) => match why.kind() {
            ErrorKind::NotFound => String::new(),
            _ => panic!("failed to read play state: {why}"),
        },
    };

    let watch_later_text = watch_later_text.trim();

    let watch_later = if watch_later_text.is_empty() {
        WatchLaterState::new(
            "/home/danya/Videos/yt-downloads/In Search Of A Flat Earth [JTfhYyTuT44].webm".into(),
        )
    } else {
        match serde_json::from_str(watch_later_text) {
            Ok(s) => s,
            Err(why) => {
                eprintln!(
                    "failed to deserialize watch later state: {why}. Resetting to known good state"
                );
                WatchLaterState::new(
                    "/home/danya/Videos/yt-downloads/In Search Of A Flat Earth [JTfhYyTuT44].webm"
                        .into(),
                )
            }
        }
    };
    Ok(watch_later)
}
