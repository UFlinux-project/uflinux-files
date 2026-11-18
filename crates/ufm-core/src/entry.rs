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

    /// What the entry is, in the words a column or a properties row shows.
    ///
    /// What a folder or an archive *is* says more here than its mime type does:
    /// the mime of an archive names its codec, which reads as noise in a column
    /// whose siblings all say "Folder". Anything the reader could not classify
    /// is named by its mime type, and a file with no mime at all is a `File`.
    #[must_use]
    pub fn kind_label(&self) -> String {
        match self.kind {
            EntryKind::Directory | EntryKind::MountPoint => return "Folder".to_string(),
            EntryKind::Archive => return "Archive".to_string(),
            EntryKind::Device => return "Device".to_string(),
            _ => {}
        }
        self.mime.clone().unwrap_or_else(|| "File".to_string())
    }

    /// The mode as `drwxr-xr-x`, the way `ls -l` spells it, or `None` when the
    /// filesystem has no mode bits to spell.
    ///
    /// The type character comes from the mode's own `S_IF*` bits, except for a
    /// symlink: the reader stats a link without following it, so the mode it
    /// stores is already the link's, and the flag only says which link it was.
    /// A mode carried by an archive member has no type bits at all, which is
    /// why the kind is consulted for those.
    #[must_use]
    pub fn permissions(&self) -> Option<String> {
        let mode = self.mode?;
        let mut text = String::with_capacity(10);
        text.push(type_letter(mode, self.kind));
        for part in (0..3).rev() {
            let bits = (mode >> (part * 3)) & 0o7;
            text.push(if bits & 0o4 != 0 { 'r' } else { '-' });
            text.push(if bits & 0o2 != 0 { 'w' } else { '-' });
            text.push(if bits & 0o1 != 0 { 'x' } else { '-' });
        }
        Some(text)
    }

    /// `name (1000)` when the account database knows the uid, `1000` when it
    /// does not — a file from a NFS export whose ids belong to another machine
    /// has a number and no name, and showing only the number would look like a
    /// bug rather than like what it is.
    #[must_use]
    pub fn owner(&self) -> Option<String> {
        let uid = self.uid?;
        Some(match owner_name(uid) {
            Some(name) => format!("{name} ({uid})"),
            None => format!("{uid}"),
        })
    }

    /// `group (100)` for a gid, shaped like [`owner`](Self::owner).
    #[must_use]
    pub fn group(&self) -> Option<String> {
        let gid = self.gid?;
        Some(match group_name(gid) {
            Some(name) => format!("{name} ({gid})"),
            None => format!("{gid}"),
        })
    }
}

/// The first character of a permissions string, from the `S_IF*` bits.
#[must_use]
const fn type_letter(mode: u32, kind: EntryKind) -> char {
    match mode & 0o170_000 {
        0o040_000 => 'd',
        0o120_000 => 'l',
        0o020_000 => 'c',
        0o060_000 => 'b',
        0o010_000 => 'p',
        0o140_000 => 's',
        // No type bits, which is what an archive's mode field looks like: fall
        // back to what the reader classified the entry as.
        _ => match kind {
            EntryKind::Directory | EntryKind::MountPoint => 'd',
            EntryKind::CharDevice | EntryKind::BlockDevice | EntryKind::Device => 'c',
            EntryKind::Socket => 's',
            EntryKind::Fifo => 'p',
            EntryKind::File | EntryKind::Archive => '-',
        },
    }
}

/// How much text a reentrant lookup gets for one name. What `getpwuid_r`
/// documents as enough; a lookup that needs more reports `ERANGE` and is
/// treated as having no name, which is what a nameless account looks like
/// anyway.
const NAME_BUFFER: usize = 1024;

/// The account name for a uid, or `None` when the passwd database has no such
/// account. `getpwuid_r` rather than `getpwuid`: the properties dialog reads
/// this on the main thread, and the reentrant form is the one that is safe
/// while any other thread might be looking up a group at the same time.
#[must_use]
#[allow(unsafe_code)]
fn owner_name(uid: u32) -> Option<String> {
    let mut entry = std::mem::MaybeUninit::<libc::passwd>::zeroed();
    let mut buffer = [0_u8; NAME_BUFFER];
    let mut result: *mut libc::passwd = std::ptr::null_mut();
    // SAFETY: `entry` and `buffer` are storage owned by this call and outlive
    // it, and `result` is only read below when the call returned success.
    let found = unsafe {
        libc::getpwuid_r(
            uid,
            entry.as_mut_ptr(),
            buffer.as_mut_ptr().cast(),
            NAME_BUFFER,
            &raw mut result,
        )
    };
    if found != 0 || result.is_null() {
        return None;
    }
    // SAFETY: a non-null `result` means the entry was filled, and `pw_name`
    // then points at a NUL-terminated string inside `buffer`.
    let name = unsafe { (*result).pw_name };
    text_of(name)
}

/// The group name for a gid. See [`owner_name`] for why the reentrant call.
#[must_use]
#[allow(unsafe_code)]
fn group_name(gid: u32) -> Option<String> {
    let mut entry = std::mem::MaybeUninit::<libc::group>::zeroed();
    let mut buffer = [0_u8; NAME_BUFFER];
    let mut result: *mut libc::group = std::ptr::null_mut();
    // SAFETY: as in `owner_name`.
    let found = unsafe {
        libc::getgrgid_r(
            gid,
            entry.as_mut_ptr(),
            buffer.as_mut_ptr().cast(),
            NAME_BUFFER,
            &raw mut result,
        )
    };
    if found != 0 || result.is_null() {
        return None;
    }
    // SAFETY: as in `owner_name`.
    let name = unsafe { (*result).gr_name };
    text_of(name)
}

/// A C string as owned Rust text, `None` when it is absent or not UTF-8. A
/// name in some other encoding is reported as no name rather than replaced, so
/// the dialog shows the numeric id it also shows for an unknown account.
#[must_use]
#[allow(unsafe_code)]
fn text_of(name: *const libc::c_char) -> Option<String> {
    if name.is_null() {
        return None;
    }
    // SAFETY: the caller guarantees a NUL-terminated string that outlives this
    // call, which is long enough for `to_str` to copy from.
    let text = unsafe { std::ffi::CStr::from_ptr(name) };
    text.to_str().ok().map(str::to_string)
}

/// Decimal SI units for byte sizes. One kilobyte is 1,000 bytes.
#[must_use]
pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 7] = ["B", "kB", "MB", "GB", "TB", "PB", "EB"];
    if bytes < 1_000 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1_000.0 && unit < UNITS.len() - 1 {
        value /= 1_000.0;
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
    fn sizes_use_decimal_multiples() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(999), "999 B");
        assert_eq!(human_size(1_000), "1.0 kB");
        assert_eq!(human_size(1_500), "1.5 kB");
        assert_eq!(human_size(1_000_000), "1.0 MB");
        assert_eq!(human_size(1_000_000_000), "1.0 GB");
        assert_eq!(human_size(1_610_612_736), "1.6 GB");
        assert_eq!(human_size(16 * 1024 * 1024 * 1024), "17 GB");
        assert_eq!(human_size(u64::MAX), "18 EB");
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

    fn metadata(kind: EntryKind, mode: Option<u32>) -> Entry {
        Entry {
            location: crate::Location::file("/x"),
            name: "x".into(),
            kind,
            flags: EntryFlags::empty(),
            size: 0,
            allocated: None,
            modified: None,
            accessed: None,
            created: None,
            mode,
            uid: None,
            gid: None,
            mime: None,
            child_count: None,
            icon_key: kind.icon_name().into(),
        }
    }

    #[test]
    fn a_folder_is_a_folder_and_a_file_is_named_by_its_mime() {
        assert_eq!(metadata(EntryKind::Directory, None).kind_label(), "Folder");
        assert_eq!(metadata(EntryKind::MountPoint, None).kind_label(), "Folder");
        let mut archive = metadata(EntryKind::Archive, None);
        archive.mime = Some("application/zip".into());
        assert_eq!(archive.kind_label(), "Archive", "mime must not override");
        let mut text = metadata(EntryKind::File, None);
        text.mime = Some("text/plain".into());
        assert_eq!(text.kind_label(), "text/plain");
        assert_eq!(metadata(EntryKind::File, None).kind_label(), "File");
    }

    #[test]
    fn permissions_read_like_ls() {
        // 0o040755: a directory that everyone can read and enter.
        assert_eq!(
            metadata(EntryKind::Directory, Some(0o040_755)).permissions(),
            Some("drwxr-xr-x".into())
        );
        assert_eq!(
            metadata(EntryKind::File, Some(0o100_644)).permissions(),
            Some("-rw-r--r--".into())
        );
        assert_eq!(
            metadata(EntryKind::File, Some(0o100_000)).permissions(),
            Some("----------".into())
        );
        assert_eq!(
            metadata(EntryKind::File, Some(0o100_777)).permissions(),
            Some("-rwxrwxrwx".into())
        );
        assert_eq!(metadata(EntryKind::File, None).permissions(), None);
    }

    #[test]
    fn a_type_character_comes_from_the_mode_when_it_has_one() {
        // A socket whose mode carries no type bits: the kind has to say.
        assert_eq!(
            metadata(EntryKind::Socket, Some(0o755)).permissions(),
            Some("srwxr-xr-x".into())
        );
        // And when both agree, the mode wins — a directory bit on an entry the
        // reader filed as a file is a directory as far as `ls` is concerned.
        assert_eq!(
            metadata(EntryKind::File, Some(0o040_700)).permissions(),
            Some("drwx------".into())
        );
    }

    #[test]
    fn an_owner_without_an_account_shows_only_its_number() {
        let mut e = metadata(EntryKind::File, None);
        // A uid no machine's passwd file is likely to know.
        e.uid = Some(4_000_123_456);
        e.gid = Some(4_000_123_457);
        assert_eq!(e.owner().as_deref(), Some("4000123456"));
        assert_eq!(e.group().as_deref(), Some("4000123457"));
        // The real account of whoever runs the tests has a name, and the number
        // is shown beside it so a report can quote either.
        e.uid = Some(crate::paths::uid());
        let owner = e.owner().expect("the current uid is known");
        assert!(
            owner.ends_with(&format!("({})", crate::paths::uid())),
            "{owner}"
        );
        assert!(owner.len() > format!("({})", crate::paths::uid()).len());
        e.uid = None;
        assert_eq!(e.owner(), None);
    }
}
