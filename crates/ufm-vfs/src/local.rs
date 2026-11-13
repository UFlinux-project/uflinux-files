//! The local filesystem reader, which is also the reader for every mounted
//! network share.
//!
//! Because network locations are mounted before they are browsed, this reader
//! never learns that a directory came from SMB or SFTP: it calls `readdir` and
//! `lstat` and is done. That is the whole reason the views, the thumbnailer and
//! drag-and-drop have no remote code paths.
//!
//! Two habits keep it fast on weak hardware. Metadata comes from `lstat` on the
//! `DirEntry`, never from following a symlink, so a folder of symlinks onto a
//! dead NFS server does not hang the view. And the listing is filtered and
//! sorted after the read, on the worker thread, so the main context only ever
//! sees finished vectors.

use std::collections::HashSet;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ufm_core::entry::{Entry, EntryFlags, EntryKind};
use ufm_core::error::{Error, Result};
use ufm_core::location::Location;
use ufm_core::paths;
use ufm_core::sort::{SortKey, filter_entries, sort_entries};

use crate::cancel::Cancel;
use crate::fsinfo::{FsHints, FsInfo};
use crate::{CHUNK, DirReader, ListingSink};

/// Extensions that the views treat as enterable containers. The archive reader
/// decides what it can actually open; this list only decides what gets a
/// folder-with-a-badge icon and a double-click that navigates instead of launching.
const ARCHIVE_EXTENSIONS: &[&str] = &[
    "zip", "tar", "tgz", "bz2", "tbz2", "xz", "txz", "gz", "zst", "7z", "rar", "jar", "apk", "rpm",
    "deb", "odt", "ods", "odp", "epub", "docx", "xlsx", "pptx", "apkapp", "war", "ear", "whl",
];

/// Reads a real directory. Cheap to construct and share; the only state is the
/// filesystem-capability cache.
#[derive(Default)]
pub struct LocalReader {
    fsinfo: FsInfo,
}

impl LocalReader {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Drop cached mount capabilities, after a mount or unmount.
    pub fn clear_fs_cache(&self) {
        self.fsinfo.clear();
    }

    #[must_use]
    pub fn hints(&self, path: &Path) -> FsHints {
        self.fsinfo.hints(path)
    }

    /// Read `path` into `all`, reporting every [`CHUNK`] entries. Returns the
    /// number of entries read, before filtering.
    fn collect(
        &self,
        path: &Path,
        all: &mut Vec<Entry>,
        cancel: &Cancel,
        sink: &mut dyn ListingSink,
    ) -> Result<usize> {
        let hints = self.fsinfo.hints(path);
        // One extra stat on the directory itself. A child living on another
        // device is then free to detect as a mount point, which is how a USB
        // stick inside /run/user looks different from a folder, without a
        // second stat per entry.
        let dev = std::fs::metadata(path).map(|m| m.dev()).unwrap_or(0);
        let mut read = std::fs::read_dir(path)?;
        let mut since_report = 0usize;
        let mut total = 0usize;
        let mut buffer: Vec<usize> = Vec::with_capacity(CHUNK);

        loop {
            match read.next() {
                None => break,
                Some(Err(e)) => {
                    // A single unreadable entry — a file deleted under us, or a
                    // permission hole inside a large tree — must not throw away
                    // the listing the user is already looking at.
                    if total == 0 {
                        return Err(e.into());
                    }
                    continue;
                }
                Some(Ok(child)) => {
                    if let Some(entry) = Self::entry_of(&child, dev, &hints) {
                        all.push(entry);
                        buffer.push(all.len() - 1);
                        since_report += 1;
                        total += 1;
                    }
                }
            }
            if since_report >= CHUNK {
                cancel.ensure_active()?;
                sink.batch(&select(all, std::mem::take(&mut buffer).as_slice()));
                since_report = 0;
            }
        }

        if !buffer.is_empty() {
            sink.batch(&select(all, &buffer));
        }
        Ok(total)
    }

    /// Build one entry from a `DirEntry`, without following symlinks. An
    /// associated function rather than a method: it needs nothing from `Self`.
    fn entry_of(child: &std::fs::DirEntry, parent_dev: u64, hints: &FsHints) -> Option<Entry> {
        let name = child.file_name().to_string_lossy().into_owned();
        if name == "." || name == ".." {
            return None;
        }
        let path = child.path();
        let meta = child.metadata().ok()?;
        // Raw mode, file type bits included: a view renders `drwxr-xr-x`
        // from this and `permissions()` alone would drop the type.
        let mode = meta.mode();
        let is_link = meta.file_type().is_symlink();

        let kind = classify(&meta, &name, parent_dev, is_link);
        let mut flags = EntryFlags::empty();
        if name.starts_with('.') {
            flags |= EntryFlags::HIDDEN;
        }
        if is_link {
            flags |= EntryFlags::SYMLINK;
            // Only a symlink costs an extra stat, and only to find out whether it
            // is broken, which the view badges in red.
            if std::fs::metadata(&path).is_err() {
                flags |= EntryFlags::BROKEN;
            }
        }
        if !hints.read_only && writable(&meta) {
            flags |= EntryFlags::WRITABLE;
        }
        if !hints.trash {
            flags |= EntryFlags::NO_TRASH;
        }

        let mime = mime_for(&meta, &name, kind);
        let icon_key = match kind {
            EntryKind::Directory | EntryKind::MountPoint => "folder".to_string(),
            EntryKind::Archive => "package-x-generic".to_string(),
            _ => mime.clone().unwrap_or_else(|| kind.icon_name().to_string()),
        };

        Some(Entry {
            location: Location::file(path),
            name,
            kind,
            flags,
            size: meta.len(),
            allocated: Some(meta.blocks().saturating_mul(512)),
            modified: system_time(meta.mtime(), meta.mtime_nsec()),
            accessed: system_time(meta.atime(), meta.atime_nsec()),
            created: None,
            mode: Some(mode),
            uid: Some(meta.uid()),
            gid: Some(meta.gid()),
            mime,
            child_count: None,
            icon_key,
        })
    }
}

impl DirReader for LocalReader {
    fn accepts(&self, location: &Location) -> bool {
        location.as_path().is_some()
    }

    fn read_streaming(
        &self,
        location: &Location,
        key: SortKey,
        show_hidden: bool,
        query: Option<&str>,
        cancel: &Cancel,
        sink: &mut dyn ListingSink,
    ) -> Result<usize> {
        let path = location
            .as_path()
            .ok_or_else(|| Error::InvalidLocation(format!("{location}: not a local path")))?;
        cancel.ensure_active()?;

        let mut all = Vec::new();
        let total = self.collect(path, &mut all, cancel, sink)?;
        if total == 0 && all.is_empty() {
            sink.sorted(&[]);
            return Ok(0);
        }

        filter_entries(&mut all, show_hidden, query);
        sort_entries(&mut all, key);
        let count = all.len();
        sink.sorted(&all);
        Ok(count)
    }

    fn metadata(&self, location: &Location) -> Result<Entry> {
        let path = location
            .as_path()
            .ok_or_else(|| Error::InvalidLocation(format!("{location}: not a local path")))?;
        let meta = std::fs::symlink_metadata(path)?;
        let name = location
            .file_name()
            .map_or_else(|| path_root_label(path), str::to_string);
        let is_link = meta.file_type().is_symlink();
        let parent_dev = path
            .parent()
            .and_then(|p| std::fs::metadata(p).ok())
            .map_or_else(|| meta.dev(), |m| m.dev());
        let kind = classify(&meta, &name, parent_dev, is_link);
        let hints = self.fsinfo.hints(path);
        let mode = meta.mode();

        let mut flags = EntryFlags::empty();
        if name.starts_with('.') {
            flags |= EntryFlags::HIDDEN;
        }
        if is_link {
            flags |= EntryFlags::SYMLINK;
            if std::fs::metadata(path).is_err() {
                flags |= EntryFlags::BROKEN;
            }
        }
        if !hints.read_only && writable(&meta) {
            flags |= EntryFlags::WRITABLE;
        }
        if !hints.trash {
            flags |= EntryFlags::NO_TRASH;
        }

        let mime = mime_for(&meta, &name, kind);
        let icon_key = match kind {
            EntryKind::Directory | EntryKind::MountPoint => "folder".to_string(),
            EntryKind::Archive => "package-x-generic".to_string(),
            _ => mime.clone().unwrap_or_else(|| kind.icon_name().to_string()),
        };

        Ok(Entry {
            location: location.clone(),
            name,
            kind,
            flags,
            size: meta.len(),
            allocated: Some(meta.blocks().saturating_mul(512)),
            modified: system_time(meta.mtime(), meta.mtime_nsec()),
            accessed: system_time(meta.atime(), meta.atime_nsec()),
            created: None,
            mode: Some(mode),
            uid: Some(meta.uid()),
            gid: Some(meta.gid()),
            mime,
            child_count: if kind.is_dir() {
                count_children(path)
            } else {
                None
            },
            icon_key,
        })
    }
}

/// Entries at `indices`, copied out so a batch can be handed over while the
/// source vector stays owned by the reader.
fn select(all: &[Entry], indices: &[usize]) -> Vec<Entry> {
    indices
        .iter()
        .filter_map(|i| all.get(*i))
        .cloned()
        .collect()
}

/// Map metadata to a kind. A directory on a different device than its parent is
/// shown as a mount point, which is what makes a USB stick look different from a
/// folder — and costs nothing, because the parent's device came from the read.
fn classify(meta: &std::fs::Metadata, name: &str, parent_dev: u64, is_link: bool) -> EntryKind {
    let file_type = meta.file_type();
    if file_type.is_dir() {
        return if meta.dev() == parent_dev {
            EntryKind::Directory
        } else {
            EntryKind::MountPoint
        };
    }
    if file_type.is_file() {
        return if is_archive_name(name) && !is_link {
            EntryKind::Archive
        } else {
            EntryKind::File
        };
    }
    if file_type.is_block_device() {
        return EntryKind::BlockDevice;
    }
    if file_type.is_char_device() {
        return EntryKind::CharDevice;
    }
    if file_type.is_fifo() {
        return EntryKind::Fifo;
    }
    if file_type.is_socket() {
        return EntryKind::Socket;
    }
    EntryKind::File
}

/// Whether the effective user may create or remove entries here. The group bit
/// only counts when the gid matches, which is a comparison cheap enough to do
/// per entry and avoids offering Delete on a directory that is group-writable by
/// a group we are not in.
#[must_use]
fn writable(meta: &std::fs::Metadata) -> bool {
    let mode = meta.permissions().mode();
    let uid = paths::uid();
    if meta.uid() == uid {
        return mode & 0o0200 != 0;
    }
    if meta.gid() == gid() {
        return mode & 0o0020 != 0;
    }
    mode & 0o0002 != 0
}

#[must_use]
#[allow(unsafe_code)]
fn gid() -> u32 {
    // SAFETY: getegid takes no arguments and is always valid.
    unsafe { libc::getegid() }
}

#[must_use]
pub fn is_archive_name(name: &str) -> bool {
    Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|ext| ARCHIVE_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str()))
}

/// Extension-based MIME type. Directories have no MIME type; the icon name is
/// what the view keys on.
fn mime_for(meta: &std::fs::Metadata, name: &str, kind: EntryKind) -> Option<String> {
    if !meta.is_file() || kind == EntryKind::Archive {
        return None;
    }
    mime_guess::from_path(name)
        .first()
        .map(|m| m.essence_str().to_string())
}

fn system_time(secs: i64, nsecs: i64) -> Option<SystemTime> {
    if secs < 0 {
        return None;
    }
    let secs = u64::try_from(secs).ok()?;
    let nsecs = u32::try_from(nsecs).unwrap_or(0);
    Some(UNIX_EPOCH + Duration::new(secs, nsecs))
}

/// The name to show for `/`, which has no file name.
fn path_root_label(path: &Path) -> String {
    let text = path.to_string_lossy();
    text.trim_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or("/")
        .to_string()
}

/// Entry count for a single directory, for the properties dialog. Bounded so a
/// directory with a million files does not turn a properties dialog into a
/// recursive walk.
fn count_children(path: &Path) -> Option<u32> {
    const CAP: u32 = 100_000;
    let mut count = 0u32;
    let Ok(entries) = std::fs::read_dir(path) else {
        return None;
    };
    for entry in entries.flatten() {
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        if name == "." || name == ".." {
            continue;
        }
        count += 1;
        if count >= CAP {
            return Some(CAP);
        }
    }
    Some(count)
}

/// Which directories the reader has recently served, so the watcher knows what
/// to watch and the cache knows what to drop first. Kept small on purpose: the
/// useful set is the visible column chain plus the previous directory.
#[derive(Default)]
pub struct Recent {
    paths: Mutex<Vec<PathBuf>>,
}

impl Recent {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn remember(&self, path: &Path) {
        let mut guard = self
            .paths
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.retain(|p| p != path);
        guard.push(path.to_path_buf());
        if guard.len() > 12 {
            let excess = guard.len() - 12;
            guard.drain(0..excess);
        }
    }

    #[must_use]
    pub fn paths(&self) -> Vec<PathBuf> {
        let guard = self
            .paths
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.clone()
    }

    #[must_use]
    pub fn contains(&self, path: &Path) -> bool {
        let guard = self
            .paths
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.iter().any(|p| p == path)
    }

    pub fn forget(&self, path: &Path) {
        let mut guard = self
            .paths
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.retain(|p| p != path);
    }

    #[must_use]
    pub fn as_set(&self) -> HashSet<PathBuf> {
        self.paths().into_iter().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// A directory with a known mix of entries, in a temp dir unique per test.
    fn fixture(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ufm-local-{}-{tag}", paths::uid()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("sub")).unwrap();
        fs::create_dir_all(dir.join(".hidden-dir")).unwrap();
        fs::write(dir.join("a.txt"), b"hello").unwrap();
        fs::write(dir.join("b.zip"), b"PK\x03\x04junk").unwrap();
        fs::write(dir.join(".dotfile"), b"").unwrap();
        fs::write(dir.join("IMG_2.jpg"), b"").unwrap();
        fs::write(dir.join("IMG_10.jpg"), b"").unwrap();
        std::os::unix::fs::symlink("a.txt", dir.join("link")).unwrap();
        std::os::unix::fs::symlink("nowhere", dir.join("broken")).unwrap();
        dir
    }

    fn names(entries: &[Entry]) -> Vec<&str> {
        entries.iter().map(|e| e.name.as_str()).collect()
    }

    fn read(
        reader: &LocalReader,
        dir: &Path,
        show_hidden: bool,
        query: Option<&str>,
    ) -> Vec<Entry> {
        let key = SortKey::default();
        crate::read_all(reader, &Location::file(dir), key, show_hidden, query).unwrap()
    }

    #[test]
    fn a_directory_lists_every_entry() {
        let dir = fixture("list");
        let reader = LocalReader::new();
        let entries = read(&reader, &dir, true, None);
        let mut got = names(&entries);
        got.sort_unstable();
        assert_eq!(
            got,
            [
                // Plain byte order, which is what `sort_unstable` on the names
                // gives: `b.zip` precedes `broken` because `.` is below `r`.
                ".dotfile",
                ".hidden-dir",
                "IMG_10.jpg",
                "IMG_2.jpg",
                "a.txt",
                "b.zip",
                "broken",
                "link",
                "sub"
            ]
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn hidden_entries_are_filtered_by_default() {
        let dir = fixture("hidden");
        let reader = LocalReader::new();
        let entries = read(&reader, &dir, false, None);
        assert!(!entries.iter().any(|e| e.name.starts_with('.')));
        assert!(entries.iter().all(|e| !e.hidden()));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn folders_come_first_and_numbers_sort_naturally() {
        let dir = fixture("sort");
        let reader = LocalReader::new();
        let entries = read(&reader, &dir, false, None);
        let first = &entries[0];
        assert!(first.navigable(), "sub should lead, got {}", first.name);
        let images: Vec<&str> = entries
            .iter()
            .map(|e| e.name.as_str())
            .filter(|n| n.starts_with("IMG_"))
            .collect();
        assert_eq!(images, ["IMG_2.jpg", "IMG_10.jpg"]);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_query_filters_on_the_name() {
        let dir = fixture("query");
        let reader = LocalReader::new();
        let entries = read(&reader, &dir, false, Some("img"));
        assert_eq!(entries.len(), 2, "case-insensitive substring match");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn kinds_flags_and_mime_are_computed_during_the_read() {
        let dir = fixture("kinds");
        let reader = LocalReader::new();
        let entries = read(&reader, &dir, true, None);
        let find = |n: &str| entries.iter().find(|e| e.name == n).unwrap();

        assert_eq!(find("sub").kind, EntryKind::Directory);
        assert_eq!(find("sub").icon_key, "folder");
        assert_eq!(find("b.zip").kind, EntryKind::Archive);
        assert_eq!(find("b.zip").icon_key, "package-x-generic");
        assert_eq!(find("a.txt").kind, EntryKind::File);
        assert_eq!(find("a.txt").mime.as_deref(), Some("text/plain"));
        assert_eq!(find("a.txt").size, 5);
        assert!(find("a.txt").flags.contains(EntryFlags::WRITABLE));
        assert!(find("link").flags.contains(EntryFlags::SYMLINK));
        assert!(!find("link").flags.contains(EntryFlags::BROKEN));
        assert!(find("broken").flags.contains(EntryFlags::BROKEN));
        assert!(find(".dotfile").flags.contains(EntryFlags::HIDDEN));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn chunks_are_reported_as_the_read_progresses() {
        // Declared before any statement: a sink declared mid-function reads as if
        // it only existed from that point on.
        #[derive(Default)]
        struct Counter {
            batches: Vec<usize>,
            sorted_len: Option<usize>,
        }

        impl ListingSink for Counter {
            fn batch(&mut self, entries: &[Entry]) {
                self.batches.push(entries.len());
            }
            fn sorted(&mut self, entries: &[Entry]) {
                self.sorted_len = Some(entries.len());
            }
        }

        let dir = std::env::temp_dir().join(format!("ufm-local-chunk-{}", paths::uid()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        for i in 0..(CHUNK * 2 + 7) {
            fs::write(dir.join(format!("f{i}")), b"x").unwrap();
        }

        let reader = LocalReader::new();
        let mut counter = Counter::default();
        let total = reader
            .read_streaming(
                &Location::file(&dir),
                SortKey::default(),
                true,
                None,
                &Cancel::default(),
                &mut counter,
            )
            .unwrap();

        assert_eq!(total, CHUNK * 2 + 7);
        assert_eq!(counter.batches.len(), 3, "two full chunks plus a remainder");
        assert_eq!(counter.batches[0], CHUNK);
        assert_eq!(counter.sorted_len, Some(total));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_cancelled_read_reports_no_listing() {
        #[derive(Default)]
        struct Quiet {
            calls: usize,
        }

        impl ListingSink for Quiet {
            fn batch(&mut self, _entries: &[Entry]) {
                self.calls += 1;
            }
            fn sorted(&mut self, _entries: &[Entry]) {
                self.calls += 100;
            }
        }

        let dir = fixture("cancel");
        let reader = LocalReader::new();
        let cancel = Cancel::default();
        cancel.cancel();

        let mut quiet = Quiet::default();
        let err = reader
            .read_streaming(
                &Location::file(&dir),
                SortKey::default(),
                true,
                None,
                &cancel,
                &mut quiet,
            )
            .unwrap_err();
        assert!(err.is_cancelled(), "{err}");
        assert_eq!(
            quiet.calls % 100,
            0,
            "sorted must not run after cancellation"
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_missing_directory_is_an_io_error() {
        let reader = LocalReader::new();
        let err = crate::read_all(
            &reader,
            &Location::file("/definitely-not-a-directory-ufm"),
            SortKey::default(),
            true,
            None,
        )
        .unwrap_err();
        assert!(matches!(err, Error::Io(_)), "{err}");
    }

    #[test]
    fn metadata_describes_one_entry() {
        let dir = fixture("meta");
        let reader = LocalReader::new();
        let entry = reader.metadata(&Location::file(dir.join("a.txt"))).unwrap();
        assert_eq!(entry.name, "a.txt");
        assert_eq!(entry.size, 5);
        // The mode is reported raw, so a view can render the permission string
        // itself rather than a lossy "writable" boolean.
        let mode = entry.mode.expect("a regular file has a mode");
        assert_eq!(mode & 0o170_000, 0o100_000, "S_IFREG survives: {mode:#o}");
        assert_eq!(
            mode & 0o022,
            fs::metadata(dir.join("a.txt")).unwrap().mode() & 0o022
        );

        let dir_entry = reader.metadata(&Location::file(&dir)).unwrap();
        assert!(dir_entry.navigable());
        assert!(dir_entry.child_count.unwrap() >= 7);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_archive_name_is_recognised_case_insensitively() {
        assert!(is_archive_name("backup.TAR.GZ"));
        assert!(is_archive_name("game.7z"));
        assert!(!is_archive_name("notes.txt"));
        assert!(!is_archive_name("no-extension"));
    }

    #[test]
    fn recent_directories_are_bounded_and_deduplicated() {
        let recent = Recent::new();
        for i in 0..20 {
            recent.remember(Path::new(&format!("/d{i}")));
        }
        recent.remember(Path::new("/d19"));
        let paths = recent.paths();
        assert_eq!(paths.len(), 12);
        assert_eq!(paths.last().unwrap(), &PathBuf::from("/d19"));
        assert!(!recent.contains(Path::new("/d0")));
        recent.forget(Path::new("/d19"));
        assert!(!recent.contains(Path::new("/d19")));
    }
}
