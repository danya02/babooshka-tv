use std::env;
use std::fs;
use std::io;
use std::path::Path;

/// Sets up the environment variables so that the current process can run wayland programs.
pub fn setup_wayland_env() -> io::Result<()> {
    // Get the current user's UID
    let uid = unsafe { libc::getuid() };

    // Set XDG_RUNTIME_DIR (standard location)
    let runtime_dir = format!("/run/user/{}", uid);
    unsafe {
        env::set_var("XDG_RUNTIME_DIR", &runtime_dir);
    }

    // Find Wayland sockets in the runtime dir
    let path = Path::new(&runtime_dir);
    let mut sockets: Vec<String> = Vec::new();
    if path.exists() && path.is_dir() {
        for entry in fs::read_dir(path)? {
            let entry = entry?;
            if let Some(name) = entry.file_name().to_str() {
                if name.starts_with("wayland-") {
                    sockets.push(name.to_string());
                }
            }
        }
    }

    // Select the first socket or fallback to "wayland-0"
    let wayland_display = if !sockets.is_empty() {
        sockets[0].clone()
    } else {
        "wayland-0".to_string()
    };
    unsafe {
        env::set_var("WAYLAND_DISPLAY", wayland_display);
        env::set_var(
            "DBUS_SESSION_BUS_ADDRESS",
            format!("unix:path={}/bus", runtime_dir),
        );

        env::set_var("GDK_BACKEND", "wayland");
        env::set_var("QT_QPA_PLATFORM", "wayland");

        env::set_var("SDL_VIDEODRIVER", "wayland");
    }

    // Find X11 sockets and set DISPLAY
    let x11_dir = format!("{}/X11-unix", runtime_dir);
    let mut x11_sockets: Vec<u32> = Vec::new();
    if Path::new(&x11_dir).exists() && Path::new(&x11_dir).is_dir() {
        for entry in fs::read_dir(&x11_dir)? {
            let entry = entry?;
            if let Some(name) = entry.file_name().to_str() {
                if name.starts_with("X") {
                    if let Ok(num) = name[1..].parse::<u32>() {
                        x11_sockets.push(num);
                    }
                }
            }
        }
    }
    let display_num = if !x11_sockets.is_empty() {
        x11_sockets.sort();
        x11_sockets[0]
    } else {
        0
    };
    unsafe {
        env::set_var("DISPLAY", format!(":{}", display_num));
    }

    let xauthority_path = format!("{}/Xauthority", runtime_dir);
    if Path::new(&xauthority_path).exists() {
        unsafe {
            env::set_var("XAUTHORITY", xauthority_path);
        }
    }

    Ok(())
}
