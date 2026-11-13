//! Directory-entry metadata, in the shape both views and the indexer need.

use std::path::PathBuf;
use std::time::SystemTime;

use bitflags::bitflags;

bitflags! {
    /// Cheap flags computed once during a directory read, so the views never
    /// have to stat an entry again to draw it.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
    pub struct EntryFlags: u32 {
        /// Write permission for the effective user.
        const WRITABLE = 1 << 0;
        /// A symlink or other indirect reference.
        const SYMLINK = 1 << 1;
        /// The target of a symlink is missing.
        const BROKEN = 1 << 2;
        /// Inside a read-only archive. Drives the badge in the header bar.
        const READ_ONLY = 1 << 3;
        /// Hidden by a leading dot or by the user.
        const HIDDEN = 1 << 4;
        /// Starred by the user.
        const STARRED = 1 << 5;
        /// On a filesystem that has no trash can, so Delete is permanent here.
        const NO_TRASH = 1 << 6;
    }
}

/// What an entry is, reduced to the handful of shapes the UI distinguishes.
/// Anything else is [`EntryKind::File`] and is told apart by MIME type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EntryKind {
    File,
    Directory,
    MountPoint,
    /// A directory that is the root of an archive, i.e. an archive container.
    Archive,
    Device,
    Socket,
    Fifo,
    CharDevice,
    BlockDevice,
}

impl EntryKind {
    #[must_use]
    pub const fn is_dir(self) -> bool {
        matches!(self, Self::Directory | Self::MountPoint)
    }

    /// The icon name to fall back to when no MIME icon is known.
    #[must_use]
    pub const fn icon_name(self) -> &'static str {
        match self {
            Self::Directory | Self::MountPoint => "folder",
            Self::Archive => "package-x-generic",
            Self::Device | Self::CharDevice | Self::BlockDevice => "drive-harddisk",
            Self::Socket | Self::Fifo => "system-run",
            Self::File => "text-x-generic",
        }
    }
}

/// One row of a directory listing, or one search hit.
///
/// Deliberately plain data with owned `String`s rather than borrowed paths: a
/// listing is produced on a worker thread and published to the UI in slices, so
/// nothing here may borrow from the reader that made it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// The location to navigate to when this entry is activated.
    pub location: crate::Location,
    /// Display name. For an archive member this is the last inner component.
    pub name: String,
    pub kind: EntryKind,
    pub flags: EntryFlags,
    /// Bytes on disk for a file, 0 for a directory.
    pub size: u64,
    /// Bytes actually allocated, for the properties dialog.
    pub allocated: Option<u64>,
    pub modified: Option<SystemTime>,
    pub accessed: Option<SystemTime>,
    pub created: Option<SystemTime>,
    /// Unix mode bits when the filesystem has them.
    pub mode: Option<u32>,
    pub uid: Option<u32>,
    pub gid: Option<u32>,
    /// Guess from the extension, refined by the thumbnailer. `None` for a
    /// directory, which is keyed by icon name instead.
    pub mime: Option<String>,
    /// Number of entries inside a directory, when the reader already knows it.
    /// `None` means "not counted", which the list view renders as an em dash
    /// rather than a misleading zero.
    pub child_count: Option<u32>,
    /// Cached icon key: the MIME type, or `folder` for a directory. The view
    /// keeps a small map from key to `GtkIconPaintable` so thousands of rows
    /// share a handful of icons.
    pub icon_key: String,
}

impl Entry {
    /// Whether the entry is a directory-like thing that can be entered.
    #[must_use]
    pub fn navigable(&self) -> bool {
        self.kind.is_dir() || self.kind == EntryKind::Archive
    }

    #[must_use]
    pub fn hidden(&self) -> bool {
        self.flags.contains(EntryFlags::HIDDEN)
    }

    #[must_use]
    pub fn writable(&self) -> bool {
        self.flags.contains(EntryFlags::WRITABLE) && !self.flags.contains(EntryFlags::READ_ONLY)
    }

    /// A human-readable size, or the entry count for a directory.
    #[must_use]
    pub fn size_label(&self) -> String {
        match self.kind {
            EntryKind::Directory | EntryKind::MountPoint => match self.child_count {
                Some(n) => format!("{n}"),
                None => "\u{2014}".to_string(),
            },
            _ => human_size(self.size),
        }
    }

    #[must_use]
    pub fn modified_label(&self) -> String {
        self.modified
            .map_or_else(|| "\u{2014}".to_string(), crate::config::format_modified)
    }
}

/// Binary multiples, matching what the GNOME ecosystem shows: 1 KiB = 1024.
#[must_use]
pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    // Below 10 the extra digit is worth having, above that it is noise.
    let text = if value < 10.0 {
        format!("{value:.1}")
    } else {
        format!("{value:.0}")
    };
    format!("{text} {}", UNITS[unit])
}

/// The path a search hit or a recent entry points at, when it is a plain file.
#[must_use]
pub fn path_of(location: &crate::Location) -> Option<PathBuf> {
    location.as_path().map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_use_binary_multiples() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(999), "999 B");
        assert_eq!(human_size(1024), "1.0 KiB");
        assert_eq!(human_size(1536), "1.5 KiB");
        assert_eq!(human_size(1024 * 1024), "1.0 MiB");
        assert_eq!(human_size(1024 * 1024 * 1024), "1.0 GiB");
        // Below ten the decimal is informative; above it the extra digit is noise.
        assert_eq!(human_size(1_610_612_736), "1.5 GiB");
        assert_eq!(human_size(16 * 1024 * 1024 * 1024), "16 GiB");
        assert_eq!(human_size(u64::MAX), "16384 PiB");
    }

    #[test]
    fn flags_compose() {
        let f = EntryFlags::HIDDEN | EntryFlags::WRITABLE;
        assert!(f.contains(EntryFlags::HIDDEN));
        assert!(!f.contains(EntryFlags::READ_ONLY));
        // A read-only archive entry is never writable even if the mode says so.
        let ro = EntryFlags::WRITABLE | EntryFlags::READ_ONLY;
        let e = Entry {
            location: crate::Location::file("/x"),
            name: "x".into(),
            kind: EntryKind::Archive,
            flags: ro,
            size: 1,
            allocated: None,
            modified: None,
            accessed: None,
            created: None,
            mode: None,
            uid: None,
            gid: None,
            mime: None,
            child_count: None,
            icon_key: "package-x-generic".into(),
        };
        assert!(!e.writable());
        assert!(!e.navigable() || e.kind == EntryKind::Archive);
        assert!(e.navigable(), "an archive container can be entered");
    }

    #[test]
    fn directory_size_label_shows_a_count_or_a_dash() {
        let mut e = Entry {
            location: crate::Location::file("/x"),
            name: "x".into(),
            kind: EntryKind::Directory,
            flags: EntryFlags::empty(),
            size: 4096,
            allocated: None,
            modified: None,
            accessed: None,
            created: None,
            mode: None,
            uid: None,
            gid: None,
            mime: None,
            child_count: None,
            icon_key: "folder".into(),
        };
        assert_eq!(
            e.size_label(),
            "\u{2014}",
            "unknown must not look like zero"
        );
        e.child_count = Some(3);
        assert_eq!(e.size_label(), "3");
    }
}
