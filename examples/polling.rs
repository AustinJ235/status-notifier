use std::os::fd::{AsFd, AsRawFd};
use std::time::Duration;

use polling::{PollMode, Poller};
use status_notifier::Host;

fn main() {
    let mut host = Host::new().unwrap();
    let poller = Poller::new().unwrap();

    unsafe { poller.add_with_mode(host.as_raw_fd(), polling::Event::all(0), PollMode::Level) }
        .unwrap();

    let mut events = polling::Events::new();
    let mut watching_write = true;

    loop {
        poller.wait(&mut events, None).unwrap();

        for event in events.iter() {
            match event.key {
                0 => {
                    let wants_write = host
                        .process_events(
                            |name, item, event| {
                                println!("Event: {}({}) {:?}", name, item.id, event);
                            },
                            |error| {
                                eprintln!("Error: {error:?}");
                            },
                            Some(Duration::ZERO),
                        )
                        .unwrap();

                    if watching_write != wants_write {
                        watching_write = wants_write;

                        poller
                            .modify_with_mode(
                                host.as_fd(),
                                if wants_write {
                                    polling::Event::all(0)
                                } else {
                                    polling::Event::readable(0)
                                },
                                PollMode::Level,
                            )
                            .unwrap();
                    }
                },
                _ => unreachable!(),
            }
        }

        events.clear();
    }
}
