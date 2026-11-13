//! The desktop icon protocol: `org.uflinux.FileManager.Desktop1`.
//!
//! UFLinux already has one resident GTK process that owns the wallpaper and the
//! panel, and a second full-screen process drawing an icon grid on top of it
//! would cost a compositor's worth of surfaces to paint a dozen icons. So the
//! work is split along the line where the knowledge already is: the shell knows
//! the geometry of the screen and owns the pixels, the file manager knows the
//! filesystem and owns the facts. This crate is the seam between them.
//!
//! Three modules, and no I/O in any of them. [`wire`] is the request and reply
//! shapes, with the same rules the search protocol uses — one JSON string per
//! direction, a size cap, unknown fields refused. [`model`] is the data: what is
//! on the desktop and which cell each icon sits in, plus the placement rules both
//! sides need to agree on, because a renderer that guesses differently paints an
//! icon somewhere the provider will not move it back from. [`layout`] is the
//! provider's saved state, so an icon stays where the user put it across a
//! logout.
//!
//! The rules worth knowing before using it:
//!
//! An item is named by one path component, never by a path. [`Model::path_in`]
//! joins it to the desktop directory and refuses anything that would leave that
//! directory, so a renderer cannot ask for a thumbnail of `~/.ssh/id_rsa` by
//! sending an id with slashes in it.
//!
//! A cell names a monitor. Two screens both have a cell at 0,0, and a layout that
//! cannot say which one an icon is on loses half the icons the first time a
//! monitor is unplugged — which is when losing them hurts most.
//!
//! Nothing is deleted from the model by a geometry change. An icon whose monitor
//! vanished is moved to a screen that still exists, and only an icon that really
//! cannot fit anywhere is left without a cell, reported to the caller as such. A
//! shell that hid icons because their screen was temporarily off would look like
//! it had eaten the user's desktop.
//!
//! Memory: the model is tens of items, and the fields a renderer skips when
//! absent are skipped on the wire. The one payload that could be large — a
//! thumbnail — travels as a file descriptor, so this protocol never puts image
//! bytes in a message.

#![deny(unsafe_code)]
// The prose in these docs says "D-Bus", "GSettings" and "UFLinux" in plain
// sentences, and a doc comment is not the place to fight about backticks.
#![allow(clippy::doc_markdown)]

pub mod layout;
pub mod model;
pub mod wire;

pub use layout::{Applied, Layout, path};
pub use model::{
    Cell, DEFAULT_THUMBNAIL, Geometry, Item, Kind, MAX_COLUMNS, MAX_ID_LEN, MAX_ITEMS,
    MAX_MONITORS, MAX_ROWS, Model, ModelError, Outcome, valid_id,
};
pub use wire::{
    CHANGED, Changed, Format, MAX_REPLY, MAX_REQUEST, OBJECT_PATH, PROGRESS, PROTOCOL_VERSION,
    Phase, Progress, Reply, Request, SESSION_NAME, StatusReply, Thumbnail, WireError, error,
};
