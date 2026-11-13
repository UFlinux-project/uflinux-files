//! Foundational types shared by the file manager, the indexer and the desktop
//! icon provider.
//!
//! The whole application rests on one idea: a [`Location`] is a URI, and every
//! location except an archive is also a real path on disk. Network shares are
//! mounted with kernel or FUSE helpers, so browsing them is indistinguishable
//! from browsing the local filesystem and no view, thumbnailer or drag-and-drop
//! handler needs a remote special case. Archives are the only virtual
//! filesystem, and they are addressed as `archive://<path>!<inner>`.

#![deny(unsafe_code)]

pub mod atomic;
pub mod config;
pub mod entry;
pub mod error;
pub mod location;
pub mod paths;
pub mod sort;

pub use config::{DesktopSlot, Preferences};
pub use entry::{Entry, EntryFlags, EntryKind};
pub use error::{Error, Result};
pub use location::Location;
pub use sort::{SortField, SortKey};

/// Tests that change the process environment have to hold this, because
/// `set_var` is unsafe in edition 2024 precisely because another thread may be
/// reading the environment at the same time.
#[cfg(test)]
pub(crate) fn test_env() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
