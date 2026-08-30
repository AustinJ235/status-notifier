# status-notifier

A [StatusNotifierHost](https://www.freedesktop.org/wiki/Specifications/StatusNotifierItem/)
library for building system trays and similar applications.

Registers `StatusNotifierHost` and `StatusNotifierWatcher` (if not already present) on the session
bus.

## Features

- Tracks all `StatusNotifierItem` and keeps their properties up-to-date.
- Retrieves each item's `dbusmenu` and keeps it up-to-date.
- Forwards user interaction to `StatusNotifierItem` and `dbusmenu` interfaces.
- Supports being used in blocking or poll/epoll-style event loops.
- Built-in `StatusNotifierWatcher` fallback when there isn't one present.

## Examples

- [`blocking`](examples/blocking.rs): minimal blocking event loop.
- [`polling`](examples/polling.rs): epoll-style event loop using the
  [`polling`](https://crates.io/crates/polling) crate.

## Requirements

- A D-Bus session bus (any conventional Linux desktop session).
- `libdbus-1`, linked via the [`dbus`](https://crates.io/crates/dbus) crate.
