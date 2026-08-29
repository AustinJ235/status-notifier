# status-notifier

A [`StatusNotifierItem`](https://www.freedesktop.org/wiki/Specifications/StatusNotifierItem/)
host library for Rust, for building system trays and similar applications.

`Host` connects to the D-Bus session bus, registers a `StatusNotifierHost`, and keeps track of
every registered `StatusNotifierItem` along with its `dbusmenu` menu. If no `StatusNotifierWatcher`
is running on the bus, a built-in one is registered and used, so a tray built with this library
works standalone.

## Features

- Tracks all registered items and their properties: title, status, icons (names and raw
  pixmaps), tooltip, and more.
- Retrieves and keeps each item's menu tree up to date, including labels, icons, separators,
  and checkbox/radio toggle state.
- Delivers changes as typed events (`Added`, `Removed`, `UpdatedTitle`, `UpdatedMenu`, ...)
  through a callback.
- Forwards user interaction to items: activate, secondary activate, context menu, scroll, and
  menu `clicked`/`hovered`/`opened`/`closed` events.
- Built-in `StatusNotifierWatcher` fallback when none is present on the bus.
- Blocking, polling, or fd-based operation: `Host` implements `AsFd`/`AsRawFd`, so it can be
  registered with `poll`/`epoll`-style event loops.

## Examples

- [`basic`](examples/basic.rs) — minimal blocking event loop.
- [`polling`](examples/polling.rs) — integrates `Host` into an epoll-style event loop using the
  [`polling`](https://crates.io/crates/polling) crate, including handling of pending writes.

## Requirements

- A D-Bus session bus (any conventional Linux desktop session).
- `libdbus-1`, linked via the [`dbus`](https://crates.io/crates/dbus) crate.
