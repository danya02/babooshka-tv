use std::time::Duration;

use clap::Parser;

#[derive(clap::Parser)]
struct Args {
    /// The volume to set when the lid is open
    #[clap(short, long, default_value = "100")]
    volume: u8,
}

fn main() {
    let args = Args::parse();
    let lid_stream =
        lid_subscriber::LidSubscriber::new().expect("failed to connect to lid status socket");

    for event in lid_stream {
        let new_volume = if event.lid_open { args.volume } else { 0 };
        println!(
            "New state at {}, setting volume to {}",
            event.changed_at, new_volume
        );
        for i in 0..3 {
            println!("write {}/3", i + 1);
            let mut process = std::process::Command::new("amixer")
                .arg("set")
                .arg("Master")
                .arg(format!("{new_volume}%"))
                .spawn()
                .expect("failed to run amixer");
            process.wait().expect("failed to wait for amixer");
            std::thread::sleep(Duration::from_millis(250));
        }
    }
}
