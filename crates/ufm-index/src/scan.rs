//! Walking a directory tree and turning it into records.
//!
//! This module knows how to find files and nothing about how they are indexed: it
//! hands [`Record`]s to a [`Writer`] and reports what it left out. Two things make
//! that separation worth the trouble.
//!
//! The crawl has to survive a home directory. A permission hole inside
//! `~/.local/share`, a `node_modules` with two hundred thousand entries, a dangling
//! symlink and a socket in `/dev` all have to be survivable, and none of them is a
//! reason to stop indexing the rest. Every problem here is counted and skipped, and
//! the counts are what the daemon reports as the reason part of a tree is not
//! searchable. A failure of the writer is the one thing that does propagate: that
//! means an indexing worker died, and a crawl that carried on would report success
//! over a broken index.
//!
//! The second reason is the incremental path. When a watch fires, re-crawling
//! everything is wrong — it can take longer than a user will wait, and it re-reads
//! files that did not change. [`Scanner::resync_dir`] re-reads one directory's
//! subtree, which is what a single inotify event describes.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ufm_core::{Error, Result};
use walkdir::WalkDir;

use crate::doc::{Kind, Record, facet_for};
use crate::write::Writer;

/// Directory names that are never worth indexing, matched at any depth. Each of
/// these holds either derived data or more files than a user would search for.
pub const PRUNED_DIRS: &[&str] = &[
    ".cache",
    ".git",
    ".hg",
    ".svn",
    "node_modules",
    ".Trash",
    "Trash",
    ".mozilla",
];

/// How long a directory has to be quiet before it is worth re-reading, which is
/// the watcher's debounce rather than a second opinion on the same question.
pub const DEBOUNCE: Duration = ufm_vfs::watch::Watcher::DEBOUNCE;

/// What to skip while walking, and how much text to pull out of a file.
#[derive(Debug, Clone)]
pub struct ScanPolicy {
    /// How deep to go from a root. A home directory nests deeply enough by
    /// accident; without a limit, one archive extracted with a hundred levels of
    /// nesting decides how long a crawl takes.
    pub max_depth: usize,
    /// Index dotfiles as well, marking them hidden. Leaving them out entirely would
    /// make `.bashrc` unfindable, which is not what "hidden" means.
    pub include_hidden: bool,
    /// Do not cross a mount point. A crawl of `/home` should not follow a network
    /// share mounted inside it: the user mounted that deliberately, and it is
    /// indexed as its own root or not at all.
    pub stay_on_filesystem: bool,
    /// Directories never descended into. A pattern without a `/` is a directory
    /// name matched at any depth; one with a `/` is an absolute prefix.
    pub prune: Vec<String>,
    /// Largest file, in bytes, whose text is extracted. Beyond this a file is
    /// still indexed by name; reading it costs more than it can add.
    pub content_max_size: u64,
}

impl Default for ScanPolicy {
    fn default() -> Self {
        Self {
            max_depth: 12,
            include_hidden: true,
            stay_on_filesystem: true,
            prune: PRUNED_DIRS.iter().map(|s| (*s).to_string()).collect(),
            content_max_size: 1024 * 1024,
        }
    }
}

impl ScanPolicy {
    /// Whether a path should be skipped, by its own name or by its position.
    #[must_use]
    pub fn prunes(&self, path: &Path) -> bool {
        self.prune.iter().any(|pattern| {
            if pattern.contains('/') {
                path.starts_with(pattern)
            } else {
                path.file_name()
                    .is_some_and(|name| name == pattern.as_str())
            }
        })
    }
}

/// What one crawl did. Reported over D-Bus as the reason a search is incomplete.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CrawlStats {
    /// Records written.
    pub indexed: u64,
    /// Directories written, including the roots.
    pub directories: u64,
    /// Entries whose name is not valid UTF-8, so they cannot be indexed at all.
    pub skipped_encoding: u64,
    /// Directories not descended into. They are still indexed as rows, because a
    /// pruned folder is a folder the user can still open.
    pub pruned: u64,
    /// Directories that could not be read: a permission hole, a dangling symlink,
    /// or a mount that vanished mid-walk.
    pub unreadable: u64,
    /// Files whose text was extracted.
    pub extracted: u64,
    /// Wall time, rounded to milliseconds so it survives a JSON round trip.
    pub millis: u64,
}

impl CrawlStats {
    fn absorb(&mut self, other: &Self) {
        self.indexed += other.indexed;
        self.directories += other.directories;
        self.skipped_encoding += other.skipped_encoding;
        self.pruned += other.pruned;
        self.unreadable += other.unreadable;
        self.extracted += other.extracted;
    }
}

/// Pulls text out of a file. Implemented by the daemon, which is where the decision
/// to shell out to `pdftotext` belongs; the scanner only decides which files are
/// worth asking about.
pub trait Extractor {
    /// Text for `path`, or `None` when there is nothing to index. Returning `None`
    /// is not an error: most files hold no text a human would search for.
    fn extract(&self, path: &Path, mime: Option<&str>) -> Option<String>;
}

/// Reads a file as UTF-8 text.
///
/// Deliberately strict: a file that is not valid UTF-8 is a binary, and indexing
/// the first megabyte of an ELF image as prose adds postings that match nothing
/// anyone would type.
pub struct PlainText;

impl Extractor for PlainText {
    fn extract(&self, path: &Path, mime: Option<&str>) -> Option<String> {
        let is_text = mime.is_some_and(|m| {
            m.starts_with("text/") || m == "application/json" || m == "application/x-shellscript"
        });
        if !is_text {
            return None;
        }
        std::fs::read_to_string(path).ok().filter(|t| !t.is_empty())
    }
}

/// A crawl, driven by a policy and an extractor.
pub struct Scanner<'p, E: Extractor> {
    policy: &'p ScanPolicy,
    extractor: Option<&'p E>,
}

impl<'p, E: Extractor> Scanner<'p, E> {
    #[must_use]
    pub const fn new(policy: &'p ScanPolicy, extractor: Option<&'p E>) -> Self {
        Self { policy, extractor }
    }

    /// Index everything under `root`, adding to whatever is already indexed.
    ///
    /// The root itself is indexed too, so searching for a top-level folder finds
    /// it. A root that cannot be read is not an error: it lands in
    /// [`CrawlStats::unreadable`], because an unmounted `~/Documents` is a normal
    /// state for a home directory to be in.
    pub fn crawl(&self, root: &Path, writer: &mut Writer<'_>) -> Result<CrawlStats> {
        let started = Instant::now();
        let mut stats = CrawlStats::default();
        let walk = WalkDir::new(root)
            .max_depth(self.policy.max_depth)
            // Not following symlinks means a link is indexed as the link the user
            // sees, and makes a `~/.config` full of loops harmless.
            .follow_links(false)
            .same_file_system(self.policy.stay_on_filesystem)
            // Each open directory is a file descriptor held for the duration.
            .max_open(16);
        let mut iter = walk.into_iter();
        while let Some(entry) = iter.next() {
            let entry = match entry {
                Ok(entry) => entry,
                Err(err) => {
                    // A permission hole, or a directory that vanished mid-walk. If
                    // it is the root, the whole root is unreadable and there is
                    // nothing further to say about it.
                    if err.depth() == 0 {
                        stats.unreadable = 1;
                        break;
                    }
                    stats.unreadable += 1;
                    continue;
                }
            };
            let path = entry.path();
            // The root of the crawl is never pruned: the user asked for it, and
            // dropping it would make a root vanish from its own search results.
            let pruned = entry.depth() > 0 && self.policy.prunes(path);
            if pruned && !entry.file_type().is_dir() {
                continue;
            }
            self.record_for(&entry, writer, &mut stats)?;
            if pruned {
                stats.pruned += 1;
                iter.skip_current_dir();
            }
        }
        stats.millis = duration_millis(started);
        Ok(stats)
    }

    /// Index every root in `roots`, in order.
    pub fn crawl_all(&self, roots: &[PathBuf], writer: &mut Writer<'_>) -> Result<CrawlStats> {
        let started = Instant::now();
        let mut total = CrawlStats::default();
        for root in roots {
            total.absorb(&self.crawl(root, writer)?);
        }
        total.millis = duration_millis(started);
        Ok(total)
    }

    /// Re-index one directory and everything below it.
    ///
    /// The whole subtree, not just the direct children, because of how a facet is
    /// indexed: a document under `/a/b/c` also carries the terms for `/`, `/a` and
    /// `/a/b`, which is what makes "everything inside this folder" a single term
    /// query. The same property means a term query on `/a/b`'s facet matches its
    /// grandchildren, so clearing a folder clears what is inside it too. Narrowing
    /// that to the direct children would mean reading the stored parent of every
    /// descendant, which costs more than re-reading the folder.
    ///
    /// Clearing happens before the re-read, so a file renamed within the folder
    /// cannot leave a stale row behind. The folder's own record is cleared with its
    /// contents and re-added by the crawl, so after a resync the index says what is
    /// true: a folder that vanished in the meantime is simply gone, and one that
    /// could not be read reports itself through [`CrawlStats::unreadable`] with
    /// nothing indexed.
    pub fn resync_dir(&self, dir: &Path, writer: &mut Writer<'_>) -> Result<CrawlStats> {
        let dir_str = dir.to_str().ok_or_else(|| {
            Error::Search("a directory with a non-UTF-8 name cannot be re-indexed".to_string())
        })?;
        let facet = facet_for(dir_str).ok_or_else(|| {
            Error::Search(format!(
                "{} is not an absolute path, so nothing can be indexed under it",
                dir.display()
            ))
        })?;
        let query = tantivy::query::TermQuery::new(
            tantivy::Term::from_facet(writer.handle().fields().parent, &facet),
            tantivy::schema::IndexRecordOption::Basic,
        );
        writer.delete_query(&query)?;
        writer.remove(dir_str)?;
        // Depth is measured from here, so a deep folder re-reads as deeply as a
        // crawl of it would.
        self.crawl(dir, writer)
    }

    /// Build and write the record for one walked entry.
    fn record_for(
        &self,
        entry: &walkdir::DirEntry,
        writer: &mut Writer<'_>,
        stats: &mut CrawlStats,
    ) -> Result<()> {
        let path = entry.path();
        let Ok(metadata) = entry.metadata() else {
            // A dangling symlink: `lstat` worked and `stat` did not. Counted as
            // unreadable rather than skipped silently, so a broken link turns up in
            // the numbers a user can be shown.
            stats.unreadable += 1;
            return Ok(());
        };
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            stats.skipped_encoding += 1;
            return Ok(());
        };
        let Some(mut record) = Record::from_path(path, &metadata, name) else {
            stats.skipped_encoding += 1;
            return Ok(());
        };
        if !self.policy.include_hidden && record.hidden {
            return Ok(());
        }
        if record.kind == Kind::Directory {
            stats.directories += 1;
        } else if self.wants_content(&record) {
            self.fill_content(&mut record, writer, stats)?;
        }
        writer.upsert(&record)?;
        stats.indexed += 1;
        Ok(())
    }

    fn wants_content(&self, record: &Record) -> bool {
        self.extractor.is_some()
            && record.size <= i64::try_from(self.policy.content_max_size).unwrap_or(i64::MAX)
    }

    /// Extract text and attach it, handing data back to tantivy as it goes.
    ///
    /// The commit check lives here rather than in the walk loop because this is the
    /// only place a record can grow large. A crawl that pulls two megabytes out of
    /// every text file has to let tantivy flush, or the writer's memory budget is
    /// the only thing keeping the process honest — and that budget is sized for the
    /// whole daemon, not for one file.
    fn fill_content(
        &self,
        record: &mut Record,
        writer: &mut Writer<'_>,
        stats: &mut CrawlStats,
    ) -> Result<()> {
        let text = self
            .extractor
            .and_then(|e| e.extract(Path::new(&record.path), record.mime.as_deref()));
        if let Some(text) = text {
            record.content = Some(text);
            stats.extracted += 1;
        }
        writer.commit_if_due()?;
        Ok(())
    }
}

fn duration_millis(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::write::{IndexHandle, IndexKind, WritePolicy};
    use std::fs;

    /// A small tree: two folders, two text files, a dotfile, a binary-looking file.
    fn tree(root: &Path) {
        fs::create_dir_all(root.join("Documents/notes")).expect("mkdir");
        fs::create_dir_all(root.join(".cache/build")).expect("mkdir");
        fs::create_dir_all(root.join("Pictures")).expect("mkdir");
        fs::write(root.join("Documents/report.txt"), "annual report\n").expect("write");
        fs::write(root.join("Documents/notes/todo.md"), "# todo\n").expect("write");
        fs::write(root.join(".bashrc"), "export PATH\n").expect("write");
        fs::write(root.join("Pictures/photo.bin"), [0u8, 159, 10, 0, 200]).expect("write");
    }

    fn handle(dir: &Path) -> IndexHandle {
        IndexHandle::open(IndexKind::Files, dir).expect("open")
    }

    #[test]
    fn a_crawl_indexes_files_and_the_folders_holding_them() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("root");
        tree(&root);
        let index = handle(&dir.path().join("index"));
        let policy = ScanPolicy::default();
        let mut writer = index.writer(WritePolicy::default()).expect("writer");
        let stats = Scanner::<PlainText>::new(&policy, None)
            .crawl(&root, &mut writer)
            .expect("crawl");
        writer.finish().expect("finish");

        // root, Documents, notes, .cache, Pictures, report.txt, todo.md, .bashrc,
        // photo.bin. `build` is inside the pruned `.cache`.
        assert_eq!(stats.indexed, 9, "{stats:?}");
        assert_eq!(stats.directories, 5, "{stats:?}");
        assert_eq!(stats.unreadable, 0);
        assert_eq!(index.num_records(), 9);
    }

    #[test]
    fn a_pruned_directory_is_still_a_row_but_is_not_entered() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("root");
        tree(&root);
        let index = handle(&dir.path().join("index"));
        let policy = ScanPolicy::default();
        let mut writer = index.writer(WritePolicy::default()).expect("writer");
        let stats = Scanner::<PlainText>::new(&policy, None)
            .crawl(&root, &mut writer)
            .expect("crawl");
        assert_eq!(stats.pruned, 1, ".cache is pruned");
        let paths = paths_of(&index, &mut writer);
        assert!(
            !paths.iter().any(|p| p.contains("build")),
            "the contents of a pruned folder are never read: {paths:?}"
        );
        assert!(
            paths
                .iter()
                .any(|p| Path::new(p).file_name().is_some_and(|n| n == ".cache")),
            "a pruned folder is still something the user can open: {paths:?}"
        );
    }

    #[test]
    fn a_depth_limit_stops_the_walk() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("root");
        tree(&root);
        let index = handle(&dir.path().join("index"));
        let policy = ScanPolicy {
            max_depth: 1,
            ..ScanPolicy::default()
        };
        let mut writer = index.writer(WritePolicy::default()).expect("writer");
        let stats = Scanner::<PlainText>::new(&policy, None)
            .crawl(&root, &mut writer)
            .expect("crawl");
        // root plus its immediate children; `Documents/notes` is one level too far.
        assert_eq!(stats.indexed, 5, "{stats:?}");
    }

    #[test]
    fn hidden_entries_can_be_left_out_entirely() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("root");
        tree(&root);
        let index = handle(&dir.path().join("index"));
        let policy = ScanPolicy {
            include_hidden: false,
            ..ScanPolicy::default()
        };
        let mut writer = index.writer(WritePolicy::default()).expect("writer");
        let stats = Scanner::<PlainText>::new(&policy, None)
            .crawl(&root, &mut writer)
            .expect("crawl");
        assert_eq!(stats.indexed, 7, ".bashrc and .cache are gone: {stats:?}");
    }

    #[test]
    fn an_unreadable_root_is_reported_instead_of_failing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let index = handle(&dir.path().join("index"));
        let policy = ScanPolicy::default();
        let mut writer = index.writer(WritePolicy::default()).expect("writer");
        let stats = Scanner::<PlainText>::new(&policy, None)
            .crawl(&dir.path().join("does-not-exist"), &mut writer)
            .expect("a missing root is not an error");
        assert_eq!(stats.indexed, 0);
        assert_eq!(stats.unreadable, 1, "the root itself could not be read");
    }

    #[test]
    fn content_is_extracted_only_for_small_text_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("root");
        tree(&root);
        fs::write(root.join("big.txt"), "x".repeat(4096)).expect("write");
        let index = handle(&dir.path().join("index"));
        let policy = ScanPolicy {
            content_max_size: 64,
            ..ScanPolicy::default()
        };
        let mut writer = index.writer(WritePolicy::default()).expect("writer");
        let stats = Scanner::new(&policy, Some(&PlainText))
            .crawl(&root, &mut writer)
            .expect("crawl");
        // report.txt and todo.md are small text files. photo.bin has no text MIME,
        // big.txt is over the limit, and .bashrc has no extension to guess from.
        assert_eq!(stats.extracted, 2, "{stats:?}");
    }

    #[test]
    fn a_resync_replaces_a_folder_and_leaves_its_siblings_alone() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("root");
        tree(&root);
        let index = handle(&dir.path().join("index"));
        let policy = ScanPolicy::default();
        let mut writer = index.writer(WritePolicy::default()).expect("writer");
        Scanner::<PlainText>::new(&policy, None)
            .crawl(&root, &mut writer)
            .expect("crawl")
            .expect_indexed();
        writer.commit().expect("commit");
        assert_eq!(index.num_records(), 9);

        // Delete one file and add another, then resync only `Documents`.
        fs::remove_file(root.join("Documents/report.txt")).expect("rm");
        fs::write(root.join("Documents/new.txt"), "hello\n").expect("write");
        let stats = Scanner::<PlainText>::new(&policy, None)
            .resync_dir(&root.join("Documents"), &mut writer)
            .expect("resync");
        assert_eq!(
            stats.indexed, 4,
            "Documents, notes, new.txt and todo.md: {stats:?}"
        );

        let paths = paths_of(&index, &mut writer);
        assert!(
            !paths.iter().any(|p| p.ends_with("report.txt")),
            "a deleted file must not remain: {paths:?}"
        );
        assert!(paths.iter().any(|p| p.ends_with("new.txt")));
        assert!(
            paths.iter().any(|p| p.ends_with(".bashrc")),
            "a sibling is untouched: {paths:?}"
        );
        assert!(
            paths.iter().any(|p| p.ends_with("todo.md")),
            "a file that did not change is re-indexed, not lost: {paths:?}"
        );
    }

    #[test]
    fn a_resync_of_a_vanished_folder_clears_it_and_says_so() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("root");
        tree(&root);
        let index = handle(&dir.path().join("index"));
        let policy = ScanPolicy::default();
        let mut writer = index.writer(WritePolicy::default()).expect("writer");
        Scanner::<PlainText>::new(&policy, None)
            .crawl(&root, &mut writer)
            .expect("crawl")
            .expect_indexed();
        writer.commit().expect("commit");

        fs::remove_dir_all(root.join("Pictures")).expect("rmdir");
        let stats = Scanner::<PlainText>::new(&policy, None)
            .resync_dir(&root.join("Pictures"), &mut writer)
            .expect("a vanished folder is a state, not a failure");
        assert_eq!(stats.indexed, 0);
        assert_eq!(
            stats.unreadable, 1,
            "the caller needs to know the folder is gone, not merely empty"
        );
        // The clear already happened, which is the right outcome: the folder is
        // gone, so its children must not stay searchable.
        let paths = paths_of(&index, &mut writer);
        assert!(!paths.iter().any(|p| p.contains("Pictures")), "{paths:?}");
    }

    #[test]
    fn crawling_two_roots_adds_up() {
        let dir = tempfile::tempdir().expect("tempdir");
        let a = dir.path().join("a");
        let b = dir.path().join("b");
        fs::create_dir_all(&a).expect("mkdir");
        fs::create_dir_all(&b).expect("mkdir");
        fs::write(a.join("one.txt"), "1").expect("write");
        fs::write(b.join("two.txt"), "2").expect("write");
        let index = handle(&dir.path().join("index"));
        let policy = ScanPolicy::default();
        let mut writer = index.writer(WritePolicy::default()).expect("writer");
        let stats = Scanner::<PlainText>::new(&policy, None)
            .crawl_all(&[a, b], &mut writer)
            .expect("crawl");
        writer.finish().expect("finish");
        assert_eq!(stats.indexed, 4, "two roots and two files");
        assert_eq!(stats.directories, 2);
        assert_eq!(index.num_records(), 4);
    }

    #[test]
    fn an_absolute_prune_pattern_matches_a_position_not_a_name() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("root");
        tree(&root);
        let policy = ScanPolicy {
            prune: vec![root.join("Pictures").to_string_lossy().to_string()],
            ..ScanPolicy::default()
        };
        assert!(policy.prunes(&root.join("Pictures")));
        assert!(
            policy.prunes(&root.join("Pictures/photo.bin")),
            "the prefix covers everything below it"
        );
        assert!(!policy.prunes(&root.join("Documents")));
    }

    impl CrawlStats {
        /// Panics with the whole picture in the message, so a failing assertion
        /// reports the crawl rather than one number.
        fn expect_indexed(self) {
            assert!(self.indexed > 0, "{self:?}");
        }
    }

    /// Everything committed so far, as paths. The writer is needed because an
    /// uncommitted crawl is invisible by design, and a test that forgot that would
    /// read back an empty index.
    fn paths_of(index: &IndexHandle, writer: &mut Writer<'_>) -> Vec<String> {
        writer.commit().expect("commit");
        let searcher = index.searcher();
        let hits = searcher
            .search(
                &tantivy::query::AllQuery,
                &tantivy::collector::TopDocs::with_limit(200).order_by_score(),
            )
            .expect("search");
        hits.into_iter()
            .filter_map(|(_, addr)| {
                let doc: tantivy::TantivyDocument = searcher.doc(addr).ok()?;
                doc.get_first(index.fields().path)
                    .and_then(|v| tantivy::schema::Value::as_str(&v))
                    .map(str::to_string)
            })
            .collect()
    }
}
