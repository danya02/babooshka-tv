fn main() {
    let subscriber = lid_subscriber::LidSubscriber::new().expect("failed to create subscriber");
    for state in subscriber {
        println!("Received new state: {:?}", state);
    }
}
