//! A small cache of directory listings.
//!
//! The cache exists for one interaction: going Back. Without it, pressing Back
//! re-reads the directory, re-stats every entry and re-sorts, so the button that
//! should be instant instead costs a full read — and on a mounted network share,
//! a full round trip. With it, Back repaints from memory.
//!
//! The budget is deliberately tight and the eviction is a plain LRU: 64
//! directories and 20 000 entries total. A listing entry is a name, a location
//! and a dozen fixed fields, roughly 300 bytes, so the ceiling is about 6 MB of
//! heap — small enough that the file manager and the desktop icon provider can
//! both be resident on a 2 GB machine without either noticing.
//!
//! Correctness comes from invalidation, not from short lifetimes: a watch event
//! for a directory drops its entry, so a stale listing is never shown for longer
//! than the debounce interval.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use ufm_core::entry::Entry;
use ufm_core::location::Location;
use ufm_core::sort::SortKey;

/// How many directory listings to keep. Covers the visible column chain, the
/// tab history and a few folders the user bounced in and out of.
const MAX_DIRECTORIES: usize = 64;

/// How many entries to keep in total, across all listings. The binding limit on
/// a machine with little memory; see the module comment for the arithmetic.
const MAX_ENTRIES: usize = 20_000;

/// How long a listing may be reused without a change notification. A watch can
/// be lost — a device disappears, the inotify queue overflows — and then the
/// worst outcome is a listing that is this old, rather than a folder that never
/// updates.
const MAX_AGE: Duration = Duration::from_secs(30);

/// A listing that was served once and can be painted again without a read.
#[derive(Debug, Clone)]
pub struct CachedListing {
    /// Filtered and sorted exactly as it was when first shown, so a repaint is
    /// byte-identical and no re-sort is needed on the way out.
    pub entries: Vec<Entry>,
    /// The key this listing was produced for. A cache hit requires an exact
    /// match, because a listing sorted by size is not a listing sorted by name.
    pub key: SortKey,
    pub show_hidden: bool,
    /// The filter text, if any. Kept so a view can tell whether the cache answers
    /// the current filter or only a previous one.
    pub query: Option<String>,
    pub read_at: Instant,
}

impl CachedListing {
    #[must_use]
    pub fn age(&self) -> Duration {
        self.read_at.elapsed()
    }

    #[must_use]
    pub fn is_fresh(&self) -> bool {
        self.age() <= MAX_AGE
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Whether this listing answers exactly the request `key`/`show_hidden`/`query`
    /// describes. Anything less and the view would show the wrong order or the
    /// wrong rows, which is worse than a cache miss.
    #[must_use]
    pub fn answers(&self, key: SortKey, show_hidden: bool, query: Option<&str>) -> bool {
        self.key == key
            && self.show_hidden == show_hidden
            && self.query.as_deref() == query
            && self.is_fresh()
    }
}

/// Lookup key: a directory plus the view state that changes its listing.
///
/// The filter is part of the key rather than applied on top, because a type-ahead
/// filter over a large folder is the case where re-reading is most expensive.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct Key {
    path: PathBuf,
    field: u8,
    descending: bool,
    folders_first: bool,
    show_hidden: bool,
    query: Option<String>,
}

impl Key {
    fn new(
        location: &Location,
        key: SortKey,
        show_hidden: bool,
        query: Option<&str>,
    ) -> Option<Self> {
        // Only a real directory can be a key. `as_path` reports the *container*
        // for an archive location, which would make every directory inside one
        // archive collide on the same path, and an archive's contents change
        // when the file is rewritten without any inotify event to notice.
        if !location.is_real() {
            return None;
        }
        Some(Self {
            path: location.as_path()?.to_path_buf(),
            field: key.field as u8,
            descending: key.descending,
            folders_first: key.folders_first,
            show_hidden,
            query: query.map(str::to_string),
        })
    }
}

/// A bounded LRU of directory listings, safe to share between the worker pool
/// and the main context.
#[derive(Default)]
pub struct DirCache {
    inner: Mutex<Store>,
}

#[derive(Default)]
struct Store {
    entries: HashMap<Key, CachedListing>,
    /// Most recently used last. A `VecDeque` because both ends are touched and
    /// the table is tiny, so a linear scan beats a linked list.
    order: VecDeque<Key>,
    entry_total: usize,
}

impl DirCache {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Store a listing. `entries` is what the view was given, so it is already
    /// filtered and sorted.
    pub fn store(
        &self,
        location: &Location,
        key: SortKey,
        show_hidden: bool,
        query: Option<&str>,
        entries: Vec<Entry>,
    ) {
        let Some(cache_key) = Key::new(location, key, show_hidden, query) else {
            return; // An archive or virtual location: nothing to key on.
        };
        let count = entries.len();
        let listing = CachedListing {
            entries,
            key,
            show_hidden,
            query: query.map(str::to_string),
            read_at: Instant::now(),
        };
        let mut store = self.lock();
        if let Some(old) = store.entries.insert(cache_key.clone(), listing) {
            store.entry_total = store.entry_total.saturating_sub(old.entries.len());
        }
        store.touch(&cache_key);
        store.entry_total += count;
        store.evict();
    }

    /// The listing for this exact request, if one is held and still fresh.
    #[must_use]
    pub fn get(
        &self,
        location: &Location,
        key: SortKey,
        show_hidden: bool,
        query: Option<&str>,
    ) -> Option<CachedListing> {
        let cache_key = Key::new(location, key, show_hidden, query)?;
        let mut store = self.lock();
        let listing = store.entries.get(&cache_key)?;
        if !listing.is_fresh() {
            let total = listing.entries.len();
            store.entries.remove(&cache_key);
            store.order.retain(|k| k != &cache_key);
            store.entry_total = store.entry_total.saturating_sub(total);
            return None;
        }
        let found = listing.clone();
        store.touch(&cache_key);
        Some(found)
    }

    /// Drop everything for one directory, after a watch event or a write.
    pub fn invalidate(&self, path: &Path) {
        self.remove_if(|k| k.path == path);
    }

    /// Drop every listing under `path` too, for a rename or a recursive delete,
    /// where children of the moved tree are now wrong as well.
    pub fn invalidate_subtree(&self, path: &Path) {
        self.remove_if(|k| k.path.starts_with(path));
    }

    fn remove_if(&self, reject: impl Fn(&Key) -> bool) {
        let mut store = self.lock();
        let doomed: Vec<Key> = store
            .entries
            .keys()
            .filter(|k| reject(k))
            .cloned()
            .collect();
        for key in doomed {
            if let Some(old) = store.entries.remove(&key) {
                store.entry_total = store.entry_total.saturating_sub(old.entries.len());
            }
            store.order.retain(|k| k != &key);
        }
    }

    pub fn clear(&self) {
        let mut store = self.lock();
        store.entries.clear();
        store.order.clear();
        store.entry_total = 0;
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.lock().entries.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Entries held across all listings, for the memory-budget test.
    #[must_use]
    pub fn entry_total(&self) -> usize {
        self.lock().entry_total
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Store> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl Store {
    fn touch(&mut self, key: &Key) {
        self.order.retain(|k| k != key);
        self.order.push_back(key.clone());
    }

    /// Enforce both budgets, oldest first. The entry budget is checked first
    /// because it is the one that protects a low-memory machine.
    fn evict(&mut self) {
        while self.entries.len() > MAX_DIRECTORIES || self.entry_total > MAX_ENTRIES {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            if let Some(old) = self.entries.remove(&oldest) {
                self.entry_total = self.entry_total.saturating_sub(old.entries.len());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use ufm_core::entry::{EntryFlags, EntryKind};

    fn entry(dir: &Path, name: &str) -> Entry {
        Entry {
            location: Location::file(dir.join(name)),
            name: name.to_string(),
            kind: EntryKind::File,
            flags: EntryFlags::empty(),
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
            icon_key: "text-x-generic".to_string(),
        }
    }

    fn listing(dir: &Path, names: &[&str]) -> Vec<Entry> {
        names.iter().map(|n| entry(dir, n)).collect()
    }

    fn dir_at(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("ufm-cache-{name}"))
    }

    fn hit(cache: &DirCache, dir: &Path) -> Option<CachedListing> {
        cache.get(&Location::file(dir), SortKey::default(), false, None)
    }

    #[test]
    fn a_stored_listing_is_served_back() {
        let dir = dir_at("hit");
        let cache = DirCache::new();
        cache.store(
            &Location::file(&dir),
            SortKey::default(),
            false,
            None,
            listing(&dir, &["a", "b"]),
        );

        let got = hit(&cache, &dir).expect("a hit");
        assert_eq!(got.len(), 2);
        assert_eq!(got.entries[0].name, "a");
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.entry_total(), 2);
    }

    #[test]
    fn a_different_view_state_is_a_miss() {
        let dir = dir_at("state");
        let cache = DirCache::new();
        cache.store(
            &Location::file(&dir),
            SortKey::default(),
            false,
            None,
            listing(&dir, &["a"]),
        );

        assert!(hit(&cache, &dir).is_some());
        let descending = SortKey::default().reversed();
        assert!(
            cache
                .get(&Location::file(&dir), descending, false, None)
                .is_none(),
            "a listing sorted the other way cannot answer this request"
        );
        assert!(
            cache
                .get(&Location::file(&dir), SortKey::default(), true, None)
                .is_none()
        );
        assert!(
            cache
                .get(&Location::file(&dir), SortKey::default(), false, Some("a"))
                .is_none()
        );
        assert!(
            hit(&cache, &dir).is_some(),
            "the original request still hits"
        );
    }

    #[test]
    fn an_archive_location_is_not_cached() {
        let cache = DirCache::new();
        let archive = Location::archive("/tmp/a.zip", &["dir"]).unwrap();
        cache.store(&archive, SortKey::default(), false, None, Vec::new());
        assert!(cache.is_empty(), "there is no path to key on");
    }

    #[test]
    fn invalidation_removes_every_listing_for_a_directory() {
        let dir = dir_at("invalidate");
        let cache = DirCache::new();
        cache.store(
            &Location::file(&dir),
            SortKey::default(),
            false,
            None,
            listing(&dir, &["a"]),
        );
        cache.store(
            &Location::file(&dir),
            SortKey::default(),
            false,
            Some("a"),
            listing(&dir, &["a"]),
        );
        assert_eq!(cache.len(), 2);

        cache.invalidate(&dir);
        assert!(cache.is_empty());
        assert_eq!(cache.entry_total(), 0);
    }

    #[test]
    fn a_subtree_invalidation_reaches_children() {
        let root = dir_at("sub");
        let child = root.join("inner");
        let cache = DirCache::new();
        cache.store(
            &Location::file(&root),
            SortKey::default(),
            false,
            None,
            listing(&root, &["a"]),
        );
        cache.store(
            &Location::file(&child),
            SortKey::default(),
            false,
            None,
            listing(&child, &["b"]),
        );
        cache.store(
            &Location::file(dir_at("unrelated")),
            SortKey::default(),
            false,
            None,
            listing(&dir_at("unrelated"), &["c"]),
        );

        cache.invalidate_subtree(&root);
        assert_eq!(cache.len(), 1, "only the unrelated directory survives");
        assert!(hit(&cache, &dir_at("unrelated")).is_some());
    }

    #[test]
    fn the_directory_budget_is_enforced_least_recently_first() {
        let cache = DirCache::new();
        for i in 0..=(MAX_DIRECTORIES * 2) {
            let dir = dir_at(&format!("lru-{i}"));
            cache.store(
                &Location::file(&dir),
                SortKey::default(),
                false,
                None,
                listing(&dir, &["a"]),
            );
        }
        assert!(cache.len() <= MAX_DIRECTORIES, "{} entries", cache.len());
        assert!(
            hit(&cache, &dir_at("lru-0")).is_none(),
            "the first directory written must be the first evicted"
        );
        assert!(hit(&cache, &dir_at(&format!("lru-{}", MAX_DIRECTORIES * 2))).is_some());
    }

    #[test]
    fn the_entry_budget_is_enforced() {
        let cache = DirCache::new();
        // Three directories of 9 000 entries each: over the 20 000 budget, so the
        // oldest must go even though the directory count is well under its cap.
        for i in 0..3 {
            let dir = dir_at(&format!("big-{i}"));
            let many = vec![entry(&dir, "x"); 9_000];
            cache.store(&Location::file(&dir), SortKey::default(), false, None, many);
        }
        assert!(
            cache.entry_total() <= MAX_ENTRIES,
            "{} entries held",
            cache.entry_total()
        );
        assert!(hit(&cache, &dir_at("big-0")).is_none());
        assert!(hit(&cache, &dir_at("big-2")).is_some());
    }

    #[test]
    fn storing_the_same_request_twice_does_not_double_count() {
        let dir = dir_at("twice");
        let cache = DirCache::new();
        cache.store(
            &Location::file(&dir),
            SortKey::default(),
            false,
            None,
            listing(&dir, &["a", "b", "c"]),
        );
        cache.store(
            &Location::file(&dir),
            SortKey::default(),
            false,
            None,
            listing(&dir, &["a"]),
        );
        assert_eq!(cache.len(), 1);
        assert_eq!(
            cache.entry_total(),
            1,
            "the old listing's entries were released"
        );
    }

    #[test]
    fn a_stale_listing_is_a_miss_and_is_dropped() {
        let dir = dir_at("stale");
        let cache = DirCache::new();
        cache.store(
            &Location::file(&dir),
            SortKey::default(),
            false,
            None,
            listing(&dir, &["a"]),
        );
        {
            let mut store = cache.lock();
            let only = store.entries.values_mut().next().unwrap();
            only.read_at = Instant::now()
                .checked_sub(MAX_AGE)
                .unwrap()
                .checked_sub(Duration::from_millis(1))
                .unwrap();
        }
        assert!(hit(&cache, &dir).is_none());
        assert!(
            cache.is_empty(),
            "a stale entry is removed on access, not left to rot"
        );
    }

    #[test]
    fn a_listing_reports_whether_it_answers_a_request() {
        let dir = dir_at("answers");
        let cache = DirCache::new();
        cache.store(
            &Location::file(&dir),
            SortKey::default(),
            false,
            None,
            listing(&dir, &["a"]),
        );
        let got = hit(&cache, &dir).unwrap();
        assert!(got.answers(SortKey::default(), false, None));
        assert!(!got.answers(SortKey::default(), false, Some("zzz")));
        assert!(!got.answers(SortKey::default(), true, None));
    }

    #[test]
    fn clearing_forgets_everything() {
        let dir = dir_at("clear");
        let cache = DirCache::new();
        cache.store(
            &Location::file(&dir),
            SortKey::default(),
            false,
            None,
            listing(&dir, &["a"]),
        );
        cache.clear();
        assert!(cache.is_empty());
        assert_eq!(cache.entry_total(), 0);
    }
}
