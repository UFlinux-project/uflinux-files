//! Reading directories.
//!
//! The contract every reader honours is what keeps navigation responsive: a
//! listing is produced on a worker thread, delivered in chunks so the first
//! screen paints before the last entry has been stat'ed, and abandoned when the
//! user navigates away. A folder with 20 000 entries therefore shows something
//! within a few frames instead of freezing the window until `readdir` ends.
//!
//! Sorting needs every entry, so it happens once at the end, off the main
//! thread, and the view is told to reorder in a single pass. That is why
//! [`ListingSink`] has two callbacks rather than one.
//!
//! Readers are chosen by [`Readers::for_location`]. The local reader handles
//! every mounted filesystem; the archive reader lives in `ufm-archive` and is
//! handed in here, which keeps this crate free of archive code.

#![deny(unsafe_code)]

pub mod cache;
pub mod cancel;
pub mod du;
pub mod fsinfo;
pub mod local;
pub mod watch;

pub use cache::{CachedListing, DirCache};
pub use cancel::Cancel;
pub use du::Tally;
pub use fsinfo::FsHints;
pub use local::LocalReader;

use std::path::Path;

use ufm_core::entry::Entry;
use ufm_core::error::Result;
use ufm_core::location::Location;
use ufm_core::sort::SortKey;

/// How many entries a reader accumulates before reporting a chunk.
///
/// Small enough that the first chunk arrives within a frame or two on a slow
/// disk, large enough that waking the main context per chunk stays negligible
/// when a directory has tens of thousands of entries.
pub const CHUNK: usize = 256;

/// Progress callbacks for one directory read.
///
/// Both are invoked on the thread doing the read, so the caller decides how to
/// get the entries onto the main context.
pub trait ListingSink {
    /// A chunk of newly read entries, in read order, unfiltered and unsorted.
    /// Called every [`CHUNK`] entries and once more for the remainder. A view
    /// appends these so the folder appears to fill in.
    fn batch(&mut self, entries: &[Entry]);

    /// The complete listing, filtered and sorted, exactly once, after the last
    /// [`batch`](Self::batch). The view replaces its model with this, which is
    /// one reorder rather than an insertion sort per entry.
    ///
    /// Not called when the read is cancelled.
    fn sorted(&mut self, entries: &[Entry]);
}

/// Read directories and single entries.
pub trait DirReader: Send + Sync {
    /// Whether this reader serves `location`.
    fn accepts(&self, location: &Location) -> bool;

    /// Stream a listing. The reader must check `cancel` between chunks and stop
    /// early; returning `Err(Error::Cancelled)` is the expected outcome, not a
    /// failure to report.
    fn read_streaming(
        &self,
        location: &Location,
        key: SortKey,
        show_hidden: bool,
        query: Option<&str>,
        cancel: &Cancel,
        sink: &mut dyn ListingSink,
    ) -> Result<usize>;

    /// Metadata for one entry, for the properties dialog and for refreshing a
    /// single row after an edit.
    fn metadata(&self, location: &Location) -> Result<Entry>;
}

/// The set of readers, in priority order.
///
/// A small enum rather than a `Vec<Box<dyn …>>` registry: there are exactly two
/// kinds, and the indirection would buy nothing.
pub enum Readers {
    Local(LocalReader),
    LocalAndArchive {
        local: LocalReader,
        archive: Box<dyn DirReader>,
    },
}

impl Readers {
    #[must_use]
    pub fn local_only() -> Self {
        Self::Local(LocalReader::default())
    }

    #[must_use]
    pub fn with_archive(archive: Box<dyn DirReader>) -> Self {
        Self::LocalAndArchive {
            local: LocalReader::default(),
            archive,
        }
    }

    #[must_use]
    pub fn local(&self) -> &LocalReader {
        match self {
            Self::Local(l) | Self::LocalAndArchive { local: l, .. } => l,
        }
    }

    #[must_use]
    pub fn for_location(&self, location: &Location) -> &dyn DirReader {
        match self {
            Self::Local(l) => l,
            Self::LocalAndArchive { local, archive } => {
                if archive.accepts(location) {
                    &**archive
                } else {
                    local
                }
            }
        }
    }

    pub fn read_streaming(
        &self,
        location: &Location,
        key: SortKey,
        show_hidden: bool,
        query: Option<&str>,
        cancel: &Cancel,
        sink: &mut dyn ListingSink,
    ) -> Result<usize> {
        self.for_location(location)
            .read_streaming(location, key, show_hidden, query, cancel, sink)
    }

    pub fn metadata(&self, location: &Location) -> Result<Entry> {
        self.for_location(location).metadata(location)
    }

    /// Forget what is cached about one directory: its listing, its parents' and
    /// everything below it.
    ///
    /// Always the local reader's cache: an archive listing is never cached, since
    /// a container's contents change when the file is rewritten and there is no
    /// event to notice. So this reaches the one cache there is, whichever reader
    /// would have served `location`.
    pub fn invalidate_around(&self, path: &Path) {
        self.local().invalidate_around(path);
    }

    /// Forget every cached listing, which is what a reload means: the caller wants
    /// the folder as it is now, not as the last visit remembered it.
    pub fn clear_listings(&self) {
        self.local().clear_listings();
    }

    /// Forget the cached filesystem capabilities, after a mount or an unmount.
    ///
    /// These are the two events that change what the filesystem behind a path
    /// *is*: a mount point that was an empty folder on tmpfs becomes a share with
    /// different free space and different trash support, and a detach takes it
    /// back. The whole table goes rather than the entry for one path, because a
    /// sweep can empty a directory a scan had seen as a mount point, and because
    /// mounts are rare enough that re-probing every device costs nothing next to
    /// being wrong — the sidebar's free space and the trash badge both come from
    /// here.
    pub fn clear_fs_cache(&self) {
        self.local().clear_fs_cache();
    }

    /// Entries held across all cached listings, for the memory bench.
    #[must_use]
    pub fn cached_entries(&self) -> usize {
        self.local().cached_entries()
    }
}

impl Default for Readers {
    fn default() -> Self {
        Self::local_only()
    }
}

/// Read a whole listing into one vector. Convenient for tests and for the
/// columns view, whose columns are small; the main view streams instead.
pub fn read_all<R: DirReader + ?Sized>(
    reader: &R,
    location: &Location,
    key: SortKey,
    show_hidden: bool,
    query: Option<&str>,
) -> Result<Vec<Entry>> {
    let cancel = Cancel::default();
    let mut out = Vec::new();
    let mut sink = CollectSink { out: &mut out };
    reader.read_streaming(location, key, show_hidden, query, &cancel, &mut sink)?;
    Ok(out)
}

struct CollectSink<'a> {
    out: &'a mut Vec<Entry>,
}

impl ListingSink for CollectSink<'_> {
    fn batch(&mut self, _entries: &[Entry]) {}

    fn sorted(&mut self, entries: &[Entry]) {
        self.out.clear();
        self.out.extend_from_slice(entries);
    }
}
