#![feature(nonpoison_mutex)]
#![feature(sync_nonpoison)]
use std::{path::PathBuf, sync::Arc, time::Duration};

use clap::Parser;
use tokio::sync::Mutex;
use tracing::info;

mod api;
mod playlist;
mod watch_later;

#[derive(clap::Parser, Debug)]
struct Args {
    /// File to save the playback state for resuming later
    #[clap(short, long)]
    pub play_state: PathBuf,

    /// File to read the playlist from
    #[clap(short = 'l', long)]
    pub playlist: PathBuf,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_level(true)
        .init();
    let args = Args::parse();
    tracing::info!("args: {args:?}");

    let playlist = playlist::Playlist::load_from_file(&args.playlist)
        .await
        .expect("failed to read the playlist file");
    let first_file = playlist.items[0].clone();

    println!("first file: {}", first_file.display());

    let mut player = api::MpvPlayer::new(&first_file.to_string_lossy().to_string().as_str())
        .await
        .expect("failed to init player");

    let mut watch_later = watch_later::load_from_file(&args.play_state).await.unwrap();

    player.restore_state(&watch_later).await.unwrap();

    let player = Arc::new(Mutex::new(player));

    {
        let player = player.clone();
        tokio::spawn(async move {
            let mut lid_status = lid_subscriber::AsyncLidSubscriber::new()
                .await
                .expect("failed to subscribe to lid status");
            let mut play_state = true;
            loop {
                let mut interval = tokio::time::interval(Duration::from_millis(7250));
                interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                tokio::select! {
                    Some(state) = lid_status.next() => {
                        play_state = state.lid_open;
                        player.lock().await.set_paused(!play_state).await.unwrap();
                    }
                    _ = interval.tick() => {
                        player.lock().await.set_paused(!play_state).await.unwrap();
                    }
                }
            }
        });
    }

    {
        let player = player.clone();
        let play_state = args.play_state.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_millis(1000)).await;
                let old_state = watch_later;
                watch_later = player.lock().await.save_state().await.unwrap();
                if old_state != watch_later {
                    tokio::fs::write(&play_state, serde_json::to_string(&watch_later).unwrap())
                        .await
                        .unwrap();
                }
            }
        });
    }

    {
        let player = player.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_millis(5000)).await;
                let duration = player.lock().await.get_file_duration().await.unwrap();
                let position = player.lock().await.get_playback_time().await.unwrap();
                info!("duration: {duration}, position: {position}");

                // If the position is within 5 seconds of duration,
                // load the next video in the playlist
                if (duration - position).abs() < 5. {
                    let current_file = player.lock().await.get_path().await.unwrap();
                    let next_file = playlist.next_file(&current_file.into());
                    info!("loading next file: {next_file:?}");
                    player
                        .lock()
                        .await
                        .loadfile(next_file.to_string_lossy().to_string().as_str())
                        .await
                        .unwrap();
                }
            }
        });
    }

    {
        let player = player.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_millis(2125)).await;
                if !player.lock().await.is_running() {
                    tracing::error!("player died, exiting");
                    std::process::exit(1);
                }
            }
        });
    }

    tokio::signal::ctrl_c()
        .await
        .expect("failed to listen to ctrl-c");

    let _ = player.lock().await.send_quit();
}
