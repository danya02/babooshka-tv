use std::time::Duration;

fn main() {
    let mut backlight = None;
    // find the directory at /sys/class/backlight that contains brightness

    let backlight_dir = std::fs::read_dir("/sys/class/backlight").expect("failed to read dir");
    for i in backlight_dir {
        let i = i.expect("failed to read dir entry");
        let path = i.path().join("brightness");
        if std::fs::exists(&path).expect("failed to check existence of brightness file") {
            backlight = Some(path);
            break;
        } else {
            continue;
        }
    }

    let Some(backlight) = backlight else {
        panic!("no backlight file found in /sys/class/backlight; perhaps not running on laptop?");
    };

    // Try to read and then write to the file
    let existing_brightness =
        std::fs::read_to_string(&backlight).expect("failed to read brightness");
    std::fs::write(&backlight, existing_brightness)
        .expect("failed to write brightness -- consider running this program as root");
    let max_brightness = std::fs::read_to_string(&backlight.with_file_name("max_brightness"))
        .expect("failed to read brightness");

    let lid_stream =
        lid_subscriber::LidSubscriber::new().expect("failed to connect to lid status socket");

    for event in lid_stream {
        if event.lid_open {
            println!(
                "lid is open at {}; setting brightness to 0",
                event.changed_at
            );
            for i in 0..3 {
                println!("write {}/3", i + 1);
                std::fs::write(&backlight, "0")
                    .expect("failed to write brightness -- consider running this program as root");
                std::thread::sleep(Duration::from_millis(250));
            }
        } else {
            println!(
                "lid is closed at {}; setting brightness to {max_brightness}",
                event.changed_at
            );
            for i in 0..3 {
                println!("write {}/3", i + 1);
                std::fs::write(&backlight, &max_brightness)
                    .expect("failed to write brightness -- consider running this program as root");
                std::thread::sleep(Duration::from_millis(250));
            }
        }
    }
}
