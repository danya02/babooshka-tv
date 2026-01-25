fn main() {
    println!("Clearing environment variables for current process");
    std::env::vars().for_each(|(k, _)| unsafe { std::env::remove_var(&k) });

    println!("Setting up wayland environment");
    utils::setup_wayland_env().expect("failed to setup wayland environment");

    println!("Current variables:");
    std::env::vars().for_each(|(k, v)| println!("{}={}", k, v));

    println!("Spawning app");
    let status = std::process::Command::new("glxgears")
        .spawn()
        .expect("failed to spawn")
        .wait()
        .expect("failed to wait for child");
    println!("Child exited with status: {}", status);
}
