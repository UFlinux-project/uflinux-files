//! Opening an index, and the writer that feeds it.
//!
//! The daemon holds one handle per index (`files` and `apps`) for the whole
//! session. Two things about tantivy shape the design here.
//!
//! A writer allocates its term hash table up front, sized from the memory
//! budget, so the budget is an upfront cost rather than a ceiling: at the 15 MB
//! floor one worker reserves about 6 MB of table plus 1 MiB arena pages. That is
//! why a writer is created once per indexing run and kept, instead of per commit
//! — a second writer is another 6 MB of address space to hold a few thousand
//! records.
//!
//! A reader with tantivy's default reload policy refreshes on a background
//! thread and is explicitly *not* synchronised with `commit()`. The file manager
//! shows search results next to a folder it just changed, so "eventually
//! consistent" reads make a re-index look broken. [`ReloadPolicy::Manual`] plus
//! an explicit [`IndexHandle::refresh`] after every commit makes a commit visible
//! by the time the call returns.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use tantivy::directory::{Advice, MmapDirectory};
use tantivy::indexer::{IndexWriter, IndexWriterOptions, LogMergePolicy};
use tantivy::{Index, IndexReader, ReloadPolicy, Searcher};
use ufm_core::{Error, Result};

use crate::doc::Record;
use crate::schema::{Fields, schema as build_schema, tokenizers};

/// Which of the two indexes a handle refers to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum IndexKind {
    /// Filenames and, for opted-in roots, file content.
    Files,
    /// Desktop entries, for the search bar and the start menu.
    Apps,
}

impl IndexKind {
    /// The directory name and the name reported over D-Bus.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Files => "files",
            Self::Apps => "apps",
        }
    }

    /// The directory this index lives in, under the shared index root.
    #[must_use]
    pub fn path(self, root: &Path) -> PathBuf {
        root.join(self.as_str())
    }
}

/// Memory and commit policy for one indexing run, sized against the daemon's
/// budget: 24 MiB resident while idle, 64 MiB while indexing.
#[derive(Debug, Clone, Copy)]
pub struct WritePolicy {
    /// Memory the writer may use. Tantivy's floor is 15 MB and it allocates a
    /// third of this eagerly; going much higher buys little when a commit happens
    /// every couple of seconds.
    pub memory_budget: usize,
    /// Commit once this many operations are buffered.
    pub max_records: u64,
    /// Commit once this much time has passed since the last commit, so a slow
    /// trickle of changes still becomes visible.
    pub max_age: Duration,
    /// Segments to accumulate before merging. Merging a folder rename's worth of
    /// tiny segments early costs I/O that a low-end machine would rather spend on
    /// the UI.
    pub merge_after: usize,
    /// Stop merging once a merge would produce a segment larger than this.
    pub max_docs_per_merge: usize,
}

impl Default for WritePolicy {
    fn default() -> Self {
        Self {
            // 15 MB is tantivy's own floor; asking for less is an error, and
            // asking for more is address space this daemon cannot spare.
            memory_budget: 15_000_000,
            max_records: 512,
            max_age: Duration::from_secs(2),
            merge_after: 8,
            max_docs_per_merge: 1_000_000,
        }
    }
}

/// An open index, with a reader that is refreshed on demand.
///
/// The writer does not live here: it takes the directory's write lock, so it is
/// created only while something is being indexed, by [`IndexHandle::writer`].
/// That is what lets the daemon sit at its idle footprint between runs.
pub struct IndexHandle {
    kind: IndexKind,
    index: Index,
    fields: Fields,
    reader: IndexReader,
    path: PathBuf,
    /// Kept alongside the copy inside `index`, because the mmap cache statistics
    /// are on `MmapDirectory` and are not reachable through the `Directory`
    /// trait. Cloning is cheap: both point at the same Arc.
    directory: MmapDirectory,
}

impl IndexHandle {
    /// Open, or create, the index for `kind` in `dir`.
    ///
    /// An index whose schema does not match this build is reported rather than
    /// repaired silently: losing a content index means the next search finds
    /// nothing, and the user should be told a re-index is running instead of
    /// being shown an empty result set forever.
    pub fn open(kind: IndexKind, dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(dir).map_err(|e| {
            Error::Search(format!(
                "cannot create the index directory {}: {e}",
                dir.display()
            ))
        })?;
        // MADV_RANDOM, because a term dictionary is read by jumping around it.
        // It turns off the kernel's speculative read-ahead, which is the one
        // thing here that would grow the daemon's page-cache footprint beyond
        // what a query actually touches.
        let directory = MmapDirectory::open_with_madvice(dir, Advice::Random)
            .map_err(|e| Error::Search(format!("cannot open {}: {e}", dir.display())))?;
        // Both indexes carry a body-text field. For files that is extracted text,
        // which is why this is the expensive index; for applications it is
        // `GenericName` and `Comment` - a few hundred short strings, and the reason
        // "disk usage" finds an entry that merely mentions it. The flag in
        // `build_schema` is still worth having: an index built without the field is
        // a strictly smaller index that answers a strictly smaller set of questions,
        // which is the right trade for a machine tighter on memory than this budget
        // assumes. `Fields` reports the field as absent and the query layer stops
        // offering body-text matches, so nothing has to know which was built.
        let index = Index::builder()
            .schema(build_schema(true))
            .tokenizers(tokenizers())
            .open_or_create(directory.clone())
            .map_err(|e| {
                Error::Search(format!(
                    "the index in {} has to be rebuilt: {e}",
                    dir.display()
                ))
            })?;
        let fields = Fields::from_schema(&index.schema())?;
        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::Manual)
            // The default caches 100 decompressed doc-store blocks. A page of
            // results touches a handful, and every cached block is uncompressed
            // memory held between searches.
            .doc_store_cache_num_blocks(16)
            // No background warming: the daemon would rather pay for a segment on
            // first use than hold a thread and its page faults open forever.
            .num_warming_threads(0)
            .try_into()
            .map_err(|e| Error::Search(format!("cannot open a reader: {e}")))?;
        Ok(Self {
            kind,
            index,
            fields,
            reader,
            path: dir.to_path_buf(),
            directory,
        })
    }

    #[must_use]
    pub const fn kind(&self) -> IndexKind {
        self.kind
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    #[must_use]
    pub fn index(&self) -> &Index {
        &self.index
    }

    #[must_use]
    pub const fn fields(&self) -> &Fields {
        &self.fields
    }

    /// A snapshot to search against. Cheap; call it per request.
    #[must_use]
    pub fn searcher(&self) -> Searcher {
        self.reader.searcher()
    }

    /// Make everything committed so far visible to [`searcher`](Self::searcher).
    pub fn refresh(&self) -> Result<()> {
        self.reader
            .reload()
            .map_err(|e| Error::Search(format!("cannot reload the reader: {e}")))
    }

    /// Open a writer. Only one can exist at a time, in this process or another,
    /// because tantivy takes a directory lock.
    pub fn writer(&self, policy: WritePolicy) -> Result<Writer<'_>> {
        Writer::new(self, policy)
    }

    /// How many records are currently searchable.
    #[must_use]
    pub fn num_records(&self) -> u64 {
        self.reader.searcher().num_docs()
    }

    /// Everything worth reporting in a `Status` call.
    #[must_use]
    pub fn status(&self) -> IndexStatus {
        let searcher = self.reader.searcher();
        let deleted = searcher
            .segment_readers()
            .iter()
            .map(|r| u64::from(r.max_doc() - r.num_docs()))
            .sum();
        IndexStatus {
            kind: self.kind.as_str().to_string(),
            path: self.path.clone(),
            records: searcher.num_docs(),
            deleted,
            segments: searcher.segment_readers().len(),
            mmapped_files: self.directory.get_cache_info().mmapped.len(),
            opstamp: self.index.load_metas().map_or(0, |meta| meta.opstamp),
        }
    }
}

/// A snapshot of one index, for the `Status` method and for the tests.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct IndexStatus {
    pub kind: String,
    pub path: PathBuf,
    /// Records a search can return.
    pub records: u64,
    /// Records deleted since the last merge, still costing a segment.
    pub deleted: u64,
    pub segments: usize,
    /// How many index files are currently mapped.
    pub mmapped_files: usize,
    /// Tantivy's commit counter, useful for telling two refreshes apart.
    pub opstamp: u64,
}

/// A writer that commits on its own schedule.
///
/// Tantivy has no `should_commit`; the caller decides. This wraps that decision
/// in one place so the crawl and the watcher-driven updates commit under the same
/// rules, and so the count and the clock live in one pair of fields instead of at
/// every call site.
pub struct Writer<'a> {
    handle: &'a IndexHandle,
    inner: IndexWriter,
    policy: WritePolicy,
    buffered: u64,
    since: Instant,
}

impl<'a> Writer<'a> {
    fn new(handle: &'a IndexHandle, policy: WritePolicy) -> Result<Self> {
        let options = IndexWriterOptions::builder()
            .memory_budget_per_thread(policy.memory_budget)
            // One worker. Each worker costs its own eagerly allocated table, and
            // a crawl is I/O bound on a slow disk, not CPU bound.
            .num_worker_threads(1)
            // Merging is what makes a re-index feel heavy; one thread at a time
            // keeps it in the background instead of competing with the UI.
            .num_merge_threads(1)
            .build();
        let inner: IndexWriter = handle
            .index
            .writer_with_options(options)
            .map_err(|e| Error::Search(format!("cannot open a writer: {e}")))?;
        let mut merge_policy = LogMergePolicy::default();
        merge_policy.set_min_num_segments(policy.merge_after);
        merge_policy.set_max_docs_before_merge(policy.max_docs_per_merge);
        inner.set_merge_policy(Box::new(merge_policy));
        Ok(Self {
            handle,
            inner,
            policy,
            buffered: 0,
            since: Instant::now(),
        })
    }

    #[must_use]
    pub fn handle(&self) -> &IndexHandle {
        self.handle
    }

    /// Index one record, replacing any record already stored for its path.
    ///
    /// Delete-then-add rather than add-then-deduplicate: the path is the key a hit
    /// is opened with, and a second document for the same path would show the same
    /// file twice in the results.
    pub fn upsert(&mut self, record: &Record) -> Result<()> {
        let doc = record.to_doc(self.handle.fields());
        self.inner.delete_term(record.term(self.handle.fields()));
        self.inner
            .add_document(doc)
            .map_err(|e| Error::Search(format!("cannot index {}: {e}", record.path)))?;
        self.buffered += 1;
        Ok(())
    }

    /// Drop the record stored for `path`, if any. Cheap when there is none.
    pub fn remove(&mut self, path: &str) -> Result<()> {
        let term = tantivy::Term::from_field_text(self.handle.fields().path, path);
        self.inner.delete_term(term);
        self.buffered += 1;
        Ok(())
    }

    /// Drop every record matching `query`. Used to clear one directory before it
    /// is re-read, which is cheaper and much more precise than deleting paths one
    /// by one.
    pub fn delete_query(&mut self, query: &tantivy::query::TermQuery) -> Result<()> {
        self.inner
            .delete_query(Box::new(query.clone()))
            .map_err(|e| Error::Search(format!("cannot clear records: {e}")))?;
        self.buffered += 1;
        Ok(())
    }

    /// Whether the buffered changes have grown old or numerous enough to show.
    #[must_use]
    pub fn should_commit(&self) -> bool {
        self.buffered > 0
            && (self.buffered >= self.policy.max_records
                || self.since.elapsed() >= self.policy.max_age)
    }

    /// Commit if the policy says so. Returns whether a commit happened.
    pub fn commit_if_due(&mut self) -> Result<bool> {
        if self.should_commit() {
            self.commit()?;
            return Ok(true);
        }
        Ok(false)
    }

    /// Write the buffered changes and make them visible.
    pub fn commit(&mut self) -> Result<()> {
        self.inner
            .commit()
            .map_err(|e| Error::Search(format!("cannot commit: {e}")))?;
        self.handle.refresh()?;
        self.buffered = 0;
        self.since = Instant::now();
        Ok(())
    }

    /// Whether anything has been written since the last commit.
    #[must_use]
    pub const fn has_pending(&self) -> bool {
        self.buffered > 0
    }

    /// How many operations are buffered.
    #[must_use]
    pub const fn buffered(&self) -> u64 {
        self.buffered
    }

    /// Discard the buffered changes. The writer is being thrown away anyway, and
    /// this says so explicitly rather than relying on `Drop`.
    pub fn rollback(mut self) -> Result<()> {
        self.inner
            .rollback()
            .map_err(|e| Error::Search(format!("cannot roll back: {e}")))?;
        Ok(())
    }

    /// Commit, then join the merge threads so no merge outlives the writer.
    ///
    /// Dropping the writer instead would also release the lock, but a merge still
    /// running at that point keeps the process busy after the daemon believes it
    /// is idle, which shows up as unexplained memory in the idle benchmark.
    pub fn finish(mut self) -> Result<()> {
        self.commit()?;
        self.inner
            .wait_merging_threads()
            .map_err(|e| Error::Search(format!("a merge failed: {e}")))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doc::Kind;
    use crate::test_record;

    fn handle(dir: &Path) -> IndexHandle {
        IndexHandle::open(IndexKind::Files, dir).expect("open a fresh index")
    }

    #[test]
    fn a_fresh_index_opens_empty_and_reports_itself() {
        let dir = tempfile::tempdir().expect("tempdir");
        let handle = handle(dir.path());
        let status = handle.status();
        assert_eq!(status.records, 0);
        assert_eq!(status.segments, 0);
        assert_eq!(status.kind, "files");
        assert_eq!(handle.num_records(), 0);
    }

    #[test]
    fn the_two_kinds_do_not_share_a_directory() {
        let root = tempfile::tempdir().expect("tempdir");
        let files = IndexKind::Files.path(root.path());
        let apps = IndexKind::Apps.path(root.path());
        assert_ne!(files, apps);
        IndexHandle::open(IndexKind::Files, &files).expect("files index");
        IndexHandle::open(IndexKind::Apps, &apps).expect("apps index");
        assert!(files.join("meta.json").is_file());
        assert!(apps.join("meta.json").is_file());
    }

    #[test]
    fn an_index_reopens_across_a_handle() {
        let dir = tempfile::tempdir().expect("tempdir");
        {
            let handle = handle(dir.path());
            let mut writer = handle.writer(WritePolicy::default()).expect("writer");
            writer.upsert(&test_record("/home/u/a.txt")).expect("index");
            writer.commit().expect("commit");
        }
        let reopened = handle(dir.path());
        assert_eq!(
            reopened.num_records(),
            1,
            "a commit that returned must be visible after reopening"
        );
    }

    #[test]
    fn a_committed_record_is_visible_without_a_background_reload() {
        let dir = tempfile::tempdir().expect("tempdir");
        let handle = handle(dir.path());
        assert_eq!(handle.num_records(), 0);
        let mut writer = handle.writer(WritePolicy::default()).expect("writer");
        writer.upsert(&test_record("/home/u/a.txt")).expect("index");
        assert_eq!(
            handle.num_records(),
            0,
            "uncommitted work must not appear, or the count would move mid-crawl"
        );
        writer.commit().expect("commit");
        assert_eq!(handle.num_records(), 1);
    }

    #[test]
    fn only_one_writer_at_a_time() {
        let dir = tempfile::tempdir().expect("tempdir");
        let handle = handle(dir.path());
        let _writer = handle.writer(WritePolicy::default()).expect("first writer");
        let err = handle
            .writer(WritePolicy::default())
            .err()
            .expect("the directory lock is held");
        assert!(
            err.to_string().contains("lock"),
            "the error should explain the lock: {err}"
        );
    }

    #[test]
    fn the_commit_policy_fires_on_count_and_on_time() {
        let dir = tempfile::tempdir().expect("tempdir");
        let handle = handle(dir.path());
        let mut writer = handle
            .writer(WritePolicy {
                max_records: 3,
                max_age: Duration::from_secs(3600),
                ..WritePolicy::default()
            })
            .expect("writer");
        assert!(!writer.commit_if_due().expect("nothing buffered yet"));
        for name in ["a", "b"] {
            writer
                .upsert(&test_record(&format!("/home/u/{name}")))
                .expect("index");
        }
        assert!(!writer.should_commit(), "two of three is not due");
        writer.upsert(&test_record("/home/u/c")).expect("index");
        assert!(writer.should_commit(), "the third record makes it due");
        assert!(writer.commit_if_due().expect("commit"));
        assert!(!writer.commit_if_due().expect("nothing new to commit"));

        // The same writer, now driven by the clock rather than by the count.
        writer.upsert(&test_record("/home/u/d")).expect("index");
        writer.policy.max_records = 1_000;
        writer.policy.max_age = Duration::ZERO;
        assert!(writer.should_commit(), "any elapsed time at all is enough");
    }

    #[test]
    fn upserting_the_same_path_twice_leaves_one_record() {
        let dir = tempfile::tempdir().expect("tempdir");
        let handle = handle(dir.path());
        let mut writer = handle.writer(WritePolicy::default()).expect("writer");
        let mut record = test_record("/home/u/a.txt");
        writer.upsert(&record).expect("first");
        writer.commit().expect("commit");
        record.size = 9_999;
        writer.upsert(&record).expect("second");
        writer.finish().expect("finish");
        assert_eq!(
            handle.num_records(),
            1,
            "the second write replaces the first rather than duplicating it"
        );
    }

    #[test]
    fn removing_a_record_lets_the_same_path_be_reindexed_as_a_folder() {
        let dir = tempfile::tempdir().expect("tempdir");
        let handle = handle(dir.path());
        let mut writer = handle.writer(WritePolicy::default()).expect("writer");
        let mut record = test_record("/home/u/thing");
        writer.upsert(&record).expect("file");
        writer.commit().expect("commit");
        writer.remove("/home/u/thing").expect("remove");
        record.kind = Kind::Directory;
        record.size = 0;
        writer.upsert(&record).expect("directory");
        writer.finish().expect("finish");

        let searcher = handle.searcher();
        assert_eq!(searcher.num_docs(), 1);
        let (_, addr) = searcher
            .search(
                &tantivy::query::AllQuery,
                &tantivy::collector::TopDocs::with_limit(2).order_by_score(),
            )
            .expect("search")
            .into_iter()
            .next()
            .expect("one hit");
        let doc: tantivy::TantivyDocument = searcher.doc(addr).expect("stored doc");
        let kind = doc
            .get_first(handle.fields().kind)
            .and_then(|v| tantivy::schema::Value::as_facet(&v))
            .map(Kind::from_facet_value);
        assert_eq!(kind, Some(Kind::Directory));
    }

    #[test]
    fn a_rollback_discards_the_buffer() {
        let dir = tempfile::tempdir().expect("tempdir");
        let handle = handle(dir.path());
        let mut writer = handle.writer(WritePolicy::default()).expect("writer");
        writer.upsert(&test_record("/home/u/a.txt")).expect("index");
        assert!(writer.has_pending());
        writer.rollback().expect("rollback");
        assert_eq!(handle.num_records(), 0);
    }

    #[test]
    fn an_index_built_with_another_schema_is_reported_not_repaired() {
        let dir = tempfile::tempdir().expect("tempdir");
        // Both kinds share a schema now, so the mismatch has to come from outside:
        // an index written by a build whose schema differs, which here is one built
        // without the body-text field. Opening it must fail loudly rather than
        // quietly answer "nothing matches" forever, because the only cure is a
        // rebuild and the user is the one who has to be told to run one.
        tantivy::Index::builder()
            .schema(crate::schema::schema(false))
            .tokenizers(crate::schema::tokenizers())
            .create_in_dir(dir.path())
            .expect("an index written with the other schema");
        let err = IndexHandle::open(IndexKind::Files, dir.path())
            .err()
            .expect("a schema mismatch is an error, not a silent empty index");
        assert!(err.to_string().contains("rebuilt"), "{err}");
    }
}
