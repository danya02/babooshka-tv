#![feature(never_type)]
use std::{
    io::Write,
    os::unix::net::{UnixListener, UnixStream},
    path::PathBuf,
    sync::{Arc, Condvar, Mutex},
    time::Duration,
};

use api_types::LidState;
use clap::Parser;

#[derive(clap::Parser)]
struct Args {
    /// Path to the file indicating lid status.
    /// If not provided, defaults to /proc/acpi/button/lid/<first directory>/state
    #[clap(short, long)]
    lid_file: Option<PathBuf>,

    /// Path to the status socket.
    /// If not provided, defaults to /tmp/run/lid-status.sock
    #[clap(short, long)]
    status_socket: Option<PathBuf>,
}

fn main() {
    let args = Args::parse();
    let lid_file = args.lid_file.unwrap_or_else(autodetect_lid_file);
    println!("using lid file: {}", lid_file.display());
    let is_open = is_open(&lid_file);
    println!("lid is open: {}", is_open);

    let status_socket = args
        .status_socket
        .unwrap_or_else(|| "/tmp/run/lid-status.sock".into());
    println!("using status socket: {}", status_socket.display());
    // ensure directory
    std::fs::create_dir_all(PathBuf::from(
        status_socket
            .parent()
            .expect("failed to get parent of status socket"),
    ))
    .expect("failed to create directory where the socket should live");

    // Check if anyone else is listening on the socket
    match UnixStream::connect(&status_socket) {
        Ok(_) => {
            eprintln!(
                "Someone else is already listening on the socket at {}",
                status_socket.display()
            );
            return;
        }
        Err(_) => {
            // Stale file or doesn't exist; safe to remove
            let _ = std::fs::remove_file(&status_socket);
        }
    }

    let shared = Arc::new((
        Mutex::new(LidState {
            lid_open: is_open,
            changed_at: api_types::now(),
        }),
        Condvar::new(),
    ));

    {
        let shared = shared.clone();
        let lid_file = lid_file.clone();
        std::thread::spawn(move || check_lid_loop(&lid_file, shared));
    }

    let socket = UnixListener::bind(status_socket).expect("failed to listen to socket");

    for stream in socket.incoming() {
        let Ok(stream) = stream else {
            println!(
                "failed to accept connection because: {:?}, shutting down",
                stream
            );
            return;
        };

        let shared = shared.clone();
        std::thread::spawn(move || {
            let Err(err) = connection_handler(stream, shared);
            if err.kind() != std::io::ErrorKind::BrokenPipe {
                println!("error in connection handler: {:?}", err);
            }
        });
    }
}

fn check_lid_loop(lid_file: &std::path::Path, shared: Arc<(Mutex<LidState>, Condvar)>) -> ! {
    loop {
        std::thread::sleep(Duration::from_millis(500));
        let is_open = is_open(lid_file);
        let mut guard = shared.0.lock().expect("failed to lock state");
        if guard.lid_open != is_open {
            guard.lid_open = is_open;
            guard.changed_at = api_types::now();
            shared.1.notify_all();
        }
    }
}

fn connection_handler(
    mut conn: UnixStream,
    shared: Arc<(Mutex<LidState>, Condvar)>,
) -> Result<!, std::io::Error> {
    // Send initial state
    let state_str = {
        let guard = shared.0.lock().expect("failed to lock state");
        serde_json::to_string(&*guard).expect("failed to serialize state")
    };
    conn.write_all(state_str.as_bytes())?;
    conn.write(b"\n")?;
    conn.flush()?;

    // Loop to wait for changes and send updates
    loop {
        // Wait for notification (re-acquires lock on wake)
        let guard = shared
            .1
            .wait(shared.0.lock().expect("failed to lock state"))
            .expect("failed to wait on condvar");

        let state_str = serde_json::to_string(&*guard).expect("failed to serialize state");
        conn.write_all(state_str.as_bytes())?;
        conn.write(b"\n")?;
        conn.flush()?;
    }
}

fn autodetect_lid_file() -> std::path::PathBuf {
    let dirs =
        std::fs::read_dir("/proc/acpi/button/lid").expect("cannot read dir /proc/acpi/button/lid");
    for i in dirs {
        let i = i.expect("cannot read dir entry");
        let kind = i.file_type().expect("cannot read file type");
        if kind.is_dir() {
            let path = i.path().join("state");
            if std::fs::exists(&path).expect("cannot check existence of lid file") {
                return path;
            }
        }
    }

    panic!("cannot autodetect lid file; perhaps not running on laptop?")
}

fn is_open(lid_file: &std::path::Path) -> bool {
    let s = std::fs::read_to_string(lid_file).expect("cannot read lid file");
    s.trim_ascii_end().ends_with("open")
}
