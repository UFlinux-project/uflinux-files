//! The reader that turns a container into a directory listing.
//!
//! This is where an archive stops being a file and becomes a folder. The backend
//! modules know how to list their members and how to stream one member's bytes;
//! this module knows how to turn a [`Location`] into a node in a table of contents
//! and a node into an [`Entry`], and how to behave like the rest of `ufm-vfs` while
//! doing it: chunks, then one filtered and sorted publish, with cancellation
//! honoured between chunks.
//!
//! Everything below the `!` in an `archive://` location is read-only, and the
//! entries say so — [`EntryFlags::READ_ONLY`] and [`EntryFlags::NO_TRASH`] on every
//! one, never [`EntryFlags::WRITABLE`]. The views key create, rename, delete and
//! drag-and-drop off those flags, which is why no view has an archive-specific
//! branch.
//!
//! One thing is better here than in a real directory: a directory's entry count is
//! already known from the table of contents, so the list view gets a real number
//! where the local reader has to count the folder or give up.

use std::fs::File;
use std::io::Read as _;
use std::path::Path;
use std::sync::Arc;

use ufm_core::entry::{Entry, EntryFlags, EntryKind};
use ufm_core::error::{Error, Result};
use ufm_core::location::Location;
use ufm_core::sort::{SortKey, filter_entries, sort_entries};

use ufm_vfs::{Cancel, DirReader, ListingSink};

use crate::Container;
use crate::bare::PlainContainer;
use crate::format::Format;
use crate::sevenz::SevenzContainer;
use crate::tar::TarContainer;
use crate::toc::{Cache, DIR_MODE, FILE_MODE, Key, Node, NodeKind, Toc};
use crate::zip::ZipContainer;

/// How many bytes of a file are read to decide what it is. Enough for every magic
/// number in [`Format`], including tar's magic at offset 257.
const HEAD: usize = 512;

/// A container that is open: its members are listed and its bytes can be read.
///
/// The cache holds one of these per archive, so opening a container is paid once
/// and every later listing is a slice of an array. The container is kept alive
/// alongside its table of contents so that one eviction frees both — which matters
/// for zip, where the open file owns the parsed central directory as well.
pub struct Opened {
    format: Format,
    toc: Arc<Toc>,
    container: Box<dyn Container>,
}

const _: fn() = || {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Opened>();
};

impl Opened {
    /// Which kind of container this turned out to be, for the properties dialog and
    /// for the read-only badge's tooltip.
    #[must_use]
    pub fn format(&self) -> Format {
        self.format
    }

    #[must_use]
    pub fn toc(&self) -> &Toc {
        &self.toc
    }

    /// The archive file this was opened from.
    #[must_use]
    pub fn path(&self) -> &Path {
        self.container.path()
    }

    /// Uncompressed bytes in the whole container, for the "this is going to take a
    /// while" warning and for extraction progress.
    #[must_use]
    pub fn total_bytes(&self) -> u64 {
        self.toc.total_bytes
    }

    /// Stream one member's bytes, decompressed, in chunks of at most
    /// [`ufm_vfs::CHUNK`]. `out` can abort the read by returning an error, which is
    /// how cancellation reaches a format whose own reading is a blocking loop.
    pub fn read_member(&self, node: usize, out: &mut dyn FnMut(&[u8]) -> Result<()>) -> Result<()> {
        self.container.read(&self.toc, node, out)
    }
}

/// Browses inside archive files.
///
/// Cheap to construct; the only state is the table-of-contents cache. Callers hand
/// one of these to [`ufm_vfs::Readers::with_archive`], which is the only way the
/// rest of the workspace reaches archives.
#[derive(Default)]
pub struct ArchiveReader {
    containers: Cache<Opened>,
}

const _: fn() = || {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<ArchiveReader>();
};

impl ArchiveReader {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// List `path`'s members, opening the container if the file has changed since it
    /// was last read.
    ///
    /// The key includes size and mtime, so a container written by something else is
    /// re-read rather than served stale, and a browse back to a folder the user just
    /// left costs nothing.
    pub fn contents(&self, path: &Path) -> Result<Arc<Opened>> {
        let key = Key::of(path)?;
        if let Some(opened) = self.containers.get(&key) {
            return Ok(opened);
        }
        let head = head_of(path)?;
        let format = Format::detect(path, &head).ok_or_else(|| {
            Error::Archive(format!(
                "{}: not an archive this build can read",
                path.display()
            ))
        })?;
        let path = path.to_path_buf();
        let container: Box<dyn Container> = match format {
            Format::Zip => Box::new(ZipContainer::new(path)?),
            Format::Tar(codec) => Box::new(TarContainer::new(path, codec)?),
            Format::Sevenz => Box::new(SevenzContainer::new(path)?),
            Format::Bare(codec) => Box::new(PlainContainer::new(path, codec)?),
        };
        let toc = container.scan()?;
        let opened = Arc::new(Opened {
            format: container.format(),
            toc,
            container,
        });
        self.containers.put(key, Arc::clone(&opened));
        Ok(opened)
    }

    /// Resolve a location inside an archive to an open container and a node.
    pub fn node_of(&self, location: &Location) -> Result<(Arc<Opened>, usize)> {
        let (container, inner) = split(location)?;
        let opened = self.contents(container)?;
        let at = opened
            .toc
            .find(inner)
            .ok_or_else(|| not_in(container, inner))?;
        Ok((opened, at))
    }

    /// Stream the bytes of the member `location` names. Used by "Open With", which
    /// materialises one file, and by the preview pane.
    pub fn stream(
        &self,
        location: &Location,
        out: &mut dyn FnMut(&[u8]) -> Result<()>,
    ) -> Result<()> {
        let (opened, at) = self.node_of(location)?;
        if opened.toc.node(at).is_some_and(|node| node.kind.is_dir()) {
            return Err(Error::Archive(format!(
                "{}: {} is a directory",
                opened.path().display(),
                label(location)
            )));
        }
        opened.read_member(at, out)
    }

    /// Forget one container, after it has been written to or deleted.
    pub fn invalidate(&self, path: &Path) {
        self.containers.invalidate(path);
    }

    /// Drop every cached container. The view calls this on a low-memory signal,
    /// where the tables of contents are the largest thing worth freeing.
    pub fn clear(&self) {
        self.containers.clear();
    }

    /// How many containers are held. For tests and for checking the memory budget.
    #[must_use]
    pub fn cached(&self) -> usize {
        self.containers.len()
    }
}

/// The container path and the inner path of an archive location.
fn split(location: &Location) -> Result<(&Path, &str)> {
    let container = location
        .archive_path()
        .ok_or_else(|| Error::InvalidLocation(format!("{location}: not inside an archive")))?;
    let inner = location
        .archive_inner()
        .ok_or_else(|| Error::InvalidLocation(format!("{location}: not inside an archive")))?;
    Ok((container, inner))
}

/// How to refer to the inside of an archive in a message: the inner path, or a
/// slash at the root, since "is not in the archive" reads oddly with nothing in it.
fn label(location: &Location) -> String {
    match location.archive_inner() {
        Some("") | None => "/".to_string(),
        Some(inner) => inner.to_string(),
    }
}

/// The error for a location that names nothing in the container. The user typed
/// this, or a bookmark points at it, so it says which archive and which member.
fn not_in(container: &Path, inner: &str) -> Error {
    Error::Archive(format!(
        "{}: {} is not in the archive",
        container.display(),
        if inner.is_empty() { "/" } else { inner }
    ))
}

/// The first bytes of a file, for [`Format::detect`].
///
/// A short read is fine: a file under 512 bytes cannot be a tar, and detection
/// treats a short buffer as "nothing matched" rather than as an error.
fn head_of(path: &Path) -> Result<Vec<u8>> {
    let mut file = File::open(path)?;
    let mut head = vec![0u8; HEAD];
    let mut filled = 0usize;
    while filled < HEAD {
        match file.read(&mut head[filled..]) {
            Ok(0) => break,
            Ok(read) => filled += read,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e.into()),
        }
    }
    head.truncate(filled);
    Ok(head)
}

/// Entries at `indices`, copied out so a batch can be handed over while the whole
/// listing stays owned by this read.
fn select(all: &[Entry], indices: &[usize]) -> Vec<Entry> {
    indices
        .iter()
        .filter_map(|i| all.get(*i))
        .cloned()
        .collect()
}

/// What a member is, in the vocabulary the views use. A symlink counts as a file:
/// its target cannot be followed inside a container, so it has no shape of its own
/// beyond the badge [`EntryFlags::SYMLINK`] draws.
fn kind_of(node: &Node) -> EntryKind {
    match node.kind {
        NodeKind::Dir => EntryKind::Directory,
        NodeKind::Symlink | NodeKind::File => {
            if ufm_vfs::local::is_archive_name(&node.name) {
                EntryKind::Archive
            } else {
                EntryKind::File
            }
        }
        // A device or a FIFO. Which of the two is not recorded separately, and the
        // difference matters only in the properties dialog, which shows the mode.
        NodeKind::Special => EntryKind::Device,
    }
}

/// Extension-based MIME type. Directories and nested containers have none: the icon
/// key is what the view keys on for those.
fn mime_for(name: &str, kind: EntryKind) -> Option<String> {
    match kind {
        EntryKind::File => mime_guess::from_path(name)
            .first()
            .map(|mime| mime.essence_str().to_string()),
        EntryKind::Directory
        | EntryKind::MountPoint
        | EntryKind::Archive
        | EntryKind::Device
        | EntryKind::Socket
        | EntryKind::Fifo
        | EntryKind::CharDevice
        | EntryKind::BlockDevice => None,
    }
}

impl DirReader for ArchiveReader {
    fn accepts(&self, location: &Location) -> bool {
        location.is_archive()
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
        let (opened, at) = self.node_of(location)?;
        cancel.ensure_active()?;
        if !opened.toc.node(at).is_some_and(|node| node.kind.is_dir()) {
            return Err(Error::Archive(format!(
                "{}: {} is not a directory",
                opened.path().display(),
                label(location)
            )));
        }

        // Everything is already in memory, so the chunks exist to keep the contract
        // rather than to wait for anything: a container with 100 000 members still
        // publishes its first chunk before this call returns. The cancellation check
        // between chunks is what stops a huge folder from being finished off after
        // the user has already navigated away.
        let mut all = Vec::new();
        let mut indices: Vec<usize> = Vec::with_capacity(ufm_vfs::CHUNK);
        let mut total = 0usize;
        for child in opened.toc.children(at) {
            let Some(node) = opened.toc.node(child) else {
                continue;
            };
            let Ok(entry) = entry_of(location, node, &opened.toc, child) else {
                continue;
            };
            all.push(entry);
            indices.push(all.len() - 1);
            total += 1;
            if indices.len() >= ufm_vfs::CHUNK {
                cancel.ensure_active()?;
                sink.batch(&select(&all, &std::mem::take(&mut indices)));
            }
        }
        if !indices.is_empty() {
            sink.batch(&select(&all, &indices));
        }
        cancel.ensure_active()?;
        if total == 0 {
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
        let (opened, at) = self.node_of(location)?;
        // The root of an archive is the container file, which is not a member and
        // has no name of its own in the table of contents. A properties dialog
        // opened on `archive:///x/a.zip!/` is asking about `a.zip`, so it is
        // described from the file rather than from a node.
        if at == Toc::ROOT {
            return container_entry(opened.path());
        }
        // An entry's location is its parent plus its name, so the parent is the
        // location this entry would have been listed in.
        let parent = location
            .parent()
            .ok_or_else(|| Error::InvalidLocation(location.to_string()))?;
        let node = opened
            .toc
            .node(at)
            .ok_or_else(|| not_in(opened.path(), location.archive_inner().unwrap_or("")))?;
        entry_of(&parent, node, &opened.toc, at)
    }
}

/// The entry for a container file, as the folder holding it would list it.
fn container_entry(path: &Path) -> Result<Entry> {
    let meta = std::fs::metadata(path)?;
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .map_or_else(|| path.display().to_string(), ToString::to_string);
    let mut flags = EntryFlags::READ_ONLY | EntryFlags::NO_TRASH;
    if name.starts_with('.') {
        flags |= EntryFlags::HIDDEN;
    }
    Ok(Entry {
        location: Location::file(path),
        name,
        kind: EntryKind::Archive,
        flags,
        size: meta.len(),
        // Allocated is about a file's own blocks on disk; for a container the
        // parent folder's listing already shows that, and inside the archive the
        // number that matters is the uncompressed total.
        allocated: None,
        modified: meta.modified().ok(),
        accessed: meta.accessed().ok(),
        created: meta.created().ok(),
        mode: Some(file_mode(&meta)),
        uid: None,
        gid: None,
        mime: None,
        child_count: None,
        icon_key: "package-x-generic".to_string(),
    })
}

/// The mode of a real file, from the platform's own bits.
#[cfg(unix)]
fn file_mode(meta: &std::fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt as _;
    meta.permissions().mode()
}

#[cfg(not(unix))]
fn file_mode(_meta: &std::fs::Metadata) -> u32 {
    FILE_MODE
}

/// One entry, from one node.
///
/// `parent` is the location the entry was listed in, so the child location is the
/// parent with the node's name appended — which is also what makes a nested
/// archive's root point at the container file inside this one.
fn entry_of(parent: &Location, node: &Node, toc: &Toc, at: usize) -> Result<Entry> {
    let location = parent.join(&node.name)?;
    let kind = kind_of(node);
    let mut flags = EntryFlags::READ_ONLY | EntryFlags::NO_TRASH;
    if node.name.starts_with('.') {
        flags |= EntryFlags::HIDDEN;
    }
    if node.kind == NodeKind::Symlink {
        flags |= EntryFlags::SYMLINK;
    }
    let mime = mime_for(&node.name, kind);
    let icon_key = match kind {
        EntryKind::Directory | EntryKind::MountPoint => "folder".to_string(),
        EntryKind::Archive => "package-x-generic".to_string(),
        _ => mime.clone().unwrap_or_else(|| kind.icon_name().to_string()),
    };
    let mode = node.mode.unwrap_or(if node.kind.is_dir() {
        DIR_MODE
    } else {
        FILE_MODE
    });
    Ok(Entry {
        location,
        name: node.name.clone(),
        kind,
        flags,
        size: if node.kind.is_dir() { 0 } else { node.size },
        // Packed size is per-format and not worth a field in every node; the
        // properties dialog shows the container's own size on disk instead.
        allocated: None,
        modified: node.modified,
        accessed: None,
        created: None,
        mode: Some(mode),
        uid: node.uid,
        gid: node.gid,
        mime,
        child_count: if node.kind.is_dir() {
            u32::try_from(toc.child_count(at)).ok()
        } else {
            None
        },
        icon_key,
    })
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;
    use std::time::{Duration, SystemTime};

    use ufm_core::Location;
    use ufm_vfs::read_all;

    use super::*;
    use crate::toc::{NO_HANDLE, NodeKind, TocBuilder};

    /// A zip written by the `zip` crate: a directory with a file and a hidden file,
    /// and a nested container at the root.
    fn fixture(dir: &Path) -> std::path::PathBuf {
        let path = dir.join("a.zip");
        let file = File::create(&path).expect("created");
        let mut writer = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default();
        writer.add_directory("docs/", options).expect("docs");
        writer.start_file("docs/notes.txt", options).expect("notes");
        writer.write_all(b"hello").expect("body");
        writer.start_file("docs/.hidden", options).expect("hidden");
        writer.write_all(b"x").expect("body");
        writer
            .start_file("inner.tar", zip::write::SimpleFileOptions::default())
            .expect("inner");
        writer.write_all(b"y").expect("body");
        writer.finish().expect("finished");
        path
    }

    fn root(path: &Path) -> Location {
        Location::archive(path, &[""; 0]).expect("root")
    }

    #[test]
    fn a_directory_inside_an_archive_lists_like_a_real_folder() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let path = fixture(dir.path());
        let reader = ArchiveReader::new();
        let location = Location::archive(&path, &["docs"]).expect("location");
        let entries =
            read_all(&reader, &location, SortKey::default(), false, None).expect("listed");
        assert_eq!(entries.len(), 1, "the hidden file is filtered out");
        let notes = &entries[0];
        assert_eq!(notes.name, "notes.txt");
        assert_eq!(notes.kind, EntryKind::File);
        assert_eq!(notes.size, 5);
        assert_eq!(notes.mime.as_deref(), Some("text/plain"));
        assert_eq!(notes.icon_key, "text/plain");
        assert_eq!(
            notes.location,
            Location::archive(&path, &["docs", "notes.txt"]).expect("child"),
            "the child location stays inside the archive"
        );
        assert!(
            notes
                .flags
                .intersects(EntryFlags::READ_ONLY | EntryFlags::NO_TRASH),
            "nothing inside an archive is writable"
        );
        assert!(!notes.flags.contains(EntryFlags::WRITABLE));
    }

    #[test]
    fn hidden_files_appear_when_asked_for_and_are_badged() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let path = fixture(dir.path());
        let reader = ArchiveReader::new();
        let location = Location::archive(&path, &["docs"]).expect("location");
        let entries = read_all(&reader, &location, SortKey::default(), true, None).expect("listed");
        assert_eq!(entries.len(), 2);
        let hidden = entries
            .iter()
            .find(|entry| entry.name == ".hidden")
            .expect("hidden listed");
        assert!(hidden.hidden());
    }

    #[test]
    fn a_nested_container_is_enterable_and_badged_as_an_archive() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let path = fixture(dir.path());
        let reader = ArchiveReader::new();
        let entries =
            read_all(&reader, &root(&path), SortKey::default(), true, None).expect("listed");
        let inner = entries
            .iter()
            .find(|entry| entry.name == "inner.tar")
            .expect("listed");
        assert_eq!(inner.kind, EntryKind::Archive);
        assert!(inner.navigable(), "double-clicking it goes in");
        assert_eq!(inner.icon_key, "package-x-generic");
        assert_eq!(inner.mime, None, "a container is keyed by icon, not MIME");
        let docs = entries
            .iter()
            .find(|entry| entry.name == "docs")
            .expect("dir");
        assert_eq!(docs.kind, EntryKind::Directory);
        assert_eq!(
            docs.child_count,
            Some(2),
            "the count comes from the table of contents, not a readdir"
        );
    }

    #[test]
    fn a_query_filters_the_listing_before_it_is_published() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let path = fixture(dir.path());
        let reader = ArchiveReader::new();
        let location = Location::archive(&path, &["docs"]).expect("location");
        let entries =
            read_all(&reader, &location, SortKey::default(), true, Some("note")).expect("listed");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "notes.txt");
    }

    #[test]
    fn chunks_arrive_before_the_sorted_publish() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let path = fixture(dir.path());
        let reader = ArchiveReader::new();
        let mut batches = Vec::new();
        let sorted = {
            let mut sink = Recorder {
                batches: &mut batches,
                sorted: None,
            };
            let count = reader
                .read_streaming(
                    &root(&path),
                    SortKey::default(),
                    true,
                    None,
                    &Cancel::default(),
                    &mut sink,
                )
                .expect("listed");
            // The root holds `docs/` and `inner.tar`; `docs`'s two files are one
            // level down and are not part of this listing.
            assert_eq!(count, 2);
            sink.sorted
        };
        assert_eq!(batches, vec![2], "one batch for two entries");
        assert_eq!(sorted, Some(2), "and one sorted publish");
    }

    #[test]
    fn a_cancelled_read_reports_cancelled_and_publishes_nothing() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let path = fixture(dir.path());
        let reader = ArchiveReader::new();
        let cancel = Cancel::default();
        cancel.cancel();
        let mut batches = Vec::new();
        let mut sink = Recorder {
            batches: &mut batches,
            sorted: None,
        };
        let error = reader
            .read_streaming(
                &root(&path),
                SortKey::default(),
                true,
                None,
                &cancel,
                &mut sink,
            )
            .unwrap_err();
        assert!(error.is_cancelled(), "{error}");
        assert!(
            sink.sorted.is_none(),
            "a cancelled read must not replace the view"
        );
        assert!(batches.is_empty(), "nor half-fill it");
    }

    #[test]
    fn a_location_that_names_nothing_says_which_archive() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let path = fixture(dir.path());
        let reader = ArchiveReader::new();
        let location = Location::archive(&path, &["nope"]).expect("location");
        let error = read_all(&reader, &location, SortKey::default(), true, None).unwrap_err();
        let text = error.to_string();
        assert!(text.contains("a.zip"), "{text}");
        assert!(text.contains("nope"), "{text}");
    }

    #[test]
    fn a_file_cannot_be_listed_as_a_directory() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let path = fixture(dir.path());
        let reader = ArchiveReader::new();
        let location = Location::archive(&path, &["docs", "notes.txt"]).expect("location");
        let error = read_all(&reader, &location, SortKey::default(), true, None).unwrap_err();
        assert!(error.to_string().contains("not a directory"), "{error}");
    }

    #[test]
    fn metadata_describes_a_member_and_points_at_its_own_location() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let path = fixture(dir.path());
        let reader = ArchiveReader::new();
        let location = Location::archive(&path, &["docs", "notes.txt"]).expect("location");
        let entry = reader.metadata(&location).expect("metadata");
        assert_eq!(entry.name, "notes.txt");
        assert_eq!(entry.location, location);
        assert_eq!(
            entry.mode,
            Some(0o100_644),
            "a zip stores no mode here, so one is synthesized"
        );
    }

    #[test]
    fn metadata_at_the_archive_root_describes_the_container() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let path = fixture(dir.path());
        let reader = ArchiveReader::new();
        let entry = reader.metadata(&root(&path)).expect("metadata");
        assert_eq!(entry.name, "a.zip");
        assert_eq!(entry.kind, EntryKind::Archive);
        assert_eq!(
            entry.location,
            Location::file(&path),
            "the root of an archive is the file itself"
        );
    }

    #[test]
    fn a_member_can_be_streamed() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let path = fixture(dir.path());
        let reader = ArchiveReader::new();
        let location = Location::archive(&path, &["docs", "notes.txt"]).expect("location");
        let mut body = Vec::new();
        reader
            .stream(&location, &mut |chunk| {
                body.extend_from_slice(chunk);
                Ok(())
            })
            .expect("streamed");
        assert_eq!(body, b"hello");
    }

    #[test]
    fn streaming_a_directory_is_refused() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let path = fixture(dir.path());
        let reader = ArchiveReader::new();
        let location = Location::archive(&path, &["docs"]).expect("location");
        let error = reader.stream(&location, &mut |_| Ok(())).unwrap_err();
        assert!(error.to_string().contains("is a directory"), "{error}");
    }

    #[test]
    fn a_container_is_opened_once_and_reopened_when_it_changes() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let path = fixture(dir.path());
        let reader = ArchiveReader::new();
        let location = root(&path);
        read_all(&reader, &location, SortKey::default(), true, None).expect("first");
        read_all(&reader, &location, SortKey::default(), true, None).expect("second");
        assert_eq!(reader.cached(), 1, "the second listing came from the cache");
        reader.invalidate(&path);
        assert_eq!(reader.cached(), 0);
        read_all(&reader, &location, SortKey::default(), true, None).expect("third");
        assert_eq!(reader.cached(), 1, "and it was opened again");

        // Same path, different bytes: the key carries the size, so a stale table of
        // contents must not be served for a file that was replaced.
        std::fs::write(&path, b"not an archive any more").expect("replaced");
        let error = read_all(&reader, &location, SortKey::default(), true, None).unwrap_err();
        assert!(error.to_string().contains("a.zip"), "{error}");
    }

    #[test]
    fn a_compressed_file_shows_one_member_named_after_itself() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let path = dir.path().join("notes.txt.gz");
        std::fs::write(&path, gzip(b"inflated")).expect("written");
        let reader = ArchiveReader::new();
        let entries =
            read_all(&reader, &root(&path), SortKey::default(), true, None).expect("listed");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "notes.txt");
        assert_eq!(
            entries[0].size,
            std::fs::metadata(&path).expect("stat").len(),
            "the compressed size, which is all a listing can know without inflating"
        );
        let mut body = Vec::new();
        reader
            .stream(&entries[0].location, &mut |chunk| {
                body.extend_from_slice(chunk);
                Ok(())
            })
            .expect("streamed");
        assert_eq!(body, b"inflated");
    }

    #[test]
    fn a_file_that_is_not_an_archive_is_refused_and_named() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let path = dir.path().join("plain.txt");
        std::fs::write(&path, b"just text").expect("written");
        let reader = ArchiveReader::new();
        let error = read_all(&reader, &root(&path), SortKey::default(), true, None).unwrap_err();
        assert!(error.to_string().contains("plain.txt"), "{error}");
    }

    #[test]
    fn a_mode_from_the_container_is_passed_through() {
        let toc = toc_with(
            "bin/tool",
            NodeKind::File,
            Some(0o104_755),
            Some(0),
            Some(0),
        );
        let entry = entry_of(&toc, "bin/tool").expect("entry");
        assert_eq!(entry.mode, Some(0o104_755), "setuid survives");
        assert_eq!(entry.uid, Some(0));
        assert_eq!(entry.gid, Some(0));
    }

    #[test]
    fn a_device_is_not_enterable_and_has_no_mime_type() {
        let toc = toc_with("null", NodeKind::Special, None, None, None);
        let entry = entry_of(&toc, "null").expect("entry");
        assert_eq!(entry.kind, EntryKind::Device);
        assert!(!entry.navigable());
        assert_eq!(entry.mime, None);
        assert_eq!(entry.icon_key, "drive-harddisk");
    }

    #[test]
    fn a_timestamp_survives_the_trip() {
        let mut builder = TocBuilder::with_limit(10);
        builder
            .push(
                "old.txt",
                NodeKind::File,
                1,
                Some(SystemTime::UNIX_EPOCH + Duration::from_secs(10)),
                None,
                None,
                None,
                NO_HANDLE,
            )
            .expect("pushed");
        let toc = builder.finish();
        let entry = entry_of(&toc, "old.txt").expect("entry");
        assert_eq!(
            entry.modified.map(|when| when
                .duration_since(SystemTime::UNIX_EPOCH)
                .expect("after")
                .as_secs()),
            Some(10)
        );
    }

    /// One member, in a table of contents of its own, so an entry can be built
    /// without opening a container.
    fn toc_with(
        name: &str,
        kind: NodeKind,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
    ) -> Toc {
        let mut builder = TocBuilder::with_limit(10);
        builder
            .push(name, kind, 1, None, mode, uid, gid, NO_HANDLE)
            .expect("pushed");
        builder.finish()
    }

    fn entry_of(toc: &Toc, inner: &str) -> Result<Entry> {
        let parent = Location::file("/tmp/x.7z");
        let at = toc.find(inner).expect("member");
        super::entry_of(&parent, toc.node(at).expect("node"), toc, at)
    }

    struct Recorder<'a> {
        batches: &'a mut Vec<usize>,
        sorted: Option<usize>,
    }

    impl ListingSink for Recorder<'_> {
        fn batch(&mut self, entries: &[Entry]) {
            self.batches.push(entries.len());
        }

        fn sorted(&mut self, entries: &[Entry]) {
            self.sorted = Some(entries.len());
        }
    }

    fn gzip(body: &[u8]) -> Vec<u8> {
        let mut encoder = std::process::Command::new("gzip")
            .arg("-c")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .expect("gzip started");
        encoder
            .stdin
            .take()
            .expect("stdin")
            .write_all(body)
            .expect("written");
        let output = encoder.wait_with_output().expect("gzipped");
        assert!(output.status.success());
        output.stdout
    }
}
