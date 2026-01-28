use serde_json::json;

fn main() {
    let lid_subscriber = lid_subscriber::LidSubscriber::new().expect("failed to create subscriber");
    for state in lid_subscriber {
        println!("Received new state: {state:?}");
        set_switch_state(state.lid_open);
    }
}

fn set_switch_state(state: bool) {
    let url = format!(
        "http://10.22.0.50:8123/api/services/switch/{}",
        if state { "turn_on" } else { "turn_off" }
    );
    println!("Sending request to {url}");
    let token = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJpc3MiOiI5YjczYTZiNWZiY2U0OTNjYmE4ZGE4ZTJlNzBiYTlkYyIsImlhdCI6MTc2ODI1NjA0OCwiZXhwIjoyMDgzNjE2MDQ4fQ.ux7beB3jProrHhOWrngcOGgrYR6-68yLoFq026_b6a0";
    let client = reqwest::blocking::Client::new();
    client
        .post(url)
        .header("Authorization", format!("Bearer {token}"))
        .json(&json!({"entity_id": "switch.bare_tv_switch"}))
        .send()
        .expect("failed to send request")
        .error_for_status()
        .expect("error response to request");
}
