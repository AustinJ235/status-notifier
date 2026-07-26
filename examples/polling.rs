use std::os::fd::AsRawFd;
use std::time::Duration;

use polling::{PollMode, Poller};
use status_notifier::Host;

fn main() {
    let mut host = Host::new().unwrap();
    let poller = Poller::new().unwrap();

    unsafe { poller.add_with_mode(host.as_raw_fd(), polling::Event::all(0), PollMode::Edge) }
        .unwrap();

    let mut events = polling::Events::new();

    loop {
        poller.wait(&mut events, None).unwrap();

        for event in events.iter() {
            match event.key {
                0 => {
                    host.process_events(
                        |name, item, event| {
                            println!("Event: {}({}) {:?}", name, item.id, event);
                        },
                        |error| {
                            eprintln!("Error: {error:?}");
                        },
                        Some(Duration::ZERO),
                    )
                    .unwrap();
                },
                _ => unreachable!(),
            }
        }
    }
}
