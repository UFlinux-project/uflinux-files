//! The D-Bus wire format of `org.uflinux.FileManager.Desktop1`.
//!
//! Same shape as `org.uflinux.Search1`: one JSON string in, one JSON string out,
//! every field with a default where a default makes sense, unknown fields
//! rejected rather than ignored. The reason to copy that convention rather than
//! invent another is that both ends of this protocol are already written against
//! it, and a reviewer should be able to read one and guess the other.
//!
//! The direction of the traffic is the interesting part. The renderer sends
//! [`Request::SetGeometry`] because it is the only component that knows the
//! screen layout, and [`Request::Place`] because it is the only component that
//! knows where the pointer was released. Everything else — what is on the
//! desktop, what it looks like, whether it may be moved — flows the other way.
//!
//! There is no version field in a request, as in the search protocol: the two
//! binaries ship in one package and a mismatch is a broken install, so
//! [`Request::Status`] reports the version instead of every call arguing about it.
//!
//! Memory: the model is the only large payload, and it is bounded in
//! [`crate::model`] before it is serialized. A thumbnail reply carries a file
//! descriptor number, not the bytes, so the largest thing on this bus stays a
//! desktop-sized model.

use serde::{Deserialize, Serialize};
use ufm_core::Error;

use crate::model::{Geometry, Model};

/// The version this build speaks, and the value [`StatusReply::version`] reports.
pub const PROTOCOL_VERSION: u32 = 1;

/// D-Bus name claimed by the file manager in provider mode.
pub const SESSION_NAME: &str = "org.uflinux.FileManager.Desktop1";

/// Object path the interface is exported on.
pub const OBJECT_PATH: &str = "/org/uflinux/FileManager/Desktop";

/// Name of the `Changed` signal: the model moved, re-read it.
pub const CHANGED: &str = "Changed";

/// Name of the `Progress` signal: a scan is running.
pub const PROGRESS: &str = "Progress";

/// Largest request body. A request names one file, so this is generous already;
/// the cap is what stops a confused client making the provider allocate.
pub const MAX_REQUEST: usize = 256 * 1024;

/// Largest reply body, which is what bounds the model.
pub const MAX_REPLY: usize = 2 * 1024 * 1024;

/// The error namespace.
pub mod error {
    /// A request this provider cannot act on: unknown field, bad id, bad grid.
    pub const INVALID: &str = "org.uflinux.FileManager.Desktop1.Error.Invalid";
    /// The request named an item that is not on the desktop.
    pub const NO_ITEM: &str = "org.uflinux.FileManager.Desktop1.Error.NoItem";
    /// The provider is between a change and the signal that announces it.
    pub const BUSY: &str = "org.uflinux.FileManager.Desktop1.Error.Busy";
}

/// Why a request was refused, with a message safe to show in a tooltip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireError {
    /// Not valid JSON for this interface, or a field the provider does not know.
    Invalid,
    /// No such desktop item. Distinct from `Invalid` because a renderer can
    /// legitimately act on an icon that vanished a moment ago, and should say so
    /// rather than report a protocol fault.
    NoItem,
    /// Try again: the provider is mid-change.
    Busy,
}

impl WireError {
    /// The D-Bus error name to return.
    #[must_use]
    pub const fn dbus_name(self) -> &'static str {
        match self {
            Self::Invalid => error::INVALID,
            Self::NoItem => error::NO_ITEM,
            Self::Busy => error::BUSY,
        }
    }

    #[must_use]
    pub const fn message(self) -> &'static str {
        match self {
            Self::Invalid => "Unknown or malformed request",
            Self::NoItem => "That item is no longer on the desktop",
            Self::Busy => "The desktop provider is busy",
        }
    }

    #[must_use]
    pub fn into_error(self) -> Error {
        match self {
            Self::NoItem => Error::UnsafeComponent(self.message().to_string()),
            other => Error::Protocol(other.message().to_string()),
        }
    }
}

impl From<WireError> for Error {
    fn from(e: WireError) -> Self {
        e.into_error()
    }
}

/// What a thumbnail reply describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Format {
    /// PNG bytes in the pipe, already the requested size.
    Png,
    /// No thumbnail exists; the renderer should use the icon name from the model
    /// and stop asking for this file for a while.
    Icon,
}

/// A thumbnail, or the reason there is not one.
///
/// The bytes do not travel in the JSON. The provider writes them to a pipe and
/// passes the descriptor as a unix fd; [`Thumbnail::fd`] is the index of that fd
/// in the D-Bus message's fd list, which is how a zero-copy reply is described
/// in a format that has no fd type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Thumbnail {
    pub width: u16,
    pub height: u16,
    pub format: Format,
    /// Index into the message's fd list. `None` when `format` is
    /// [`Format::Icon`], where there is nothing to read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fd: Option<u8>,
}

impl Thumbnail {
    /// A PNG at `fd_index` in this message's fd list.
    #[must_use]
    pub const fn png(width: u16, height: u16, fd_index: u8) -> Self {
        Self {
            width,
            height,
            format: Format::Png,
            fd: Some(fd_index),
        }
    }

    /// No thumbnail; fall back to an icon.
    #[must_use]
    pub const fn icon() -> Self {
        Self {
            width: 0,
            height: 0,
            format: Format::Icon,
            fd: None,
        }
    }
}

/// What the provider is doing, for a progress bar that should not lie.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Phase {
    /// Reading the directory.
    #[default]
    Scanning,
    /// Deciding where icons go.
    Placing,
    /// Finished; a renderer can stop showing a spinner.
    Done,
}

/// The `Changed` signal: the model has a new generation, pull it.
///
/// Deliberately not the model itself. Two changes that arrive back to back
/// collapse into one repaint if the renderer only learns that something
/// changed, which is the behaviour wanted while a folder is being dragged
/// across the desktop.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Changed {
    pub generation: u64,
    /// How many items the new model has, so a renderer can notice an empty
    /// desktop without a round trip when it wants to.
    pub items: usize,
    /// What happened, for a log line. Never shown raw.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// The `Progress` signal.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Progress {
    pub phase: Phase,
    pub done: u64,
    pub total: u64,
}

/// What the provider can say about itself.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StatusReply {
    pub version: u32,
    /// The directory being shown, for an error message.
    pub desktop: String,
    pub generation: u64,
    pub items: usize,
    pub monitors: usize,
    pub writable: bool,
    /// Whether a renderer has claimed the interface. `false` is the answer that
    /// makes `--desktop-icons` fall back to drawing its own layer.
    pub renderer: bool,
    /// Anything the user should be told: a desktop directory that cannot be
    /// read, a layout file that did not parse.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// Every call the renderer may make.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "method", rename_all = "kebab-case", deny_unknown_fields)]
pub enum Request {
    /// What version, which directory, how many icons, is anyone listening.
    Status,
    /// The whole model.
    GetModel,
    /// Report the grids. Replaces the set wholesale, because a monitor that
    /// stopped being mentioned is a monitor that was unplugged.
    SetGeometry { monitors: Vec<Geometry> },
    /// Move an icon to a cell. Flat rather than a nested cell so a drag can be
    /// written as three integers without a struct.
    Place {
        id: String,
        monitor: u8,
        col: u16,
        row: u16,
    },
    /// Double-click or Enter. The provider launches, so the renderer needs no
    /// knowledge of desktop entries or `MIME`.
    Launch { id: String },
    /// Open the parent folder with this item selected.
    Reveal { id: String },
    /// Ask the file manager to show a context menu for one item.
    ContextMenu { id: String },
    /// Move an item to the trash. The provider does it, so the trash directory
    /// and the confirmation rule live in one place.
    Trash { id: String },
    /// A thumbnail for one file. `path` is absolute and inside the desktop
    /// directory; the provider re-validates rather than trusting the renderer.
    RequestThumbnail { path: String, size: u32 },
}

impl Request {
    /// Parse a request body, refusing anything over [`MAX_REQUEST`] before
    /// looking at it.
    ///
    /// A body that is not valid JSON for this interface is [`WireError::Invalid`]
    /// with no detail: the detail would echo back user data, and the renderer
    /// cannot do anything with it beyond showing an error.
    pub fn decode(body: &[u8]) -> Result<Self, WireError> {
        if body.len() > MAX_REQUEST {
            return Err(WireError::Invalid);
        }
        serde_json::from_slice(body).map_err(|_| WireError::Invalid)
    }

    /// The method name, for a log line or a metric. Never user data.
    #[must_use]
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::Status => "status",
            Self::GetModel => "get-model",
            Self::SetGeometry { .. } => "set-geometry",
            Self::Place { .. } => "place",
            Self::Launch { .. } => "launch",
            Self::Reveal { .. } => "reveal",
            Self::ContextMenu { .. } => "context-menu",
            Self::Trash { .. } => "trash",
            Self::RequestThumbnail { .. } => "request-thumbnail",
        }
    }

    /// The item this request acts on, if any — which is what lets the provider
    /// answer [`WireError::NoItem`] once instead of in five arms.
    #[must_use]
    pub fn item(&self) -> Option<&str> {
        match self {
            Self::Launch { id }
            | Self::Reveal { id }
            | Self::ContextMenu { id }
            | Self::Trash { id }
            | Self::Place { id, .. } => Some(id),
            Self::Status
            | Self::GetModel
            | Self::SetGeometry { .. }
            | Self::RequestThumbnail { .. } => None,
        }
    }

    /// The grid this request reports, if it is the one that reports grids.
    #[must_use]
    pub fn monitors(&self) -> Option<&[Geometry]> {
        match self {
            Self::SetGeometry { monitors } => Some(monitors),
            _ => None,
        }
    }
}

/// Every answer the provider may give.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reply", rename_all = "kebab-case", deny_unknown_fields)]
pub enum Reply {
    /// It worked, with nothing to report.
    Ok,
    /// The model, flattened so a reply is one object rather than an object
    /// holding a nested copy of the same field names.
    Model {
        #[serde(flatten)]
        model: Model,
    },
    Status(StatusReply),
    Thumbnail(Thumbnail),
}

impl Reply {
    /// Serialize, refusing to build a message that could not be sent.
    ///
    /// The model is checked before it is encoded, so an over-long desktop is
    /// reported as an error rather than truncated mid-array into something the
    /// renderer would paint as a partial desktop.
    pub fn encode(&self) -> Result<Vec<u8>, WireError> {
        if let Self::Model { model } = self {
            model
                .check_size(MAX_REPLY)
                .map_err(|_| WireError::Invalid)?;
        }
        let body = serde_json::to_vec(self).map_err(|_| WireError::Invalid)?;
        if body.len() > MAX_REPLY {
            return Err(WireError::Invalid);
        }
        Ok(body)
    }

    /// Parse a reply, for the provider side of a test or a proxy.
    pub fn decode(body: &[u8]) -> Result<Self, WireError> {
        if body.len() > MAX_REPLY {
            return Err(WireError::Invalid);
        }
        serde_json::from_slice(body).map_err(|_| WireError::Invalid)
    }

    #[must_use]
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Model { .. } => "model",
            Self::Status(_) => "status",
            Self::Thumbnail(_) => "thumbnail",
        }
    }
}

impl Changed {
    /// The signal body for a model as it now stands.
    #[must_use]
    pub fn of(model: &Model, reason: Option<String>) -> Self {
        Self {
            generation: model.generation,
            items: model.items.len(),
            reason,
        }
    }

    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        // A signal carries a handful of integers, so it cannot fail or overflow
        // the cap; serializing to a Vec keeps the call site the same shape as a
        // reply in case that ever stops being true.
        serde_json::to_vec(self).unwrap_or_default()
    }
}

impl Progress {
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        serde_json::to_vec(self).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Cell, Item, Kind};
    use std::path::Path;

    fn geometry() -> Geometry {
        Geometry::new(0, 4, 4, 96, 96)
    }

    fn model() -> Model {
        let mut m = Model::new(Path::new("/home/u/Desktop"));
        m.set_geometry(vec![geometry()]).unwrap();
        m.push(Item::new("notes.txt", "Notes", Kind::File)).unwrap();
        m
    }

    fn decode(text: &str) -> Result<Request, WireError> {
        Request::decode(text.as_bytes())
    }

    #[test]
    fn a_bare_method_name_is_a_complete_request() {
        let r = decode(r#"{"method":"status"}"#).unwrap();
        assert_eq!(r, Request::Status);
        assert_eq!(r.kind(), "status");
        assert_eq!(r.item(), None);
        assert_eq!(
            decode(r#"{"method":"get-model"}"#).unwrap(),
            Request::GetModel
        );
    }

    #[test]
    fn every_method_round_trips_under_its_wire_name() {
        let cases = [
            (
                Request::SetGeometry {
                    monitors: vec![geometry()],
                },
                r#"{"method":"set-geometry","monitors":[{"monitor":0,"columns":4,"rows":4,"cell_w":96,"cell_h":96}]}"#,
            ),
            (
                Request::Place {
                    id: "a.txt".into(),
                    monitor: 1,
                    col: 2,
                    row: 3,
                },
                r#"{"method":"place","id":"a.txt","monitor":1,"col":2,"row":3}"#,
            ),
            (
                Request::Launch { id: "a.txt".into() },
                r#"{"method":"launch","id":"a.txt"}"#,
            ),
            (
                Request::Reveal { id: "a.txt".into() },
                r#"{"method":"reveal","id":"a.txt"}"#,
            ),
            (
                Request::ContextMenu { id: "a.txt".into() },
                r#"{"method":"context-menu","id":"a.txt"}"#,
            ),
            (
                Request::Trash { id: "a.txt".into() },
                r#"{"method":"trash","id":"a.txt"}"#,
            ),
            (
                Request::RequestThumbnail {
                    path: "/home/u/Desktop/a.png".into(),
                    size: 128,
                },
                r#"{"method":"request-thumbnail","path":"/home/u/Desktop/a.png","size":128}"#,
            ),
        ];
        for (request, text) in cases {
            let encoded = serde_json::to_string(&request).unwrap();
            assert_eq!(encoded, text);
            assert_eq!(decode(text).unwrap(), request, "{}", request.kind());
        }
    }

    #[test]
    fn an_unknown_field_or_method_is_refused_not_ignored() {
        // `monitor` misspelled would otherwise place an icon on screen 0 in
        // silence, which looks like a bug in the drag rather than in the caller.
        for bad in [
            r#"{"method":"place","id":"a","monitorr":0,"col":1,"row":1}"#,
            r#"{"method":"teleport","id":"a"}"#,
            r#"{"method":"launch"}"#,
            r#"{"method":"launch","id":"a","extra":1}"#,
            r#"{"method":"set-geometry","monitors":[{"monitor":0,"columns":4,"rows":4}]}"#,
            "not json",
            "",
            "[]",
        ] {
            let err = decode(bad).unwrap_err();
            assert_eq!(err, WireError::Invalid, "{bad} should be refused");
            assert_eq!(
                err.dbus_name(),
                "org.uflinux.FileManager.Desktop1.Error.Invalid"
            );
        }
    }

    #[test]
    fn a_request_over_the_cap_is_refused_before_it_is_parsed() {
        let big = format!(
            r#"{{"method":"launch","id":"{}"}}"#,
            "a".repeat(MAX_REQUEST)
        );
        assert_eq!(Request::decode(big.as_bytes()), Err(WireError::Invalid));
    }

    #[test]
    fn a_request_names_the_item_it_acts_on() {
        let r = Request::Place {
            id: "photos".into(),
            monitor: 0,
            col: 0,
            row: 0,
        };
        assert_eq!(r.item(), Some("photos"));
        assert_eq!(
            Request::Trash { id: "x".into() }.item(),
            Some("x".to_string()).as_deref()
        );
        assert_eq!(
            Request::RequestThumbnail {
                path: "/p".into(),
                size: 0
            }
            .item(),
            None,
            "a thumbnail is asked for by path, not by id"
        );
        assert_eq!(
            Request::SetGeometry {
                monitors: vec![geometry()]
            }
            .monitors()
            .map(<[Geometry]>::len),
            Some(1)
        );
        assert_eq!(Request::Status.monitors(), None);
    }

    #[test]
    fn a_model_reply_carries_the_model_flattened() {
        let m = model();
        let reply = Reply::Model { model: m.clone() };
        let body = reply.encode().unwrap();
        let text = String::from_utf8(body.clone()).unwrap();
        assert!(
            text.starts_with(r#"{"reply":"model","version":1,"generation":0,"#),
            "no nested model object: {text}"
        );
        assert_eq!(Reply::decode(&body).unwrap(), reply);
        assert_eq!(reply.kind(), "model");
    }

    #[test]
    fn a_status_reply_round_trips_and_omits_an_absent_note() {
        let reply = Reply::Status(StatusReply {
            version: PROTOCOL_VERSION,
            desktop: "/home/u/Desktop".into(),
            generation: 4,
            items: 12,
            monitors: 1,
            writable: true,
            renderer: true,
            note: None,
        });
        let body = reply.encode().unwrap();
        assert!(!String::from_utf8_lossy(&body).contains("note"));
        assert_eq!(Reply::decode(&body).unwrap(), reply);
        assert_eq!(
            Reply::decode(&Reply::Ok.encode().unwrap()).unwrap(),
            Reply::Ok
        );
    }

    #[test]
    fn a_thumbnail_says_where_the_bytes_are_or_that_there_are_none() {
        let png = Reply::Thumbnail(Thumbnail::png(128, 96, 0));
        let body = png.encode().unwrap();
        assert!(String::from_utf8_lossy(&body).contains(r#""fd":0"#));
        assert_eq!(Reply::decode(&body).unwrap(), png);
        let none = Reply::Thumbnail(Thumbnail::icon());
        let body = none.encode().unwrap();
        assert!(
            !String::from_utf8_lossy(&body).contains("fd"),
            "no fd to read: {body:?}"
        );
        assert_eq!(Reply::decode(&body).unwrap(), none);
    }

    #[test]
    fn an_oversized_model_is_an_error_rather_than_a_truncated_reply() {
        let mut m = model();
        for i in 0..4000 {
            let mut item = Item::new(format!("f{i}"), "n", Kind::File);
            item.tooltip = Some("y".repeat(500));
            m.items.push(item);
        }
        let reply = Reply::Model { model: m };
        assert_eq!(reply.encode(), Err(WireError::Invalid));
    }

    #[test]
    fn a_reply_over_the_cap_is_refused_on_the_way_in_too() {
        let body = vec![b'{'; MAX_REPLY + 1];
        assert_eq!(Reply::decode(&body), Err(WireError::Invalid));
    }

    #[test]
    fn the_changed_signal_describes_a_model_without_carrying_it() {
        let mut m = model();
        m.bump();
        m.bump();
        let signal = Changed::of(&m, Some("trash".into()));
        assert_eq!(
            signal,
            Changed {
                generation: 2,
                items: 1,
                reason: Some("trash".into())
            }
        );
        let body = signal.encode();
        assert!(body.len() < 128, "a signal is small: {body:?}");
        let back: Changed = serde_json::from_slice(&body).unwrap();
        assert_eq!(back, signal);
        let bare: Changed = serde_json::from_str(r#"{"generation":1,"items":0}"#).unwrap();
        assert_eq!(bare.reason, None);
    }

    #[test]
    fn progress_phases_use_wire_names() {
        let p = Progress {
            phase: Phase::Scanning,
            done: 3,
            total: 10,
        };
        let text = serde_json::to_string(&p).unwrap();
        assert_eq!(
            text, r#"{"phase":"scanning","done":3,"total":10}"#,
            "a renderer parses these names"
        );
        assert_eq!(Progress::default().phase, Phase::Scanning);
        assert_eq!(
            serde_json::to_string(&Phase::Done).unwrap(),
            r#""done""#,
            "the end of a crawl must be distinguishable from an empty scan"
        );
    }

    #[test]
    fn an_error_that_names_a_missing_item_is_not_a_protocol_fault() {
        assert_eq!(
            WireError::NoItem.dbus_name(),
            "org.uflinux.FileManager.Desktop1.Error.NoItem"
        );
        assert!(matches!(
            WireError::NoItem.into_error(),
            Error::UnsafeComponent(_)
        ));
        assert!(matches!(WireError::Busy.into_error(), Error::Protocol(_)));
        assert!(WireError::Busy.message().contains("busy"));
    }

    #[test]
    fn the_bus_names_match_the_shipped_service_files() {
        // These three strings are also in data/*.service and in the shell patch,
        // and a typo in one of them means the provider is simply never activated.
        assert_eq!(SESSION_NAME, "org.uflinux.FileManager.Desktop1");
        assert_eq!(OBJECT_PATH, "/org/uflinux/FileManager/Desktop");
        assert_eq!(CHANGED, "Changed");
        assert_eq!(PROGRESS, "Progress");
        assert_eq!(PROTOCOL_VERSION, 1);
    }

    #[test]
    fn a_cell_and_a_request_agree_on_the_shape_of_a_position() {
        let cell = Cell {
            monitor: 2,
            col: 5,
            row: 9,
        };
        let request = Request::Place {
            id: "a".into(),
            monitor: cell.monitor,
            col: cell.col,
            row: cell.row,
        };
        let Request::Place {
            monitor, col, row, ..
        } = request
        else {
            unreachable!()
        };
        assert_eq!(cell, Cell { monitor, col, row });
    }
}
