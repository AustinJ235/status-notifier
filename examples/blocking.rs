use status_notifier::Host;

fn main() {
    let mut host = Host::new().unwrap();

    loop {
        host.process_events(
            |name, item, event| {
                println!("Event: {}({}) {:?}", name, item.id, event);
            },
            |error| {
                eprintln!("Error: {error:?}");
            },
            None,
        )
        .unwrap();
    }
}
