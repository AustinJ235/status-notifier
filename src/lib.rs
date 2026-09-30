//! A [StatusNotifierHost](https://www.freedesktop.org/wiki/Specifications/StatusNotifierItem/)
//! for building system trays and similar applications.
//!
//! [`Host`] connects to the session bus, registers a `StatusNotifierHost` (and a built-in
//! `StatusNotifierWatcher` if not already present), and keeps track of every registered item along
//! with its menu. Changes are delivered as [`Event`]s through [`Host::process_events`], and items
//! are interacted with through the `item_*` and `menu_*` methods.
//!
//! See the `blocking` and `polling` examples for usage.

use std::collections::{HashMap, HashSet};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, RawFd};
use std::process;
use std::time::{Duration, Instant};

use dbus::MessageType;
use dbus::arg::{AppendAll, RefArg, Variant};
use dbus::channel::{BusType, Channel};
use dbus::message::Message;

const DBUS_REQUEST_NAME_REPLY_PRIMARY_OWNER: u32 = 1;
const DBUS_NAME_FLAG_DO_NOT_QUEUE: u32 = 4;

/// The tooltip of an [`Item`].
#[derive(Default, Debug, Clone, PartialEq)]
pub struct ToolTip {
    /// The freedesktop-compliant name of the tooltip's icon.
    pub icon_name: String,
    /// The tooltip's icon as raw image data, possibly in multiple sizes.
    pub icon_pixmap: Vec<Pixmap>,
    /// The title of the tooltip.
    pub title: String,
    /// Descriptive text for the tooltip. May contain a subset of HTML markup.
    pub description: String,
}

/// An icon image provided by an item.
#[derive(Default, Clone, PartialEq)]
pub struct Pixmap {
    /// The width and height of the image in pixels.
    pub dimensions: [i32; 2],
    /// The raw image data in ARGB32 format, in network (big-endian) byte order.
    pub image_data: Vec<u8>,
}

impl std::fmt::Debug for Pixmap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pixmap")
            .field("dimensions", &self.dimensions)
            .finish_non_exhaustive()
    }
}

/// A `StatusNotifierItem` and its menu.
///
/// Most fields correspond directly to the properties of the `org.kde.StatusNotifierItem`
/// interface. Fields an item doesn't provide are left with their default (empty) values.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Item {
    /// The bus name the item is reachable at, used to identify it in the [`Host`] methods.
    pub bus_name: String,
    /// The category of the item, e.g. `ApplicationStatus`, `Communications`, `SystemServices`
    /// or `Hardware`.
    pub category: String,
    /// A name unique to the application, e.g. its name.
    pub id: String,
    /// A human readable name of the item.
    pub title: String,
    /// The status of the item: `Passive`, `Active` or `NeedsAttention`.
    pub status: String,
    /// The windowing-system dependent id of the application's window, or `0` if there is none.
    pub window_id: u64,
    /// An additional path to search for the item's icons.
    pub icon_theme_path: String,
    /// The freedesktop-compliant name of the main icon.
    pub icon_name: String,
    /// The main icon as raw image data, possibly in multiple sizes.
    pub icon_pixmap: Vec<Pixmap>,
    /// The name of an icon overlayed on top of the main icon.
    pub overlay_icon_name: String,
    /// The overlay icon as raw image data, possibly in multiple sizes.
    pub overlay_icon_pixmap: Vec<Pixmap>,
    /// The name of the icon used when the status is `NeedsAttention`.
    pub attention_icon_name: String,
    /// The attention icon as raw image data, possibly in multiple sizes.
    pub attention_icon_pixmap: Vec<Pixmap>,
    /// The name of an animation used when the status is `NeedsAttention`.
    pub attention_movie_name: String,
    /// The tooltip of the item.
    pub tool_tip: ToolTip,
    /// Whether the item only supports a context menu; activation should open the menu instead.
    pub item_is_menu: bool,
    /// The root of the item's menu tree, [`MenuNode::Empty`] if the item has no menu.
    pub menu: MenuNode,
}

/// PNG image data for the icon of a [`MenuNode::Item`].
#[derive(Clone, PartialEq)]
pub struct IconData(pub Vec<u8>);

impl std::fmt::Debug for IconData {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.0.is_empty() {
            write!(f, "IconData(empty)")
        } else {
            write!(f, "IconData({} bytes)", self.0.len())
        }
    }
}

/// A node in the menu tree of an [`Item`].
///
/// Nodes are identified by their `id`, which is used with the `menu_*` methods of [`Host`].
#[derive(Debug, Clone, Default, PartialEq)]
pub enum MenuNode {
    /// The item doesn't have a menu, or the menu hasn't been retrieved yet.
    #[default]
    Empty,
    /// A (sub)menu containing other nodes.
    Menu {
        /// The id of the node.
        id: i32,
        /// The displayed text of the menu.
        label: String,
        /// Whether the menu can be opened.
        enabled: bool,
        /// Whether the menu should be shown.
        visible: bool,
        /// The nodes contained in the menu.
        children: Vec<Self>,
    },
    /// An entry of a menu.
    Item {
        /// The id of the node.
        id: i32,
        /// The displayed text of the entry.
        label: String,
        /// Whether the entry can be clicked.
        enabled: bool,
        /// Whether the entry should be shown.
        visible: bool,
        /// The freedesktop-compliant name of the entry's icon.
        icon_name: String,
        /// PNG data of the entry's icon.
        icon_data: IconData,
        /// How the entry can be toggled.
        toggle_type: ToggleType,
        /// The current toggle state of the entry.
        toggle_state: ToggleState,
    },
    /// A separator between entries.
    Separator {
        /// The id of the node.
        id: i32,
        /// Whether the separator should be shown.
        visible: bool,
    },
}

/// How a [`MenuNode::Item`] can be toggled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToggleType {
    /// The entry can't be toggled.
    None,
    /// The entry can be toggled independently, like a checkbox.
    Checkmark,
    /// The entry is part of a group where only one entry is toggled at a time, like a radio
    /// button.
    Radio,
}

/// The toggle state of a [`MenuNode::Item`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToggleState {
    /// The entry is toggled on.
    On,
    /// The entry is toggled off.
    Off,
    /// The state is indeterminate, or the entry can't be toggled.
    Unknown,
}

struct IntlItem {
    menu_path: String,
    menu_revision: u32,
    item_op: Option<Item>,
}

/// A change of an [`Item`], delivered through the `event_fn` of [`Host::process_events`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    /// The item was registered and its properties retrieved.
    ///
    /// **Note**: The item's menu will not be fetched at the time of this event, but all other
    /// item properties *should be* present.
    Added,
    /// The item was removed.
    Removed,
    /// [`Item::title`] was updated.
    UpdatedTitle,
    /// [`Item::status`] was updated.
    UpdatedStatus,
    /// [`Item::icon_theme_path`] was updated.
    UpdatedIconThemePath,
    /// [`Item::icon_name`] was updated.
    UpdatedIconName,
    /// [`Item::icon_pixmap`] was updated.
    UpdatedIconPixmap,
    /// [`Item::attention_icon_name`] was updated.
    UpdatedAttentionIconName,
    /// [`Item::attention_icon_pixmap`] was updated.
    UpdatedAttentionIconPixmap,
    /// [`Item::overlay_icon_name`] was updated.
    UpdatedOverlayIconName,
    /// [`Item::overlay_icon_pixmap`] was updated.
    UpdatedOverlayIconPixmap,
    /// [`Item::tool_tip`] was updated.
    UpdatedToolTip,
    /// [`Item::menu`] was updated.
    UpdatedMenu,
}

struct Watcher {
    hosts: HashSet<String>,
    items: HashSet<String>,
}

enum ReplyTo {
    RegisterHost,
    NotifierItems,
    ItemGetAll {
        item_name: String,
    },
    ItemGet {
        item_name: String,
        property: &'static str,
    },
    MenuGetLayout {
        item_name: String,
        parent_id: i32,
    },
    ItemMethod {
        item_name: String,
    },
    MenuEvent {
        item_name: String,
    },
}

/// The main object representing a `StatusNotifierHost`.
///
/// A `Host` connects to the session bus and keeps track of every registered `StatusNotifierItem`
/// along with its menu. Item state is kept up to date, and changes are delivered as [`Event`]s,
/// by calling [`Host::process_events`].
///
/// `Host` implements [`AsFd`], so instead of polling, the underlying connection can be registered
/// with `poll`/`epoll`-style event loops to wait for activity (see the `polling` example).
///
/// **Note**: The built-in `StatusNotifierWatcher` will only be used if there isn't a
/// `StatusNotifierWatcher` already registered.
pub struct Host {
    channel: Channel,
    bus_name: String,
    items: HashMap<String, IntlItem>,
    intl_watcher_op: Option<Watcher>,
    pending_replies: HashMap<u32, (Instant, ReplyTo)>,
}

/// The error type used throughout the library.
///
/// [`operation`](Error::operation) tracks where the error originated and [`kind`](Error::kind)
/// what went wrong.
#[derive(Debug)]
pub struct Error {
    /// The operation the error originated from.
    pub operation: Operation,
    /// What went wrong.
    pub kind: ErrorKind,
    /// The bus name of the remote peer involved, when known.
    pub peer: Option<String>,
}

impl Error {
    fn new(operation: Operation, kind: ErrorKind) -> Self {
        Self {
            operation,
            kind,
            peer: None,
        }
    }

    fn with_peer<P>(operation: Operation, kind: ErrorKind, peer: P) -> Self
    where
        P: Into<String>,
    {
        Self {
            operation,
            kind,
            peer: Some(peer.into()),
        }
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "error while {}", self.operation)?;

        if let Some(peer) = self.peer.as_deref() {
            write!(f, " ({peer})")?;
        }

        write!(f, ": {}", self.kind)
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match &self.kind {
            ErrorKind::Dbus(e) => Some(e),
            _ => None,
        }
    }
}

/// The operation an [`Error`] originated from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Operation {
    /// Connecting to or communicating with the session bus.
    Connect,
    /// Requesting a bus name.
    RequestName,
    /// Adding a signal match rule.
    AddMatch,
    /// Registering this host with the `StatusNotifierWatcher`.
    RegisterHost,
    /// Retrieving the registered items from the `StatusNotifierWatcher`.
    GetItems,
    /// Retrieving the properties of an item.
    ItemGet,
    /// Calling a method of an item.
    ItemMethod,
    /// Processing a signal sent by an item.
    ItemSignal,
    /// Sending a menu event to an item.
    MenuEvent,
    /// Retrieving the menu layout of an item.
    MenuGetLayout,
    /// Handling a registration request sent to the internal watcher.
    WatcherRegister,
    /// Handling a property request sent to the internal watcher.
    WatcherGet,
    /// Handling or emitting a signal of the internal watcher.
    WatcherSignal,
    /// Processing a reply to a previously sent message.
    Reply,
}

impl std::fmt::Display for Operation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Connect => "communicating with the session bus",
            Self::RequestName => "requesting a bus name",
            Self::AddMatch => "adding a signal match rule",
            Self::RegisterHost => "registering the host",
            Self::GetItems => "retrieving the registered items",
            Self::ItemGet => "retrieving item properties",
            Self::ItemMethod => "calling an item method",
            Self::ItemSignal => "processing an item signal",
            Self::MenuEvent => "sending a menu event",
            Self::MenuGetLayout => "retrieving a menu layout",
            Self::WatcherRegister => "handling a watcher registration",
            Self::WatcherGet => "handling a watcher property request",
            Self::WatcherSignal => "handling a watcher signal",
            Self::Reply => "processing a reply",
        })
    }
}

/// What went wrong for an [`Error`].
#[derive(Debug)]
pub enum ErrorKind {
    /// A message could not be sent.
    SendFailed,
    /// A message contained invalid or unexpected data.
    InvalidData,
    /// A reply wasn't received in time.
    Timeout,
    /// The remote peer replied with an error.
    Dbus(dbus::Error),
    /// The connection to the session bus was lost.
    Disconnected,
    /// A `StatusNotifierHost` is already registered on the session bus.
    HostAlreadyExists,
    /// The external `StatusNotifierWatcher` is no longer available.
    WatcherLost,
    /// The given item bus name isn't known.
    UnknownItem,
    /// The given menu node doesn't exist in the item's menu.
    UnknownMenuNode,
    /// The given menu node is disabled.
    MenuNodeDisabled,
}

impl std::fmt::Display for ErrorKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SendFailed => f.write_str("the message could not be sent"),
            Self::InvalidData => f.write_str("invalid or unexpected data was received"),
            Self::Timeout => f.write_str("the reply wasn't received in time"),
            Self::Dbus(e) => write!(f, "{e}"),
            Self::Disconnected => f.write_str("the connection was lost"),
            Self::HostAlreadyExists => f.write_str("a StatusNotifierHost is already registered"),
            Self::WatcherLost => f.write_str("the StatusNotifierWatcher is no longer available"),
            Self::UnknownItem => f.write_str("the item doesn't exist"),
            Self::UnknownMenuNode => f.write_str("the menu node doesn't exist"),
            Self::MenuNodeDisabled => f.write_str("the menu node is disabled"),
        }
    }
}

/// The direction of a [`Host::item_scroll`] request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollOrientation {
    /// Scroll horizontally.
    Horizontal,
    /// Scroll vertically.
    Vertical,
}

impl Host {
    /// Connect to the session bus and register a `StatusNotifierHost`.
    ///
    /// If no `StatusNotifierWatcher` is present on the bus, the built-in one is registered and
    /// used (see [`Host::using_intl_watcher`]).
    ///
    /// # Errors
    ///
    /// Returns an error if connecting to the session bus fails, if another `StatusNotifierHost`
    /// is already registered ([`ErrorKind::HostAlreadyExists`]), or if any of the setup messages
    /// can't be sent.
    pub fn new() -> Result<Self, Error> {
        let mut channel = Channel::get_private(BusType::Session)
            .map_err(|e| Error::new(Operation::Connect, ErrorKind::Dbus(e)))?;

        let bus_msg = channel
            .pop_message()
            .ok_or_else(|| Error::new(Operation::Connect, ErrorKind::InvalidData))?;

        let bus_name: String = bus_msg
            .read1()
            .map_err(|_| Error::new(Operation::Connect, ErrorKind::InvalidData))?;
        let host_name = format!("org.kde.StatusNotifierHost-{}", process::id());

        let req_host_msg = channel
            .send_with_reply_and_block(
                Message::call_with_args(
                    "org.freedesktop.DBus",
                    "/org/freedesktop/DBus",
                    "org.freedesktop.DBus",
                    "RequestName",
                    (&host_name, DBUS_NAME_FLAG_DO_NOT_QUEUE),
                ),
                Duration::from_secs(3),
            )
            .map_err(|e| Error::new(Operation::RequestName, ErrorKind::Dbus(e)))?;

        let req_host_reply: u32 = req_host_msg
            .read1()
            .map_err(|_| Error::new(Operation::RequestName, ErrorKind::InvalidData))?;

        if req_host_reply != DBUS_REQUEST_NAME_REPLY_PRIMARY_OWNER {
            return Err(Error::new(
                Operation::RequestName,
                ErrorKind::HostAlreadyExists,
            ));
        }

        let req_watcher_msg = channel
            .send_with_reply_and_block(
                Message::call_with_args(
                    "org.freedesktop.DBus",
                    "/org/freedesktop/DBus",
                    "org.freedesktop.DBus",
                    "RequestName",
                    ("org.kde.StatusNotifierWatcher", DBUS_NAME_FLAG_DO_NOT_QUEUE),
                ),
                Duration::from_secs(3),
            )
            .map_err(|e| Error::new(Operation::RequestName, ErrorKind::Dbus(e)))?;

        let req_watcher_reply: u32 = req_watcher_msg
            .read1()
            .map_err(|_| Error::new(Operation::RequestName, ErrorKind::InvalidData))?;

        let use_intl_watcher = req_watcher_reply == DBUS_REQUEST_NAME_REPLY_PRIMARY_OWNER;

        for match_rule in [
            "type='signal',interface='org.kde.StatusNotifierWatcher'",
            "type='signal',interface='org.kde.StatusNotifierItem'",
            "type='signal',interface='org.freedesktop.DBus',member='NameOwnerChanged'",
            "type='signal',interface='com.canonical.dbusmenu'",
        ] {
            channel
                .send_with_reply_and_block(
                    Message::call_with_args(
                        "org.freedesktop.DBus",
                        "/org/freedesktop/DBus",
                        "org.freedesktop.DBus",
                        "AddMatch",
                        (match_rule,),
                    ),
                    Duration::from_secs(3),
                )
                .map_err(|e| Error::new(Operation::AddMatch, ErrorKind::Dbus(e)))?;
        }

        channel.set_watch_enabled(true);

        let mut host = Self {
            channel,
            bus_name,
            items: HashMap::new(),
            intl_watcher_op: None,
            pending_replies: HashMap::new(),
        };

        if use_intl_watcher {
            host.intl_watcher_op = Some(Watcher {
                hosts: HashSet::new(),
                items: HashSet::new(),
            });
        }

        let serial = host
            .channel
            .send(Message::call_with_args(
                "org.kde.StatusNotifierWatcher",
                "/StatusNotifierWatcher",
                "org.kde.StatusNotifierWatcher",
                "RegisterStatusNotifierHost",
                ((&host.bus_name),),
            ))
            .map_err(|()| Error::new(Operation::RegisterHost, ErrorKind::SendFailed))?;

        host.pending_replies
            .insert(serial, (Instant::now(), ReplyTo::RegisterHost));

        Ok(host)
    }

    /// Get an [`Item`] from the item's bus name.
    ///
    /// Returns `None` if the item doesn't exist or its properties haven't been retrieved yet.
    pub fn get_item<N>(&self, item_name: N) -> Option<&Item>
    where
        N: AsRef<str>,
    {
        self.items
            .get(item_name.as_ref())
            .and_then(|intl_item| intl_item.item_op.as_ref())
    }

    /// Get all existing [`Item`]'s.
    pub fn get_all_items(&self) -> impl Iterator<Item = (&String, &Item)> {
        self.items.iter().filter_map(|(item_name, intl_item)| {
            intl_item.item_op.as_ref().map(|item| (item_name, item))
        })
    }

    /// Call the `Activate` method of the `StatusNotifierItem` interface.
    ///
    /// This is the primary action of an item, typically triggered by a left click. `x` and `y`
    /// are the screen coordinates of the triggering input event.
    ///
    /// # Errors
    ///
    /// - [`ErrorKind::UnknownItem`] if `item_name` isn't a known item
    /// - [`ErrorKind::Disconnected`] if the message couldn't be sent
    ///
    /// **Note**: An error reply from the item itself is delivered later through the `error_fn`
    // of [`Host::process_events`].
    pub fn item_activate<N>(&mut self, item_name: N, x: i32, y: i32) -> Result<(), Error>
    where
        N: AsRef<str>,
    {
        self.item_method(item_name.as_ref(), "Activate", (x, y))
    }

    /// Call the `ContextMenu` method of the `StatusNotifierItem` interface.
    ///
    /// This asks the item to show its own context menu at the screen coordinates `x` and `y`,
    /// typically triggered by a right click.
    ///
    /// # Errors
    ///
    /// - [`ErrorKind::UnknownItem`] if `item_name` isn't a known item
    /// - [`ErrorKind::Disconnected`] if the message couldn't be sent
    ///
    /// **Note**: An error reply from the item itself is delivered later through the `error_fn`
    // of [`Host::process_events`].
    pub fn item_context_menu<N>(&mut self, item_name: N, x: i32, y: i32) -> Result<(), Error>
    where
        N: AsRef<str>,
    {
        self.item_method(item_name.as_ref(), "ContextMenu", (x, y))
    }

    /// Call the `SecondaryActivate` method of the `StatusNotifierItem` interface.
    ///
    /// This is the secondary action of an item, typically triggered by a middle click. `x` and
    /// `y` are the screen coordinates of the triggering input event.
    ///
    /// # Errors
    ///
    /// - [`ErrorKind::UnknownItem`] if `item_name` isn't a known item
    /// - [`ErrorKind::Disconnected`] if the message couldn't be sent
    ///
    /// **Note**: An error reply from the item itself is delivered later through the `error_fn`
    // of [`Host::process_events`].
    pub fn item_secondary_activate<N>(&mut self, item_name: N, x: i32, y: i32) -> Result<(), Error>
    where
        N: AsRef<str>,
    {
        self.item_method(item_name.as_ref(), "SecondaryActivate", (x, y))
    }

    /// Call the `Scroll` method of the `StatusNotifierItem` interface.
    ///
    /// This reports scroll wheel input over the item, `delta` being the amount scrolled in the
    /// given [`ScrollOrientation`].
    ///
    /// # Errors
    ///
    /// - [`ErrorKind::UnknownItem`] if `item_name` isn't a known item
    /// - [`ErrorKind::Disconnected`] if the message couldn't be sent
    ///
    /// **Note**: An error reply from the item itself is delivered later through the `error_fn`
    // of [`Host::process_events`].
    pub fn item_scroll<N>(
        &mut self,
        item_name: N,
        delta: i32,
        orientation: ScrollOrientation,
    ) -> Result<(), Error>
    where
        N: AsRef<str>,
    {
        self.item_method(
            item_name.as_ref(),
            "Scroll",
            (
                delta,
                match orientation {
                    ScrollOrientation::Horizontal => "horizontal",
                    ScrollOrientation::Vertical => "vertical",
                },
            ),
        )
    }

    fn item_method<A>(&mut self, item_name: &str, method: &str, args: A) -> Result<(), Error>
    where
        A: AppendAll,
    {
        if self.get_item(item_name).is_none() {
            return Err(Error::with_peer(
                Operation::ItemMethod,
                ErrorKind::UnknownItem,
                item_name,
            ));
        }

        let serial = self
            .channel
            .send(Message::call_with_args(
                item_name,
                "/StatusNotifierItem",
                "org.kde.StatusNotifierItem",
                method,
                args,
            ))
            .map_err(|()| {
                Error::with_peer(Operation::ItemMethod, ErrorKind::Disconnected, item_name)
            })?;

        self.pending_replies.insert(
            serial,
            (
                Instant::now(),
                ReplyTo::ItemMethod {
                    item_name: item_name.to_string(),
                },
            ),
        );

        Ok(())
    }

    /// Send the `clicked` event of the `dbusmenu` interface to a menu node.
    ///
    /// This activates the [`MenuNode`] with `node_id`, e.g. after the user clicked the entry.
    ///
    /// # Errors
    ///
    /// - [`ErrorKind::UnknownItem`] if `item_name` isn't a known item
    /// - [`ErrorKind::UnknownMenuNode`] if `node_id` isn't part of the item's menu
    /// - [`ErrorKind::MenuNodeDisabled`] if the node isn't enabled
    /// - [`ErrorKind::Disconnected`] if the message couldn't be sent
    ///
    /// **Note**: An error reply from the item itself is delivered later through the `error_fn` of
    /// [`Host::process_events`].
    pub fn menu_clicked<N>(&mut self, item_name: N, node_id: i32) -> Result<(), Error>
    where
        N: AsRef<str>,
    {
        self.menu_event(item_name.as_ref(), node_id, "clicked")
    }

    /// Send the `hovered` event of the `dbusmenu` interface to a menu node.
    ///
    /// This tells the item that the pointer moved onto the [`MenuNode`] with `node_id`.
    ///
    /// # Errors
    ///
    /// - [`ErrorKind::UnknownItem`] if `item_name` isn't a known item
    /// - [`ErrorKind::UnknownMenuNode`] if `node_id` isn't part of the item's menu
    /// - [`ErrorKind::MenuNodeDisabled`] if the node isn't enabled
    /// - [`ErrorKind::Disconnected`] if the message couldn't be sent
    ///
    /// **Note**: An error reply from the item itself is delivered later through the `error_fn` of
    /// [`Host::process_events`].
    pub fn menu_hovered<N>(&mut self, item_name: N, node_id: i32) -> Result<(), Error>
    where
        N: AsRef<str>,
    {
        self.menu_event(item_name.as_ref(), node_id, "hovered")
    }

    /// Send the `opened` event of the `dbusmenu` interface to a menu node.
    ///
    /// This tells the item that the (sub)menu with `node_id` was shown to the user.
    ///
    /// # Errors
    ///
    /// # Errors
    ///
    /// - [`ErrorKind::UnknownItem`] if `item_name` isn't a known item
    /// - [`ErrorKind::UnknownMenuNode`] if `node_id` isn't part of the item's menu
    /// - [`ErrorKind::MenuNodeDisabled`] if the node isn't enabled
    /// - [`ErrorKind::Disconnected`] if the message couldn't be sent
    ///
    /// **Note**: An error reply from the item itself is delivered later through the `error_fn` of
    /// [`Host::process_events`].
    pub fn menu_opened<N>(&mut self, item_name: N, node_id: i32) -> Result<(), Error>
    where
        N: AsRef<str>,
    {
        self.menu_event(item_name.as_ref(), node_id, "opened")
    }

    /// Send the `closed` event of the `dbusmenu` interface to a menu node.
    ///
    /// This tells the item that the (sub)menu with `node_id` was closed.
    ///
    /// # Errors
    ///
    /// - [`ErrorKind::UnknownItem`] if `item_name` isn't a known item
    /// - [`ErrorKind::UnknownMenuNode`] if `node_id` isn't part of the item's menu
    /// - [`ErrorKind::MenuNodeDisabled`] if the node isn't enabled
    /// - [`ErrorKind::Disconnected`] if the message couldn't be sent
    ///
    /// **Note**: An error reply from the item itself is delivered later through the `error_fn` of
    /// [`Host::process_events`].
    pub fn menu_closed<N>(&mut self, item_name: N, node_id: i32) -> Result<(), Error>
    where
        N: AsRef<str>,
    {
        self.menu_event(item_name.as_ref(), node_id, "closed")
    }

    fn menu_event(&mut self, item_name: &str, node_id: i32, event: &str) -> Result<(), Error> {
        let Some(intl_item) = self.items.get(item_name) else {
            return Err(Error::with_peer(
                Operation::MenuEvent,
                ErrorKind::UnknownItem,
                item_name,
            ));
        };

        let Some(item) = intl_item.item_op.as_ref() else {
            return Err(Error::with_peer(
                Operation::MenuEvent,
                ErrorKind::UnknownItem,
                item_name,
            ));
        };

        if intl_item.menu_path.is_empty() {
            return Err(Error::with_peer(
                Operation::MenuEvent,
                ErrorKind::UnknownMenuNode,
                item_name,
            ));
        }

        let mut check_nodes = vec![&item.menu];
        let mut node_exists = false;
        let mut node_enabled = false;

        while let Some(node) = check_nodes.pop() {
            match node {
                MenuNode::Empty
                | MenuNode::Separator {
                    ..
                } => continue,
                MenuNode::Menu {
                    id,
                    children,
                    enabled,
                    ..
                } => {
                    if *id == node_id {
                        node_exists = true;
                        node_enabled = *enabled;
                        break;
                    }

                    check_nodes.extend(children.iter())
                },
                MenuNode::Item {
                    id,
                    enabled,
                    ..
                } => {
                    if *id == node_id {
                        node_exists = true;
                        node_enabled = *enabled;
                        break;
                    }
                },
            }
        }

        if !node_exists {
            return Err(Error::with_peer(
                Operation::MenuEvent,
                ErrorKind::UnknownMenuNode,
                item_name,
            ));
        }

        if event == "clicked" && !node_enabled {
            return Err(Error::with_peer(
                Operation::MenuEvent,
                ErrorKind::MenuNodeDisabled,
                item_name,
            ));
        }

        let serial = self
            .channel
            .send(Message::call_with_args(
                item_name,
                &intl_item.menu_path,
                "com.canonical.dbusmenu",
                "Event",
                (node_id, event, Variant(""), 0_u32),
            ))
            .map_err(|()| {
                Error::with_peer(Operation::MenuEvent, ErrorKind::Disconnected, item_name)
            })?;

        self.pending_replies.insert(
            serial,
            (
                Instant::now(),
                ReplyTo::MenuEvent {
                    item_name: item_name.to_string(),
                },
            ),
        );

        Ok(())
    }

    /// Check if the internal `StatusNotifierWatcher` is being used.
    pub fn using_intl_watcher(&self) -> bool {
        self.intl_watcher_op.is_some()
    }

    /// Process pending [`Event`]s and [`Error`]s.
    ///
    /// - `event_fn` callback is used for events of an [`Item`].
    /// - `error_fn` callback is used for [`Error`]s that occur that are non-fatal.
    ///    - The method itself will return an `Err(..)` if the error is fatal.
    /// - `timeout` field is used to configure the blocking behavior.
    ///     - `None` will block until there is an event or an error to be processed.
    ///     - `Some(Duration::ZERO)` will not block (primarily used for polling).
    ///
    /// **Note**: This method will return `Ok(true)` when there are pending writes to the `Fd`.
    /// This *should* be relatively rare, so it can be ignored for initial implementations, but
    /// should be considered when writing more robust implementations.
    pub fn process_events<EventFn, ErrorFn>(
        &mut self,
        mut event_fn: EventFn,
        mut error_fn: ErrorFn,
        timeout: Option<Duration>,
    ) -> Result<bool, Error>
    where
        EventFn: FnMut(String, &Item, Event),
        ErrorFn: FnMut(Error),
    {
        if self.channel.read_write(timeout).is_err() {
            return Err(Error::new(Operation::Connect, ErrorKind::Disconnected));
        }

        struct MsgMatch<'a> {
            ty: MessageType,
            sender: Option<&'a str>,
            destination: Option<&'a str>,
            interface: Option<&'a str>,
            member: Option<&'a str>,
        }

        while let Some(mut message) = self.channel.pop_message() {
            match (MsgMatch {
                ty: message.msg_type(),
                sender: message.sender().as_deref(),
                destination: message.destination().as_deref(),
                interface: message.interface().as_deref(),
                member: message.member().as_deref(),
            }) {
                MsgMatch {
                    ty: MessageType::MethodCall,
                    sender: Some(sender),
                    destination: Some("org.kde.StatusNotifierWatcher"),
                    interface: Some("org.kde.StatusNotifierWatcher"),
                    member: Some("RegisterStatusNotifierHost"),
                } => {
                    let Some(watcher) = self.intl_watcher_op.as_mut() else {
                        let _ = self.channel.send(reply_error(
                            &message,
                            "org.freedesktop.DBus.Error.UnknownObject",
                        ));

                        continue;
                    };

                    let Ok(host_name) = message.read1::<String>() else {
                        error_fn(Error::with_peer(
                            Operation::WatcherRegister,
                            ErrorKind::InvalidData,
                            sender,
                        ));

                        let _ = self.channel.send(reply_error(
                            &message,
                            "org.freedesktop.DBus.Error.InvalidArgs",
                        ));

                        continue;
                    };

                    let is_host_new = watcher.hosts.insert(host_name.clone());

                    if self.channel.send(message.method_return()).is_err() {
                        error_fn(Error::with_peer(
                            Operation::WatcherRegister,
                            ErrorKind::SendFailed,
                            sender,
                        ));
                    }

                    if is_host_new && watcher.hosts.len() == 1 {
                        send_signal(
                            &self.channel,
                            Message::new_signal(
                                "/StatusNotifierWatcher",
                                "org.kde.StatusNotifierWatcher",
                                "StatusNotifierHostRegistered",
                            )
                            .unwrap(),
                            &mut error_fn,
                            Error::new(Operation::WatcherSignal, ErrorKind::SendFailed),
                        );
                    }
                },
                MsgMatch {
                    ty: MessageType::MethodCall,
                    sender: Some(sender),
                    destination: Some("org.kde.StatusNotifierWatcher"),
                    interface: Some("org.kde.StatusNotifierWatcher"),
                    member: Some("RegisterStatusNotifierItem"),
                } => {
                    let Some(watcher) = self.intl_watcher_op.as_mut() else {
                        let _ = self.channel.send(reply_error(
                            &message,
                            "org.freedesktop.DBus.Error.UnknownObject",
                        ));

                        continue;
                    };

                    let Ok(item_name) = message.read1::<String>() else {
                        error_fn(Error::with_peer(
                            Operation::WatcherRegister,
                            ErrorKind::InvalidData,
                            sender,
                        ));

                        let _ = self.channel.send(reply_error(
                            &message,
                            "org.freedesktop.DBus.Error.InvalidArgs",
                        ));

                        continue;
                    };

                    let is_item_new = watcher.items.insert(item_name.clone());

                    if self.channel.send(message.method_return()).is_err() {
                        error_fn(Error::with_peer(
                            Operation::WatcherRegister,
                            ErrorKind::SendFailed,
                            sender,
                        ));
                    }

                    if is_item_new {
                        send_signal(
                            &self.channel,
                            Message::new_signal(
                                "/StatusNotifierWatcher",
                                "org.kde.StatusNotifierWatcher",
                                "StatusNotifierItemRegistered",
                            )
                            .unwrap()
                            .append1(item_name),
                            &mut error_fn,
                            Error::new(Operation::WatcherSignal, ErrorKind::SendFailed),
                        );
                    }
                },
                MsgMatch {
                    ty: MessageType::MethodCall,
                    sender: Some(sender),
                    interface: Some("org.freedesktop.DBus.Properties"),
                    member: Some("GetAll"),
                    ..
                } => {
                    let Ok(interface) = message.read1::<String>() else {
                        error_fn(Error::with_peer(
                            Operation::WatcherGet,
                            ErrorKind::InvalidData,
                            sender,
                        ));

                        let _ = self.channel.send(reply_error(
                            &message,
                            "org.freedesktop.DBus.Error.InvalidArgs",
                        ));

                        continue;
                    };

                    match interface.as_str() {
                        "org.kde.StatusNotifierWatcher" => {
                            let Some(watcher) = self.intl_watcher_op.as_mut() else {
                                let _ = self.channel.send(reply_error(
                                    &message,
                                    "org.freedesktop.DBus.Error.UnknownObject",
                                ));

                                continue;
                            };

                            let host_registered = !watcher.hosts.is_empty();
                            let items: Vec<String> = watcher.items.iter().cloned().collect();
                            let mut dict: HashMap<&str, Variant<Box<dyn RefArg>>> = HashMap::new();
                            dict.insert("RegisteredStatusNotifierItems", Variant(Box::new(items)));

                            dict.insert(
                                "IsStatusNotifierHostRegistered",
                                Variant(Box::new(host_registered)),
                            );

                            dict.insert("ProtocolVersion", Variant(Box::new(0_i32)));

                            if self
                                .channel
                                .send(message.method_return().append1(dict))
                                .is_err()
                            {
                                error_fn(Error::with_peer(
                                    Operation::WatcherGet,
                                    ErrorKind::SendFailed,
                                    sender,
                                ));
                            }
                        },
                        _ => {
                            let _ = self.channel.send(reply_error(
                                &message,
                                "org.freedesktop.DBus.Error.UnknownInterface",
                            ));
                        },
                    }
                },
                MsgMatch {
                    ty: MessageType::MethodCall,
                    sender: Some(sender),
                    interface: Some("org.freedesktop.DBus.Properties"),
                    member: Some("Get"),
                    ..
                } => {
                    let Ok((interface, property)) = message.read2::<String, String>() else {
                        error_fn(Error::with_peer(
                            Operation::WatcherGet,
                            ErrorKind::InvalidData,
                            sender,
                        ));

                        let _ = self.channel.send(reply_error(
                            &message,
                            "org.freedesktop.DBus.Error.InvalidArgs",
                        ));

                        continue;
                    };

                    match interface.as_str() {
                        "org.kde.StatusNotifierWatcher" => {
                            let Some(watcher) = self.intl_watcher_op.as_mut() else {
                                let _ = self.channel.send(reply_error(
                                    &message,
                                    "org.freedesktop.DBus.Error.UnknownObject",
                                ));

                                continue;
                            };

                            let reply = match property.as_str() {
                                "RegisteredStatusNotifierItems" => {
                                    let items = watcher.items.iter().collect::<Vec<_>>();
                                    message.method_return().append1(Variant(items))
                                },
                                "IsStatusNotifierHostRegistered" => {
                                    message
                                        .method_return()
                                        .append1(Variant(!watcher.hosts.is_empty()))
                                },
                                "ProtocolVersion" => {
                                    message.method_return().append1(Variant(0_i32))
                                },
                                _ => {
                                    reply_error(
                                        &message,
                                        "org.freedesktop.DBus.Error.UnknownProperty",
                                    )
                                },
                            };

                            if self.channel.send(reply).is_err() {
                                error_fn(Error::with_peer(
                                    Operation::WatcherGet,
                                    ErrorKind::SendFailed,
                                    sender,
                                ));
                            }
                        },
                        _ => {
                            let _ = self.channel.send(reply_error(
                                &message,
                                "org.freedesktop.DBus.Error.UnknownInterface",
                            ));
                        },
                    }
                },
                MsgMatch {
                    ty: MessageType::Signal,
                    sender: Some(sender),
                    interface: Some("org.kde.StatusNotifierWatcher"),
                    member: Some("StatusNotifierItemRegistered"),
                    ..
                } => {
                    let Ok(item_name) = message.read1::<String>() else {
                        error_fn(Error::with_peer(
                            Operation::WatcherSignal,
                            ErrorKind::InvalidData,
                            sender,
                        ));
                        continue;
                    };

                    if self.items.contains_key(&item_name) {
                        error_fn(Error::with_peer(
                            Operation::WatcherSignal,
                            ErrorKind::InvalidData,
                            sender,
                        ));
                        continue;
                    }

                    self.items.insert(
                        item_name.clone(),
                        IntlItem {
                            menu_path: String::new(),
                            menu_revision: 0,
                            item_op: None,
                        },
                    );

                    match self.channel.send(msg_item_get_all(&item_name)) {
                        Ok(serial) => {
                            self.pending_replies.insert(
                                serial,
                                (
                                    Instant::now(),
                                    ReplyTo::ItemGetAll {
                                        item_name: item_name.clone(),
                                    },
                                ),
                            );
                        },
                        Err(()) => {
                            error_fn(Error::with_peer(
                                Operation::ItemGet,
                                ErrorKind::SendFailed,
                                item_name.clone(),
                            ));
                        },
                    }
                },
                MsgMatch {
                    ty: MessageType::Signal,
                    sender: Some(sender),
                    interface: Some("org.kde.StatusNotifierWatcher"),
                    member: Some("StatusNotifierItemUnregistered"),
                    ..
                } => {
                    let Ok(item_name) = message.read1::<String>() else {
                        error_fn(Error::with_peer(
                            Operation::WatcherSignal,
                            ErrorKind::InvalidData,
                            sender,
                        ));
                        continue;
                    };

                    let Some(item) = self.items.remove(&item_name) else {
                        error_fn(Error::with_peer(
                            Operation::WatcherSignal,
                            ErrorKind::InvalidData,
                            sender,
                        ));
                        continue;
                    };

                    if let Some(item) = item.item_op {
                        event_fn(item_name, &item, Event::Removed);
                    }
                },
                MsgMatch {
                    ty: MessageType::Signal,
                    sender: Some(sender),
                    interface: Some("org.kde.StatusNotifierItem"),
                    member: Some(member),
                    ..
                } => {
                    if !self.items.contains_key(sender) {
                        // Note: signals may be received before the item is registered with the
                        //       watcher. All properies will be fetched when it is registered.
                        continue;
                    }

                    let get_properties = match member {
                        "NewTitle" => vec!["Title"],
                        "NewIcon" => vec!["IconName", "IconPixmap"],
                        "NewAttentionIcon" => vec!["AttentionIconName", "AttentionIconPixmap"],
                        "NewOverlayIcon" => vec!["OverlayIconName", "OverlayIconPixmap"],
                        "NewToolTip" => vec!["ToolTip"],
                        "NewStatus" => {
                            let Ok(status) = message.read1::<String>() else {
                                error_fn(Error::with_peer(
                                    Operation::ItemSignal,
                                    ErrorKind::InvalidData,
                                    sender,
                                ));
                                continue;
                            };

                            let intl_item = self.items.get_mut(sender).expect("unreachable");

                            let Some(item) = intl_item.item_op.as_mut() else {
                                error_fn(Error::with_peer(
                                    Operation::ItemSignal,
                                    ErrorKind::InvalidData,
                                    sender,
                                ));
                                continue;
                            };

                            item.status = status;
                            event_fn(sender.to_string(), item, Event::UpdatedStatus);
                            continue;
                        },
                        "NewIconThemePath" => vec!["IconThemePath"],
                        _ => {
                            error_fn(Error::with_peer(
                                Operation::ItemSignal,
                                ErrorKind::InvalidData,
                                sender,
                            ));
                            continue;
                        },
                    };

                    for property in get_properties {
                        match self.channel.send(msg_item_get_prop(sender, property)) {
                            Ok(serial) => {
                                self.pending_replies.insert(
                                    serial,
                                    (
                                        Instant::now(),
                                        ReplyTo::ItemGet {
                                            item_name: sender.to_string(),
                                            property,
                                        },
                                    ),
                                );
                            },
                            Err(()) => {
                                error_fn(Error::with_peer(
                                    Operation::ItemGet,
                                    ErrorKind::SendFailed,
                                    sender,
                                ));
                            },
                        }
                    }
                },
                MsgMatch {
                    ty: MessageType::Signal,
                    sender: Some(sender),
                    interface: Some("com.canonical.dbusmenu"),
                    member: Some("ItemsPropertiesUpdated"),
                    ..
                } => {
                    type UpdatedProps = Vec<(i32, HashMap<String, Variant<Box<dyn RefArg>>>)>;
                    type RemovedProps = Vec<(i32, Vec<String>)>;

                    let Ok((updated_props, removed_props)) =
                        message.read2::<UpdatedProps, RemovedProps>()
                    else {
                        error_fn(Error::with_peer(
                            Operation::ItemSignal,
                            ErrorKind::InvalidData,
                            sender,
                        ));
                        continue;
                    };

                    let Some(intl_item) = self.items.get_mut(sender) else {
                        continue;
                    };

                    if intl_item.menu_path.is_empty() {
                        continue;
                    }

                    let Some(item) = intl_item.item_op.as_mut() else {
                        continue;
                    };

                    let mut menu_updated = false;

                    for (node_id, node_properties) in updated_props {
                        let Some((node, _depth)) = find_menu_node(&mut item.menu, node_id, 0)
                        else {
                            continue;
                        };

                        for (p_name, p_value_arg) in node_properties {
                            match p_name.as_str() {
                                "visible" => {
                                    let Some(p_value) = p_value_arg.as_i64() else {
                                        continue;
                                    };

                                    match node {
                                        MenuNode::Empty => continue,
                                        MenuNode::Menu {
                                            visible, ..
                                        }
                                        | MenuNode::Item {
                                            visible, ..
                                        }
                                        | MenuNode::Separator {
                                            visible, ..
                                        } => {
                                            *visible = p_value == 1;
                                        },
                                    }
                                },
                                "enabled" => {
                                    let Some(p_value) = p_value_arg.as_i64() else {
                                        continue;
                                    };

                                    match node {
                                        MenuNode::Menu {
                                            enabled, ..
                                        }
                                        | MenuNode::Item {
                                            enabled, ..
                                        } => {
                                            *enabled = p_value == 1;
                                        },
                                        _ => continue,
                                    }
                                },
                                "label" => {
                                    let Some(p_value) = p_value_arg.as_str() else {
                                        continue;
                                    };

                                    match node {
                                        MenuNode::Menu {
                                            label, ..
                                        }
                                        | MenuNode::Item {
                                            label, ..
                                        } => {
                                            *label = p_value.to_string();
                                        },
                                        _ => continue,
                                    }
                                },
                                "icon-name" => {
                                    let Some(p_value) = p_value_arg.as_str() else {
                                        continue;
                                    };

                                    let MenuNode::Item {
                                        icon_name, ..
                                    } = node
                                    else {
                                        continue;
                                    };

                                    *icon_name = p_value.to_string();
                                },
                                "icon-data" => {
                                    let Some(new_icon_data) = parse_icon_data_arg(&p_value_arg)
                                    else {
                                        continue;
                                    };

                                    let MenuNode::Item {
                                        icon_data, ..
                                    } = node
                                    else {
                                        continue;
                                    };

                                    *icon_data = new_icon_data;
                                },
                                "toggle-state" => {
                                    let Some(new_toggle_state) =
                                        parse_toggle_state_arg(&p_value_arg)
                                    else {
                                        continue;
                                    };

                                    let MenuNode::Item {
                                        toggle_state, ..
                                    } = node
                                    else {
                                        continue;
                                    };

                                    *toggle_state = new_toggle_state;
                                },
                                "toggle-type" => {
                                    let Some(new_toggle_type) = parse_toggle_type_arg(&p_value_arg)
                                    else {
                                        continue;
                                    };

                                    let MenuNode::Item {
                                        toggle_type, ..
                                    } = node
                                    else {
                                        continue;
                                    };

                                    *toggle_type = new_toggle_type;
                                },
                                _ => continue,
                            }

                            menu_updated = true;
                        }
                    }

                    for (node_id, removed_props) in removed_props {
                        let Some((node, _depth)) = find_menu_node(&mut item.menu, node_id, 0)
                        else {
                            continue;
                        };

                        for p_name in removed_props {
                            match p_name.as_str() {
                                "label" => {
                                    match node {
                                        MenuNode::Menu {
                                            label, ..
                                        }
                                        | MenuNode::Item {
                                            label, ..
                                        } => {
                                            *label = String::new();
                                        },
                                        _ => continue,
                                    }
                                },
                                "enabled" => {
                                    match node {
                                        MenuNode::Menu {
                                            enabled, ..
                                        }
                                        | MenuNode::Item {
                                            enabled, ..
                                        } => {
                                            *enabled = true;
                                        },
                                        _ => continue,
                                    }
                                },
                                "visible" => {
                                    match node {
                                        MenuNode::Empty => continue,
                                        MenuNode::Menu {
                                            visible, ..
                                        }
                                        | MenuNode::Item {
                                            visible, ..
                                        }
                                        | MenuNode::Separator {
                                            visible, ..
                                        } => {
                                            *visible = true;
                                        },
                                    }
                                },
                                "icon-name" => {
                                    let MenuNode::Item {
                                        icon_name, ..
                                    } = node
                                    else {
                                        continue;
                                    };

                                    icon_name.clear();
                                },
                                "icon-data" => {
                                    let MenuNode::Item {
                                        icon_data, ..
                                    } = node
                                    else {
                                        continue;
                                    };

                                    icon_data.0.clear();
                                },
                                "toggle-type" => {
                                    let MenuNode::Item {
                                        toggle_type, ..
                                    } = node
                                    else {
                                        continue;
                                    };

                                    *toggle_type = ToggleType::None;
                                },
                                "toggle-state" => {
                                    let MenuNode::Item {
                                        toggle_state, ..
                                    } = node
                                    else {
                                        continue;
                                    };

                                    *toggle_state = ToggleState::Unknown;
                                },
                                _ => continue,
                            }

                            menu_updated = true;
                        }
                    }

                    if menu_updated {
                        event_fn(sender.to_string(), item, Event::UpdatedMenu);
                    }
                },
                MsgMatch {
                    ty: MessageType::Signal,
                    sender: Some(sender),
                    interface: Some("com.canonical.dbusmenu"),
                    member: Some("LayoutUpdated"),
                    ..
                } => {
                    let Ok((_revision, parent_id)) = message.read2::<u32, i32>() else {
                        error_fn(Error::with_peer(
                            Operation::ItemSignal,
                            ErrorKind::InvalidData,
                            sender,
                        ));
                        continue;
                    };

                    let Some(intl_item) = self.items.get_mut(sender) else {
                        continue;
                    };

                    if intl_item.menu_path.is_empty() {
                        continue;
                    }

                    let args: (i32, i32, &[&str]) = (parent_id, -1, &[]);

                    match self.channel.send(Message::call_with_args(
                        sender,
                        &intl_item.menu_path,
                        "com.canonical.dbusmenu",
                        "GetLayout",
                        args,
                    )) {
                        Ok(serial) => {
                            self.pending_replies.insert(
                                serial,
                                (
                                    Instant::now(),
                                    ReplyTo::MenuGetLayout {
                                        item_name: sender.to_string(),
                                        parent_id,
                                    },
                                ),
                            );
                        },
                        Err(()) => {
                            error_fn(Error::with_peer(
                                Operation::MenuGetLayout,
                                ErrorKind::SendFailed,
                                sender,
                            ))
                        },
                    }
                },
                MsgMatch {
                    ty: MessageType::Signal,
                    interface: Some("org.freedesktop.DBus"),
                    member: Some("NameOwnerChanged"),
                    ..
                } => {
                    let Ok((name, _old_owner, new_owner)) =
                        message.read3::<String, String, String>()
                    else {
                        continue;
                    };

                    if !new_owner.is_empty() {
                        if name == "org.kde.StatusNotifierWatcher" && self.intl_watcher_op.is_none()
                        {
                            match self
                                .channel
                                .send(Message::call_with_args(
                                    "org.kde.StatusNotifierWatcher",
                                    "/StatusNotifierWatcher",
                                    "org.kde.StatusNotifierWatcher",
                                    "RegisterStatusNotifierHost",
                                    ((&self.bus_name),),
                                ))
                                .map_err(|()| {
                                    Error::new(Operation::RegisterHost, ErrorKind::SendFailed)
                                }) {
                                Ok(serial) => {
                                    self.pending_replies
                                        .insert(serial, (Instant::now(), ReplyTo::RegisterHost));
                                },
                                Err(e) => error_fn(e),
                            }
                        }

                        continue;
                    }

                    let Some(watcher) = self.intl_watcher_op.as_mut() else {
                        if name == "org.kde.StatusNotifierWatcher" {
                            for (item_name, intl_item) in self.items.drain() {
                                if let Some(item) = intl_item.item_op {
                                    event_fn(item_name, &item, Event::Removed);
                                }
                            }

                            error_fn(Error::new(Operation::RegisterHost, ErrorKind::WatcherLost));
                        }

                        continue;
                    };

                    if watcher.hosts.remove(&name) && watcher.hosts.is_empty() {
                        send_signal(
                            &self.channel,
                            Message::new_signal(
                                "/StatusNotifierWatcher",
                                "org.kde.StatusNotifierWatcher",
                                "StatusNotifierHostUnregistered",
                            )
                            .unwrap(),
                            &mut error_fn,
                            Error::new(Operation::WatcherSignal, ErrorKind::SendFailed),
                        );
                    }

                    if watcher.items.remove(&name) {
                        send_signal(
                            &self.channel,
                            Message::new_signal(
                                "/StatusNotifierWatcher",
                                "org.kde.StatusNotifierWatcher",
                                "StatusNotifierItemUnregistered",
                            )
                            .unwrap()
                            .append1(&name),
                            &mut error_fn,
                            Error::new(Operation::WatcherSignal, ErrorKind::SendFailed),
                        );
                    }
                },
                MsgMatch {
                    sender: Some(sender),
                    ty: MessageType::MethodReturn,
                    ..
                } => {
                    let Some(reply_serial) = message.get_reply_serial() else {
                        error_fn(Error::with_peer(
                            Operation::Reply,
                            ErrorKind::InvalidData,
                            sender,
                        ));
                        continue;
                    };

                    let Some((_, reply_to)) = self.pending_replies.remove(&reply_serial) else {
                        error_fn(Error::with_peer(
                            Operation::Reply,
                            ErrorKind::InvalidData,
                            sender,
                        ));
                        continue;
                    };

                    match reply_to {
                        ReplyTo::RegisterHost => {
                            if self.intl_watcher_op.is_some() {
                                continue;
                            }

                            match self
                                .channel
                                .send(Message::call_with_args(
                                    "org.kde.StatusNotifierWatcher",
                                    "/StatusNotifierWatcher",
                                    "org.freedesktop.DBus.Properties",
                                    "Get",
                                    (
                                        "org.kde.StatusNotifierWatcher",
                                        "RegisteredStatusNotifierItems",
                                    ),
                                ))
                                .map_err(|()| {
                                    Error::new(Operation::GetItems, ErrorKind::SendFailed)
                                }) {
                                Ok(serial) => {
                                    self.pending_replies
                                        .insert(serial, (Instant::now(), ReplyTo::NotifierItems));
                                },
                                Err(e) => error_fn(e),
                            }
                        },
                        ReplyTo::NotifierItems => {
                            let Ok(items) = message.read1::<Variant<Vec<String>>>() else {
                                error_fn(Error::with_peer(
                                    Operation::ItemGet,
                                    ErrorKind::InvalidData,
                                    message.sender().unwrap().to_string(),
                                ));
                                continue;
                            };

                            for item_name in items.0 {
                                self.items.insert(
                                    item_name.clone(),
                                    IntlItem {
                                        menu_path: String::new(),
                                        menu_revision: 0,
                                        item_op: None,
                                    },
                                );

                                match self.channel.send(msg_item_get_all(&item_name)) {
                                    Ok(serial) => {
                                        self.pending_replies.insert(
                                            serial,
                                            (
                                                Instant::now(),
                                                ReplyTo::ItemGetAll {
                                                    item_name: item_name.clone(),
                                                },
                                            ),
                                        );
                                    },
                                    Err(()) => {
                                        error_fn(Error::with_peer(
                                            Operation::ItemGet,
                                            ErrorKind::SendFailed,
                                            item_name.clone(),
                                        ));
                                    },
                                }
                            }
                        },
                        ReplyTo::ItemGetAll {
                            item_name,
                        } => {
                            let Some(intl_item) = self.items.get_mut(&item_name) else {
                                error_fn(Error::with_peer(
                                    Operation::ItemGet,
                                    ErrorKind::InvalidData,
                                    item_name,
                                ));
                                continue;
                            };

                            let Ok(mut fields) =
                                message.read1::<HashMap<String, Variant<Box<dyn RefArg>>>>()
                            else {
                                error_fn(Error::with_peer(
                                    Operation::ItemGet,
                                    ErrorKind::InvalidData,
                                    item_name,
                                ));
                                continue;
                            };

                            let mut item = Item {
                                bus_name: item_name.clone(),
                                ..Item::default()
                            };

                            macro_rules! parse_string_fields {
                                (
                                    $item:expr,
                                    $fields:expr,
                                    { $($field:ident => $key:expr),* $(,)? }
                                ) => {
                                    $(
                                        $item.$field = $fields
                                            .remove($key)
                                            .and_then(|arg| arg.as_str().map(|s| s.to_string()))
                                            .unwrap_or_default();
                                    )*
                                };
                            }

                            parse_string_fields!(
                                item,
                                fields,
                                {
                                    category => "Category",
                                    id => "Id",
                                    title => "Title",
                                    status => "Status",
                                    icon_theme_path => "IconThemePath",
                                    icon_name => "IconName",
                                    overlay_icon_name => "OverlayIconName",
                                    attention_icon_name => "AttentionIconName",
                                    attention_movie_name => "AttentionMovieName",
                                }
                            );

                            parse_string_fields!(intl_item, fields, { menu_path => "Menu" });

                            item.window_id = fields
                                .remove("WindowId")
                                .and_then(|arg| arg.as_u64())
                                .unwrap_or_default();

                            item.item_is_menu = fields
                                .remove("ItemIsMenu")
                                .and_then(|arg| arg.as_i64().map(|val| val == 1))
                                .unwrap_or_default();

                            macro_rules! parse_pixmap_fields {
                                (
                                    $item:expr,
                                    $fields:expr,
                                    { $($field:ident => $key:expr),* $(,)? }
                                ) => {
                                    $(
                                        $item.$field = $fields
                                            .remove($key)
                                            .map(|arg| parse_pixmap_arg(&arg.0))
                                            .unwrap_or_default();
                                    )*
                                };
                            }

                            parse_pixmap_fields!(
                                item,
                                fields,
                                {
                                    icon_pixmap => "IconPixmap",
                                    overlay_icon_pixmap => "OverlayIconPixmap",
                                    attention_icon_pixmap => "AttentionIconPixmap",
                                }
                            );

                            item.tool_tip = fields
                                .remove("ToolTip")
                                .map(|arg| parse_tool_tip_arg(&arg.0))
                                .unwrap_or_default();

                            if !intl_item.menu_path.is_empty() {
                                let args: (i32, i32, &[&str]) = (0, -1, &[]);

                                match self.channel.send(Message::call_with_args(
                                    &item_name,
                                    &intl_item.menu_path,
                                    "com.canonical.dbusmenu",
                                    "GetLayout",
                                    args,
                                )) {
                                    Ok(serial) => {
                                        self.pending_replies.insert(
                                            serial,
                                            (
                                                Instant::now(),
                                                ReplyTo::MenuGetLayout {
                                                    item_name: item_name.clone(),
                                                    parent_id: 0,
                                                },
                                            ),
                                        );
                                    },
                                    Err(()) => {
                                        error_fn(Error::with_peer(
                                            Operation::MenuGetLayout,
                                            ErrorKind::SendFailed,
                                            item_name.clone(),
                                        ));
                                    },
                                }
                            }

                            if intl_item.item_op.replace(item).is_none() {
                                event_fn(
                                    item_name,
                                    intl_item.item_op.as_ref().expect("unreachable"),
                                    Event::Added,
                                );
                            }
                        },
                        ReplyTo::ItemGet {
                            item_name,
                            property,
                        } => {
                            let Some(intl_item) = self.items.get_mut(&item_name) else {
                                error_fn(Error::with_peer(
                                    Operation::ItemGet,
                                    ErrorKind::InvalidData,
                                    item_name,
                                ));
                                continue;
                            };

                            let Some(item) = intl_item.item_op.as_mut() else {
                                error_fn(Error::with_peer(
                                    Operation::ItemGet,
                                    ErrorKind::InvalidData,
                                    item_name,
                                ));
                                continue;
                            };

                            match property {
                                "Title" => {
                                    if let Ok(title) = message.read1::<Variant<String>>()
                                        && title.0 != item.title
                                    {
                                        item.title = title.0;
                                        event_fn(item_name, item, Event::UpdatedTitle);
                                    }
                                },
                                "IconThemePath" => {
                                    if let Ok(path) = message.read1::<Variant<String>>()
                                        && path.0 != item.icon_theme_path
                                    {
                                        item.icon_theme_path = path.0;
                                        event_fn(item_name, item, Event::UpdatedIconThemePath);
                                    }
                                },
                                "IconName" => {
                                    if let Ok(name) = message.read1::<Variant<String>>()
                                        && name.0 != item.icon_name
                                    {
                                        item.icon_name = name.0;
                                        event_fn(item_name, item, Event::UpdatedIconName);
                                    }
                                },
                                "IconPixmap" => {
                                    if let Some(pixmap) = message
                                        .read1::<Variant<Box<dyn RefArg>>>()
                                        .ok()
                                        .map(|arg| parse_pixmap_arg(&arg.0))
                                        && pixmap != item.icon_pixmap
                                    {
                                        item.icon_pixmap = pixmap;
                                        event_fn(item_name, item, Event::UpdatedIconPixmap);
                                    }
                                },
                                "AttentionIconName" => {
                                    if let Ok(name) = message.read1::<Variant<String>>()
                                        && name.0 != item.attention_icon_name
                                    {
                                        item.attention_icon_name = name.0;
                                        event_fn(item_name, item, Event::UpdatedAttentionIconName);
                                    }
                                },
                                "AttentionIconPixmap" => {
                                    if let Some(pixmap) = message
                                        .read1::<Variant<Box<dyn RefArg>>>()
                                        .ok()
                                        .map(|arg| parse_pixmap_arg(&arg.0))
                                        && pixmap != item.attention_icon_pixmap
                                    {
                                        item.attention_icon_pixmap = pixmap;
                                        event_fn(
                                            item_name,
                                            item,
                                            Event::UpdatedAttentionIconPixmap,
                                        );
                                    }
                                },
                                "OverlayIconName" => {
                                    if let Ok(name) = message.read1::<Variant<String>>()
                                        && name.0 != item.overlay_icon_name
                                    {
                                        item.overlay_icon_name = name.0;
                                        event_fn(item_name, item, Event::UpdatedOverlayIconName);
                                    }
                                },
                                "OverlayIconPixmap" => {
                                    if let Some(pixmap) = message
                                        .read1::<Variant<Box<dyn RefArg>>>()
                                        .ok()
                                        .map(|arg| parse_pixmap_arg(&arg.0))
                                        && pixmap != item.overlay_icon_pixmap
                                    {
                                        item.overlay_icon_pixmap = pixmap;
                                        event_fn(item_name, item, Event::UpdatedOverlayIconPixmap);
                                    }
                                },
                                "ToolTip" => {
                                    if let Some(tool_tip) = message
                                        .read1::<Variant<Box<dyn RefArg>>>()
                                        .ok()
                                        .map(|arg| parse_tool_tip_arg(&arg.0))
                                        && tool_tip != item.tool_tip
                                    {
                                        item.tool_tip = tool_tip;
                                        event_fn(item_name, item, Event::UpdatedToolTip);
                                    }
                                },
                                _ => (),
                            }
                        },
                        ReplyTo::MenuGetLayout {
                            item_name,
                            parent_id,
                        } => {
                            let Some(intl_item) = self.items.get_mut(&item_name) else {
                                error_fn(Error::with_peer(
                                    Operation::MenuGetLayout,
                                    ErrorKind::InvalidData,
                                    item_name,
                                ));
                                continue;
                            };

                            let Some(item) = intl_item.item_op.as_mut() else {
                                error_fn(Error::with_peer(
                                    Operation::MenuGetLayout,
                                    ErrorKind::InvalidData,
                                    item_name,
                                ));
                                continue;
                            };

                            let (node, depth) = match parent_id {
                                0 => (&mut item.menu, 0),
                                _ => {
                                    match find_menu_node(&mut item.menu, parent_id, 0) {
                                        Some(some) => some,
                                        None => {
                                            error_fn(Error::with_peer(
                                                Operation::MenuGetLayout,
                                                ErrorKind::InvalidData,
                                                item_name,
                                            ));
                                            continue;
                                        },
                                    }
                                },
                            };

                            let (Some(revision), Some(arg)) =
                                message.get2::<u32, Box<dyn RefArg>>()
                            else {
                                error_fn(Error::with_peer(
                                    Operation::MenuGetLayout,
                                    ErrorKind::InvalidData,
                                    item_name,
                                ));
                                continue;
                            };

                            let Ok(new_node) = parse_menu(arg.as_ref(), depth) else {
                                error_fn(Error::with_peer(
                                    Operation::MenuGetLayout,
                                    ErrorKind::InvalidData,
                                    item_name,
                                ));
                                continue;
                            };

                            *node = new_node;
                            intl_item.menu_revision = revision;
                            event_fn(item_name, item, Event::UpdatedMenu);
                        },
                        ReplyTo::ItemMethod {
                            ..
                        }
                        | ReplyTo::MenuEvent {
                            ..
                        } => (),
                    }
                },
                MsgMatch {
                    sender: Some(sender),
                    ty: MessageType::Error,
                    ..
                } => {
                    let Some(reply_serial) = message.get_reply_serial() else {
                        error_fn(Error::with_peer(
                            Operation::Reply,
                            ErrorKind::InvalidData,
                            sender,
                        ));
                        continue;
                    };

                    let Some((_, reply_to)) = self.pending_replies.remove(&reply_serial) else {
                        error_fn(Error::with_peer(
                            Operation::Reply,
                            ErrorKind::InvalidData,
                            sender,
                        ));
                        continue;
                    };

                    let Err(e) = message.as_result() else {
                        continue;
                    };

                    match reply_to {
                        ReplyTo::RegisterHost => {
                            error_fn(Error::new(Operation::RegisterHost, ErrorKind::Dbus(e)));
                        },
                        ReplyTo::NotifierItems => {
                            error_fn(Error::new(Operation::GetItems, ErrorKind::Dbus(e)));
                        },
                        ReplyTo::ItemGetAll {
                            item_name,
                        }
                        | ReplyTo::ItemGet {
                            item_name, ..
                        } => {
                            error_fn(Error::with_peer(
                                Operation::ItemGet,
                                ErrorKind::Dbus(e),
                                item_name,
                            ));
                        },
                        ReplyTo::MenuGetLayout {
                            item_name, ..
                        } => {
                            error_fn(Error::with_peer(
                                Operation::MenuGetLayout,
                                ErrorKind::Dbus(e),
                                item_name,
                            ));
                        },
                        ReplyTo::ItemMethod {
                            item_name,
                        } => {
                            error_fn(Error::with_peer(
                                Operation::ItemMethod,
                                ErrorKind::Dbus(e),
                                item_name,
                            ));
                        },
                        ReplyTo::MenuEvent {
                            item_name,
                        } => {
                            error_fn(Error::with_peer(
                                Operation::MenuEvent,
                                ErrorKind::Dbus(e),
                                item_name,
                            ));
                        },
                    }
                },
                MsgMatch {
                    ty: MessageType::MethodCall,
                    ..
                } => {
                    let _ = self
                        .channel
                        .send(reply_error(&message, "org.freedesktop.DBus.Error.Failed"));
                },
                _ => (),
            }
        }

        self.pending_replies.retain(|_, (inst_sent, reply_to)| {
            if inst_sent.elapsed() > Duration::from_millis(500) {
                match reply_to {
                    ReplyTo::RegisterHost => {
                        error_fn(Error::new(Operation::RegisterHost, ErrorKind::Timeout));
                    },
                    ReplyTo::NotifierItems => {
                        error_fn(Error::new(Operation::GetItems, ErrorKind::Timeout));
                    },
                    ReplyTo::ItemGetAll {
                        item_name,
                    }
                    | ReplyTo::ItemGet {
                        item_name, ..
                    } => {
                        error_fn(Error::with_peer(
                            Operation::ItemGet,
                            ErrorKind::Timeout,
                            item_name.clone(),
                        ));
                    },
                    ReplyTo::MenuGetLayout {
                        item_name, ..
                    } => {
                        error_fn(Error::with_peer(
                            Operation::MenuGetLayout,
                            ErrorKind::Timeout,
                            item_name.clone(),
                        ));
                    },
                    ReplyTo::ItemMethod {
                        item_name,
                    } => {
                        error_fn(Error::with_peer(
                            Operation::ItemMethod,
                            ErrorKind::Timeout,
                            item_name.clone(),
                        ));
                    },
                    ReplyTo::MenuEvent {
                        item_name,
                    } => {
                        error_fn(Error::with_peer(
                            Operation::MenuEvent,
                            ErrorKind::Timeout,
                            item_name.clone(),
                        ));
                    },
                }

                false
            } else {
                true
            }
        });

        Ok(self.channel.watch().write)
    }
}

impl AsRawFd for Host {
    fn as_raw_fd(&self) -> RawFd {
        self.channel.watch().fd
    }
}

impl AsFd for Host {
    fn as_fd(&self) -> BorrowedFd<'_> {
        unsafe { BorrowedFd::borrow_raw(self.channel.watch().fd) }
    }
}

fn reply_error(message: &Message, error: &str) -> Message {
    message.error(&dbus::strings::ErrorName::new(error).unwrap(), c"")
}

fn msg_item_get_all<I>(item_name: &I) -> Message
where
    I: AsRef<str>,
{
    Message::call_with_args(
        item_name.as_ref(),
        "/StatusNotifierItem",
        "org.freedesktop.DBus.Properties",
        "GetAll",
        ("org.kde.StatusNotifierItem",),
    )
}

fn msg_item_get_prop(item_name: &str, property: &'static str) -> Message {
    Message::call_with_args(
        item_name,
        "/StatusNotifierItem",
        "org.freedesktop.DBus.Properties",
        "Get",
        ("org.kde.StatusNotifierItem", property),
    )
}

fn send_signal(
    channel: &Channel,
    message: Message,
    error_fn: &mut impl FnMut(Error),
    error: Error,
) {
    if channel.send(message).is_err() {
        error_fn(error);
    }
}

fn parse_pixmap_arg(value: &dyn RefArg) -> Vec<Pixmap> {
    let mut pixmaps = Vec::new();

    let Some(iter) = value.as_iter() else {
        return pixmaps;
    };

    for item in iter {
        let Some(mut fields) = item.as_iter() else {
            continue;
        };

        let Some(width) = fields.next().and_then(RefArg::as_i64) else {
            continue;
        };

        let Some(height) = fields.next().and_then(RefArg::as_i64) else {
            continue;
        };

        let Some(bytes_arg) = fields.next() else {
            continue;
        };

        let Some(byte_iter) = bytes_arg.as_iter() else {
            continue;
        };

        let bytes: Vec<u8> = byte_iter
            .filter_map(|b| b.as_u64().map(|v| v as u8))
            .collect();

        if width < 1
            || width > i32::MAX as i64
            || height < 1
            || height > i32::MAX as i64
            || width as usize * height as usize * 4 != bytes.len()
        {
            continue;
        }

        pixmaps.push(Pixmap {
            dimensions: [width as i32, height as i32],
            image_data: bytes,
        });
    }

    pixmaps
}

fn parse_tool_tip_arg(value: &dyn RefArg) -> ToolTip {
    let Some(mut iter) = value.as_iter() else {
        return ToolTip::default();
    };

    let Some(icon_name) = iter
        .next()
        .and_then(|arg| arg.as_str())
        .map(|arg| arg.to_string())
    else {
        return ToolTip::default();
    };

    let Some(icon_pixmap) = iter.next().map(|arg| parse_pixmap_arg(&arg)) else {
        return ToolTip::default();
    };

    let Some(title) = iter
        .next()
        .and_then(|arg| arg.as_str())
        .map(|arg| arg.to_string())
    else {
        return ToolTip::default();
    };

    let Some(description) = iter
        .next()
        .and_then(|arg| arg.as_str())
        .map(|arg| arg.to_string())
    else {
        return ToolTip::default();
    };

    ToolTip {
        icon_name,
        icon_pixmap,
        title,
        description,
    }
}

fn find_menu_node(
    node: &mut MenuNode,
    find_id: i32,
    depth: usize,
) -> Option<(&mut MenuNode, usize)> {
    match node {
        MenuNode::Empty => None,
        MenuNode::Menu {
            id, ..
        } if *id == find_id => Some((node, depth)),
        MenuNode::Menu {
            children, ..
        } => {
            children
                .iter_mut()
                .find_map(|child| find_menu_node(child, find_id, depth + 1))
        },
        MenuNode::Item {
            id, ..
        }
        | MenuNode::Separator {
            id, ..
        } if *id == find_id => Some((node, depth)),
        _ => None,
    }
}

fn parse_menu(arg: &dyn RefArg, depth: usize) -> Result<MenuNode, ()> {
    if depth > 8 {
        return Err(());
    }

    let mut struct_iter = arg.as_iter().ok_or(())?;
    let menu_id = struct_iter.next().ok_or(())?.as_i64().ok_or(())? as i32;
    let mut property_iter = struct_iter.next().ok_or(())?.as_iter().ok_or(())?;

    let mut is_submenu = false;
    let mut is_separator = false;
    let mut visible = true;
    let mut enabled = true;
    let mut label = String::new();
    let mut icon_name = String::new();
    let mut icon_data = IconData(Vec::new());
    let mut toggle_state = ToggleState::Unknown;
    let mut toggle_type = ToggleType::None;

    while let Some(p_name_arg) = property_iter.next() {
        let Some(p_name) = p_name_arg.as_str() else {
            continue;
        };

        let Some(p_value_arg) = property_iter.next() else {
            continue;
        };

        match p_name {
            "children-display" => {
                let Some(p_value) = p_value_arg.as_str() else {
                    continue;
                };

                if p_value != "submenu" {
                    continue;
                }

                is_submenu = true;
            },
            "visible" => {
                let Some(p_value) = p_value_arg.as_i64() else {
                    continue;
                };

                visible = p_value == 1;
            },
            "enabled" => {
                let Some(p_value) = p_value_arg.as_i64() else {
                    continue;
                };

                enabled = p_value == 1;
            },
            "type" => {
                let Some(p_value) = p_value_arg.as_str() else {
                    continue;
                };

                if p_value != "separator" {
                    continue;
                }

                is_separator = true;
            },
            "label" => {
                let Some(p_value) = p_value_arg.as_str() else {
                    continue;
                };

                label = p_value.to_string();
            },
            "icon-name" => {
                let Some(p_value) = p_value_arg.as_str() else {
                    continue;
                };

                icon_name = p_value.to_string();
            },
            "icon-data" => {
                let Some(new_icon_data) = parse_icon_data_arg(&p_value_arg) else {
                    continue;
                };

                icon_data = new_icon_data;
            },
            "toggle-state" => {
                let Some(new_toggle_state) = parse_toggle_state_arg(&p_value_arg) else {
                    continue;
                };

                toggle_state = new_toggle_state;
            },
            "toggle-type" => {
                let Some(new_toggle_type) = parse_toggle_type_arg(&p_value_arg) else {
                    continue;
                };

                toggle_type = new_toggle_type;
            },
            _ => (),
        }
    }

    if depth == 0 || is_submenu {
        let mut children = Vec::new();
        let submenu_iter = struct_iter.next().ok_or(())?.as_iter().ok_or(())?;

        for menu_variant in submenu_iter {
            if let Some(mut menu_variant_iter) = menu_variant.as_iter()
                && let Some(menu_struct) = menu_variant_iter.next()
                && let Ok(submenu) = parse_menu(menu_struct, depth + 1)
            {
                children.push(submenu);
            }
        }

        Ok(MenuNode::Menu {
            id: menu_id,
            label,
            enabled,
            visible,
            children,
        })
    } else if is_separator {
        Ok(MenuNode::Separator {
            id: menu_id,
            visible,
        })
    } else {
        Ok(MenuNode::Item {
            id: menu_id,
            label,
            enabled,
            visible,
            icon_name,
            icon_data,
            toggle_type,
            toggle_state,
        })
    }
}

fn parse_icon_data_arg(arg: &dyn RefArg) -> Option<IconData> {
    let mut bytes_variant_iter = arg.as_iter()?;
    let bytes_array = bytes_variant_iter.next()?;
    let bytes_iter = bytes_array.as_iter()?;
    let mut icon_data = Vec::new();

    for byte_arg in bytes_iter {
        let byte_u64 = byte_arg.as_u64()?;

        if byte_u64 > 255 {
            return None;
        }

        icon_data.push(byte_u64 as u8);
    }

    Some(IconData(icon_data))
}

fn parse_toggle_state_arg(arg: &dyn RefArg) -> Option<ToggleState> {
    Some(match arg.as_i64()? {
        0 => ToggleState::Off,
        1 => ToggleState::On,
        _ => ToggleState::Unknown,
    })
}

fn parse_toggle_type_arg(arg: &dyn RefArg) -> Option<ToggleType> {
    match arg.as_str()? {
        "checkmark" => Some(ToggleType::Checkmark),
        "radio" => Some(ToggleType::Radio),
        _ => None,
    }
}
