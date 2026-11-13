//! The table of contents: every member of one container, in the shape a
//! directory listing needs.
//!
//! A container is read once, front to back, and reduced to a flat array of
//! nodes grouped by parent. That single choice is what makes browsing an archive
//! as snappy as browsing a folder: listing a directory inside a 40 000-member
//! `.zip` is a slice of the array, not a scan of the container, and navigating
//! back to a parent costs nothing because the parent's children were kept
//! together.
//!
//! The array is grouped rather than tree-shaped on purpose. A `Vec<Node>` with a
//! `Vec<usize>` of children per node costs a 24-byte header plus an allocation
//! per directory, which for a large archive is both more memory and more
//! fragmentation than the archive deserves. Here each node carries a `u32`
//! parent, one counting sort groups the nodes, and a directory's children are one
//! contiguous range: 8 bytes of range and 4 bytes of index per entry, no
//! allocation per directory.
//!
//! Names are sorted by byte order inside each group, which is what lets
//! [`Toc::find`] binary-search a path instead of keeping a `HashMap` of every
//! path — the map would be the largest thing in the structure.

use std::collections::HashMap;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use ufm_core::error::{Error, Result};

/// What a member is. Anything more specific is told apart by mode and MIME type,
/// exactly as in a real directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NodeKind {
    Dir,
    File,
    Symlink,
    /// Character device, block device or FIFO: recorded so the properties dialog
    /// can say so, but never enterable.
    Special,
}

impl NodeKind {
    #[must_use]
    pub const fn is_dir(self) -> bool {
        matches!(self, Self::Dir)
    }
}

/// One member of a container. Node 0 is the root of the archive and is never a
/// real member.
#[derive(Debug, Clone)]
pub struct Node {
    /// Index of the parent node. Always less than this node's own index, because
    /// a parent is created before its child.
    pub parent: u32,
    /// The name as it appears inside the container, without separators.
    pub name: String,
    pub kind: NodeKind,
    pub size: u64,
    pub modified: Option<SystemTime>,
    /// Mode bits when the format records them. Synthesized otherwise, so the
    /// properties dialog is never blank.
    pub mode: Option<u32>,
    pub uid: Option<u32>,
    pub gid: Option<u32>,
    /// Handle the format needs to read this member's bytes: a central-directory
    /// index for `zip`. The command-line formats address a member by path and
    /// leave it at [`NO_HANDLE`].
    pub handle: u64,
}

/// Members that have no per-member handle.
pub const NO_HANDLE: u64 = u64::MAX;

/// The whole container, laid out for listing.
#[derive(Debug)]
pub struct Toc {
    nodes: Vec<Node>,
    /// Children of node `i` are `kids[ranges[i]]`, sorted by name.
    kids: Vec<u32>,
    ranges: Vec<Range<usize>>,
    /// Sum of every member's uncompressed size, for the "this will take a while"
    /// warning and for extraction progress.
    pub total_bytes: u64,
}

impl Toc {
    /// The root of the archive, which every listing starts from.
    pub const ROOT: usize = 0;

    /// Including the root, so one more than the number of members.
    #[must_use]
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// Whether the container holds nothing at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.nodes.len() <= 1
    }

    #[must_use]
    pub fn node(&self, index: usize) -> Option<&Node> {
        self.nodes.get(index)
    }

    /// The children of `index`, in byte order of their names.
    pub fn children(&self, index: usize) -> impl ExactSizeIterator<Item = usize> + '_ {
        let range = self.ranges.get(index).cloned().unwrap_or(0..0);
        self.kids[range]
            .iter()
            .map(|&k| usize::try_from(k).unwrap_or(usize::MAX))
    }

    #[must_use]
    pub fn child_count(&self, index: usize) -> usize {
        self.ranges.get(index).map_or(0, Range::len)
    }

    /// Resolve `/`-separated inner path, empty for the root.
    #[must_use]
    pub fn find(&self, inner: &str) -> Option<usize> {
        if inner.is_empty() {
            return Some(Self::ROOT);
        }
        let mut node = Self::ROOT;
        for component in inner.split('/') {
            node = self.child_by_name(node, component)?;
        }
        Some(node)
    }

    /// Resolve a path that may name a directory rather than a member, for
    /// extraction, where an absent directory is not an error.
    #[must_use]
    pub fn find_dir(&self, inner: &str) -> Option<usize> {
        self.find(inner).filter(|&i| self.nodes[i].kind.is_dir())
    }

    fn child_by_name(&self, parent: usize, name: &str) -> Option<usize> {
        let range = self.ranges.get(parent)?.clone();
        let kids = self.kids.get(range)?;
        // The closure returns a name, not an Option, so an out-of-range handle has
        // to compare as the empty string rather than propagate.
        let found = kids
            .binary_search_by_key(&name, |&k| {
                let at = usize::try_from(k).unwrap_or(usize::MAX);
                self.nodes.get(at).map_or("", |n| n.name.as_str())
            })
            .ok()?;
        usize::try_from(*kids.get(found)?).ok()
    }

    /// The full path of a member inside the container, `/`-separated and without
    /// a leading separator. Empty for the root.
    #[must_use]
    pub fn path_of(&self, index: usize) -> String {
        if index == Self::ROOT {
            return String::new();
        }
        let mut parts = Vec::with_capacity(4);
        let mut at = index;
        while at != Self::ROOT {
            let node = &self.nodes[at];
            parts.push(node.name.as_str());
            at = usize::try_from(node.parent).unwrap_or(Self::ROOT);
        }
        parts.reverse();
        parts.join("/")
    }

    /// Every descendant of `index`, `index` itself included, parents before
    /// children. Extraction walks this so a directory exists before anything
    /// inside it is written.
    pub fn subtree(&self, index: usize) -> Vec<usize> {
        let mut out = vec![index];
        let mut cursor = 0;
        while cursor < out.len() {
            let node = out[cursor];
            cursor += 1;
            out.extend(self.children(node));
        }
        out
    }
}

/// Builds a [`Toc`] in one pass over the container.
pub struct TocBuilder {
    nodes: Vec<Node>,
    /// Inner path to node index. The largest thing in the builder, and dropped by
    /// [`finish`](Self::finish), so the peak is during the scan and not for the
    /// life of the cache entry.
    index: HashMap<String, usize>,
    total_bytes: u64,
    limit: usize,
}

impl Default for TocBuilder {
    /// The root has to exist before anything is pushed: it is node 0, which every
    /// parent index points at and which [`Toc::find`] starts from. A builder that
    /// left it out would put the first member where the root belongs.
    fn default() -> Self {
        Self {
            nodes: vec![Node {
                parent: u32::try_from(Toc::ROOT).unwrap_or(0),
                name: String::new(),
                kind: NodeKind::Dir,
                size: 0,
                modified: None,
                mode: Some(DIR_MODE),
                uid: None,
                gid: None,
                handle: NO_HANDLE,
            }],
            index: HashMap::new(),
            total_bytes: 0,
            limit: usize::MAX,
        }
    }
}

impl TocBuilder {
    /// `limit` is the largest member count accepted; a container over it is
    /// refused rather than allowed to grow without bound. The root counts toward
    /// it, so a limit of `n` admits `n - 1` members.
    #[must_use]
    pub fn with_limit(limit: usize) -> Self {
        // The root node already counts, so a limit of zero would admit nothing.
        Self {
            limit: limit.max(1),
            ..Self::default()
        }
    }

    /// Add a member. `path` is `/`-separated and relative; missing parent
    /// directories are created, because most containers list `a/b/c.txt` without
    /// ever listing `a/`.
    ///
    /// A component of `.`, `..`, empty or containing a NUL is rejected: it would
    /// name something outside the archive, and the archive is the whole point of
    /// the location.
    ///
    /// Every metadata field is a parameter because a member's metadata comes from
    /// the container as a whole; grouping them would mean a struct per backend
    /// that only ever has one caller.
    #[allow(clippy::too_many_arguments)]
    pub fn push(
        &mut self,
        path: &str,
        kind: NodeKind,
        size: u64,
        modified: Option<SystemTime>,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        handle: u64,
    ) -> Result<()> {
        let mut components = Vec::with_capacity(4);
        for component in path.split('/') {
            if component.is_empty() || component == "." {
                continue;
            }
            if component == ".." || component.contains('\0') {
                return Err(Error::UnsafeComponent(component.to_string()));
            }
            components.push(component);
        }
        if components.is_empty() {
            return Ok(());
        }

        // Walking the components keeps the parent index directly, so no path has
        // to be looked up twice and a prefix never has to be rebuilt.
        let mut prefix = String::new();
        let mut parent = Toc::ROOT;
        for name in &components[..components.len() - 1] {
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(name);
            parent = self.dir(&prefix, name, parent)?;
        }

        let leaf = components[components.len() - 1];
        if !prefix.is_empty() {
            prefix.push('/');
        }
        prefix.push_str(leaf);

        if let Some(&at) = self.index.get(prefix.as_str()) {
            // A container may name the same path twice; the later entry is what
            // an extractor would write, so it is what the listing shows. Only a
            // file's bytes count toward the total, so the old size has to come
            // back out when a file replaces a file.
            let node = &mut self.nodes[at];
            if !node.kind.is_dir() {
                self.total_bytes = self.total_bytes.saturating_sub(node.size);
            }
            node.kind = kind;
            node.size = size;
            node.modified = modified;
            node.mode = mode;
            node.uid = uid;
            node.gid = gid;
            node.handle = handle;
            if !kind.is_dir() {
                self.total_bytes = self.total_bytes.saturating_add(size);
            }
            return Ok(());
        }

        if self.nodes.len() >= self.limit {
            return Err(Error::Archive(format!(
                "archive has more than {} entries",
                self.limit
            )));
        }
        self.nodes.push(Node {
            parent: u32::try_from(parent).unwrap_or(u32::MAX),
            name: (*leaf).to_string(),
            kind,
            size,
            modified,
            mode,
            uid,
            gid,
            handle,
        });
        self.index.insert(prefix, self.nodes.len() - 1);
        if !kind.is_dir() {
            self.total_bytes = self.total_bytes.saturating_add(size);
        }
        Ok(())
    }

    /// Ensure a directory exists at `path`, which is non-empty and whose parent
    /// is `parent`.
    fn dir(&mut self, path: &str, name: &str, parent: usize) -> Result<usize> {
        if let Some(&at) = self.index.get(path) {
            if !self.nodes[at].kind.is_dir() {
                return Err(Error::Archive(format!(
                    "{path} is both a file and a directory"
                )));
            }
            return Ok(at);
        }
        if self.nodes.len() >= self.limit {
            return Err(Error::Archive(format!(
                "archive has more than {} entries",
                self.limit
            )));
        }
        self.nodes.push(Node {
            parent: u32::try_from(parent).unwrap_or(u32::MAX),
            name: name.to_string(),
            kind: NodeKind::Dir,
            size: 0,
            modified: None,
            mode: Some(DIR_MODE),
            uid: None,
            gid: None,
            handle: NO_HANDLE,
        });
        let at = self.nodes.len() - 1;
        self.index.insert(path.to_string(), at);
        Ok(at)
    }

    pub fn finish(mut self) -> Toc {
        let count = self.nodes.len();
        let mut counts = vec![0u32; count];
        for (at, node) in self.nodes.iter().enumerate() {
            // The root is its own parent, which is a way of saying it has none; it
            // must not be counted as a child of itself.
            if at == Toc::ROOT {
                continue;
            }
            if let Some(at) = counts.get_mut(usize::try_from(node.parent).unwrap_or(usize::MAX)) {
                *at += 1;
            }
        }
        let mut starts = vec![0u32; count + 1];
        for at in 0..count {
            starts[at + 1] = starts[at].saturating_add(counts[at]);
        }
        let edges = usize::try_from(starts[count]).unwrap_or(0);
        let mut kids = vec![0u32; edges];
        let mut cursor = starts.clone();
        for (at, node) in self.nodes.iter().enumerate() {
            if at == Toc::ROOT {
                continue;
            }
            let Some(slot) = cursor.get_mut(usize::try_from(node.parent).unwrap_or(usize::MAX))
            else {
                continue;
            };
            if let Some(cell) = kids.get_mut(usize::try_from(*slot).unwrap_or(usize::MAX)) {
                *cell = u32::try_from(at).unwrap_or(u32::MAX);
            }
            *slot = slot.saturating_add(1);
        }
        let ranges: Vec<Range<usize>> = (0..count)
            .map(|at| {
                let from = usize::try_from(starts[at]).unwrap_or(0);
                let to = usize::try_from(starts[at + 1]).unwrap_or(0);
                from..to
            })
            .collect();
        for range in &ranges {
            kids[range.clone()].sort_by_key(|&k| {
                self.nodes
                    .get(usize::try_from(k).unwrap_or(usize::MAX))
                    .map_or("", |n| n.name.as_str())
            });
        }

        self.nodes.shrink_to_fit();
        kids.shrink_to_fit();
        Toc {
            nodes: self.nodes,
            kids,
            ranges,
            total_bytes: self.total_bytes,
        }
    }
}

/// Mode given to a directory a container never declared.
pub const DIR_MODE: u32 = 0o040_755;
/// Mode given to a file whose mode the container does not record.
pub const FILE_MODE: u32 = 0o100_644;

/// A container identified by what it is rather than by when it was last opened,
/// so a file replaced or appended to is re-read instead of served from cache.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Key {
    pub path: PathBuf,
    pub modified: Option<SystemTime>,
    pub size: u64,
}

impl Key {
    /// Stat `path` and build the key for it.
    pub fn of(path: &Path) -> Result<Self> {
        let meta = std::fs::metadata(path)?;
        Ok(Self {
            path: path.to_path_buf(),
            modified: meta.modified().ok(),
            size: meta.len(),
        })
    }
}

/// The last few containers that were opened.
///
/// Eight because a column view shows at most a handful of folders at once and a
/// user comparing two archives is the realistic worst case; more would hold
/// several megabytes of table of contents for containers that are no longer on
/// screen.
///
/// Generic over the cached value rather than always a bare [`Toc`], so an open
/// container sits in the same entry as its table of contents and one eviction
/// frees both.
pub struct Cache<V> {
    slots: Mutex<Vec<Slot<V>>>,
    limit: usize,
}

struct Slot<V> {
    key: Key,
    value: Arc<V>,
}

impl<V> Default for Cache<V> {
    fn default() -> Self {
        Self::new(8)
    }
}

impl<V> Cache<V> {
    #[must_use]
    pub fn new(limit: usize) -> Self {
        Self {
            slots: Mutex::new(Vec::new()),
            limit: limit.max(1),
        }
    }

    /// Most recently used first, so a hit can be promoted by moving it forward.
    pub fn get(&self, key: &Key) -> Option<Arc<V>> {
        let mut slots = self.slots.lock().ok()?;
        let at = slots.iter().position(|s| &s.key == key)?;
        let slot = slots.remove(at);
        let value = Arc::clone(&slot.value);
        slots.insert(0, slot);
        Some(value)
    }

    pub fn put(&self, key: Key, value: Arc<V>) {
        let Ok(mut slots) = self.slots.lock() else {
            return;
        };
        slots.retain(|s| s.key.path != key.path);
        slots.insert(0, Slot { key, value });
        slots.truncate(self.limit);
    }

    /// Forget a container, after it has been modified or deleted.
    pub fn invalidate(&self, path: &Path) {
        if let Ok(mut slots) = self.slots.lock() {
            slots.retain(|s| s.key.path != path);
        }
    }

    pub fn clear(&self) {
        if let Ok(mut slots) = self.slots.lock() {
            slots.clear();
        }
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.slots.lock().map_or(0, |s| s.len())
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A container of `members`, every one a file of one byte.
    fn build(members: &[&str]) -> Toc {
        let mut b = TocBuilder::with_limit(1000);
        for path in members {
            b.push(path, NodeKind::File, 1, None, None, None, None, 0)
                .unwrap();
        }
        b.finish()
    }

    #[test]
    fn implicit_directories_appear_once() {
        let toc = build(&["a/b/c.txt", "a/b/d.txt", "a/e.txt"]);
        let a = toc.find("a").expect("a");
        assert_eq!(toc.node(a).map(|n| n.kind), Some(NodeKind::Dir));
        assert_eq!(toc.child_count(a), 2, "b and e");
        let ab = toc.find("a/b").expect("a/b");
        assert_eq!(toc.child_count(ab), 2);
        // `a/b` was created twice by two members and must exist once.
        assert_eq!(toc.len(), 1 + 3 + 2, "root, 3 files, 2 directories");
    }

    #[test]
    fn children_are_in_name_order() {
        let toc = build(&["z.txt", "a.txt", "M.txt", "0.txt", ".hidden"]);
        let names: Vec<&str> = toc
            .children(Toc::ROOT)
            .filter_map(|i| toc.node(i).map(|n| n.name.as_str()))
            .collect();
        assert_eq!(names, [".hidden", "0.txt", "M.txt", "a.txt", "z.txt"]);
    }

    #[test]
    fn a_path_resolves_or_does_not() {
        let toc = build(&["a/b.txt"]);
        assert_eq!(toc.find(""), Some(Toc::ROOT));
        assert!(toc.find("a").is_some());
        assert!(toc.find("a/b.txt").is_some());
        assert!(toc.find("a/missing.txt").is_none());
        assert!(toc.find("a/b.txt/c").is_none(), "a file has no children");
    }

    #[test]
    fn paths_are_rebuilt_from_parents() {
        let toc = build(&["a/b/c.txt"]);
        let at = toc.find("a/b/c.txt").unwrap();
        assert_eq!(toc.path_of(at), "a/b/c.txt");
        assert_eq!(toc.path_of(Toc::ROOT), "");
    }

    #[test]
    fn a_subtree_lists_a_parent_before_its_children() {
        let toc = build(&["a/b.txt", "a/c/d.txt", "e.txt"]);
        let sub = toc.subtree(toc.find("a").unwrap());
        assert_eq!(sub.len(), 4, "a, b.txt, c, d.txt");
        assert_eq!(sub[0], toc.find("a").unwrap());
        let d = toc.find("a/c/d.txt").unwrap();
        assert!(sub.contains(&d));
        assert!(!sub.contains(&toc.find("e.txt").unwrap()));
    }

    #[test]
    fn an_explicit_directory_wins_over_an_implicit_one() {
        let mut b = TocBuilder::with_limit(100);
        b.push("a/", NodeKind::Dir, 0, None, None, None, None, NO_HANDLE)
            .unwrap();
        b.push("a/b.txt", NodeKind::File, 3, None, None, None, None, 0)
            .unwrap();
        let toc = b.finish();
        let a = toc.find("a").unwrap();
        assert_eq!(toc.child_count(a), 1);
        assert_eq!(toc.len(), 3);
    }

    #[test]
    fn a_member_that_escapes_the_archive_is_refused() {
        let mut b = TocBuilder::with_limit(100);
        for path in ["../out", "a/../../out", "\0", ""] {
            let r = b.push(path, NodeKind::File, 1, None, None, None, None, 0);
            if path.is_empty() {
                assert!(r.is_ok(), "an empty name is skipped, not an error");
            } else {
                assert!(matches!(r, Err(Error::UnsafeComponent(_))), "{path}: {r:?}");
            }
        }
    }

    #[test]
    fn a_path_that_is_both_file_and_directory_is_refused() {
        let mut b = TocBuilder::with_limit(100);
        b.push("a/b.txt", NodeKind::File, 1, None, None, None, None, 0)
            .unwrap();
        let r = b.push(
            "a/b.txt/c.txt",
            NodeKind::File,
            1,
            None,
            None,
            None,
            None,
            0,
        );
        assert!(matches!(r, Err(Error::Archive(_))), "{r:?}");
    }

    #[test]
    fn a_container_over_the_limit_is_refused_rather_than_huge() {
        // The root counts toward the limit, so four slots hold three members.
        let mut b = TocBuilder::with_limit(4);
        for name in ["a", "b", "c"] {
            b.push(name, NodeKind::File, 1, None, None, None, None, 0)
                .unwrap();
        }
        let r = b.push("d", NodeKind::File, 1, None, None, None, None, 0);
        assert!(matches!(r, Err(Error::Archive(_))), "{r:?}");
    }

    #[test]
    fn the_last_entry_at_a_path_wins() {
        let mut b = TocBuilder::with_limit(100);
        b.push("a.txt", NodeKind::File, 1, None, None, None, None, 0)
            .unwrap();
        b.push("a.txt", NodeKind::File, 9, None, None, None, None, 3)
            .unwrap();
        let toc = b.finish();
        let at = toc.find("a.txt").unwrap();
        assert_eq!(toc.node(at).map(|n| (n.size, n.handle)), Some((9, 3)));
        assert_eq!(toc.len(), 2);
    }

    #[test]
    fn total_bytes_counts_files_only() {
        let mut b = TocBuilder::with_limit(100);
        b.push("d/", NodeKind::Dir, 0, None, None, None, None, NO_HANDLE)
            .unwrap();
        b.push("d/a.txt", NodeKind::File, 7, None, None, None, None, 0)
            .unwrap();
        b.push("b.txt", NodeKind::File, 3, None, None, None, None, 0)
            .unwrap();
        assert_eq!(b.finish().total_bytes, 10);
    }

    #[test]
    fn the_cache_holds_two_and_forgets_the_least_recent() {
        let cache: Cache<Toc> = Cache::new(2);
        let key = |n: u64| Key {
            path: PathBuf::from(format!("/t/{n}.zip")),
            modified: None,
            size: 1,
        };
        cache.put(key(1), Arc::new(build(&["a"])));
        cache.put(key(2), Arc::new(build(&["b"])));
        // Touching 1 makes 2 the least recently used.
        assert!(cache.get(&key(1)).is_some());
        cache.put(key(3), Arc::new(build(&["c"])));
        assert_eq!(cache.len(), 2);
        assert!(cache.get(&key(1)).is_some());
        assert!(cache.get(&key(3)).is_some());
        assert!(cache.get(&key(2)).is_none(), "2 was evicted");
    }

    #[test]
    fn a_changed_container_is_not_a_hit() {
        let cache: Cache<Toc> = Cache::new(4);
        let path = PathBuf::from("/t/a.zip");
        let base = Key {
            path: path.clone(),
            modified: None,
            size: 10,
        };
        cache.put(base.clone(), Arc::new(build(&["a"])));
        let grown = Key {
            path: path.clone(),
            modified: None,
            size: 11,
        };
        assert!(cache.get(&grown).is_none(), "size changed");
        let touched = Key {
            path: path.clone(),
            modified: Some(SystemTime::now()),
            size: 10,
        };
        assert!(cache.get(&touched).is_none(), "mtime changed");
        cache.invalidate(&path);
        assert!(cache.is_empty());
    }
}
