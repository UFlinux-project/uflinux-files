//! The thing that answers a request, and the job that keeps the answer current.
//!
//! [`Engine`] sits between the bus and [`ufm_index`]: it validates a
//! [`Request`], reads the right index, and returns a [`Reply`] that is known to fit
//! one D-Bus message. It also owns the two indexes, so exactly one component in the
//! daemon holds tantivy's directory lock, and it owns the busy flag, so a re-index
//! cannot be started twice by two clients.
//!
//! Two decisions in here are worth stating because they shape everything else.
//!
//! Indexing is not something the engine does while answering. [`Engine::dispatch`]
//! returns a [`Dispatch::Index`] describing work to do, and the caller — the
//! daemon's worker pool — runs it. Answering a `Reindex` call by crawling the home
//! directory synchronously would hold a D-Bus reply open for minutes, and every
//! other client would time out behind it. The engine therefore reports what should
//! happen rather than making it happen, and [`Engine::run`] is the executor the
//! worker calls.
//!
//! A crawl clears before it re-reads. The alternative — adding what is new and
//! hoping — means a deleted file stays searchable forever, and the only fix is a
//! full rebuild. [`ufm_index::Scanner::resync_dir`] does the clear per root, which
//! is also what lets a re-index of one folder leave the rest of the index alone.
//!
//! [`Engine::run`] takes a progress callback rather than emitting signals itself:
//! this crate does not know what a signal is, and the daemon is the process that
//! owns the bus connection.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use ufm_core::{Error, Result};
use ufm_index::{
    AppCrawler, CrawlStats, IndexHandle, IndexKind, MAX_CONTENT_BYTES, ScanPolicy, Scanner,
    SearchRequest, WritePolicy, search,
};

use crate::config::{ContentMode, IndexConfig, Root};
use crate::extract::{Chain, Limit};
use crate::wire::{
    self, Changed, Hit, MAX_REPLY, Progress, Reply, Request, SearchReply, StatusReply, WireError,
};

/// The two open indexes, the configuration, and the flags that keep them coherent.
pub struct Engine {
    files: IndexHandle,
    apps: IndexHandle,
    config: Mutex<IndexConfig>,
    /// A problem worth showing in the settings page: a root that could not be read,
    /// or a configuration file that had to be replaced with the defaults.
    note: Mutex<Option<String>>,
    busy: AtomicBool,
    generation: AtomicU64,
    /// Where to look for `.desktop` files, and which session to filter them by.
    /// Both are constructor arguments rather than ambient state so that a test can
    /// point an engine at a fixture directory and a known session.
    apps_dirs: Vec<PathBuf>,
    session: Option<String>,
}

/// Holds the busy flag for the length of a crawl.
///
/// A guard rather than a pair of calls, because the interesting failure of a manual
/// flag is the error path that forgets to clear it: the daemon would then report
/// itself busy forever and refuse every later re-index until it was restarted.
pub struct Busy<'e> {
    engine: &'e Engine,
}

impl Drop for Busy<'_> {
    fn drop(&mut self) {
        self.engine.busy.store(false, Ordering::Release);
    }
}

/// What to do with a request.
#[derive(Debug, Clone, PartialEq)]
pub enum Dispatch {
    /// The answer, ready to encode.
    Reply(Reply),
    /// Work the caller has to run before the answer means anything. The reply to
    /// send is `Ack`; the [`Changed`] signal is what tells clients the answer
    /// changed.
    Index { task: Task },
}

/// One unit of indexing work, self-contained enough to hand to a worker thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Task {
    /// Replace what is indexed under these roots with what is on disk now. The roots
    /// travel with the task, resolved, so a queued crawl indexes what was configured
    /// when it was queued rather than whatever the configuration says by the time a
    /// thread gets to it.
    Crawl { roots: Vec<Root> },
    /// Re-read one directory subtree, leaving the rest of the index alone. Driven by
    /// the file watcher, where a whole re-crawl for one saved file would be absurd.
    Resync { path: PathBuf, content: ContentMode },
    /// Re-read the desktop entries.
    Apps,
}

impl Task {
    /// A crawl of every configured root.
    #[must_use]
    pub fn all(config: &IndexConfig) -> Self {
        Self::Crawl {
            roots: config.roots.clone(),
        }
    }

    /// The task for a `Reindex` call: one root if named, all of them otherwise.
    fn reindex(config: &IndexConfig, root: Option<&str>) -> std::result::Result<Self, WireError> {
        let Some(root) = root else {
            return Ok(Self::all(config));
        };
        let path = Path::new(root);
        config
            .roots
            .iter()
            .find(|configured| configured.path == path)
            .cloned()
            .map(|root| Self::Crawl { roots: vec![root] })
            .ok_or(WireError::Invalid)
    }

    /// Which index a task writes, so the caller can report progress under the right
    /// name and so the engine can open one writer.
    #[must_use]
    pub const fn kind(&self) -> IndexKind {
        match self {
            Self::Crawl { .. } | Self::Resync { .. } => IndexKind::Files,
            Self::Apps => IndexKind::Apps,
        }
    }
}

impl Engine {
    /// Open both indexes where the distro puts them.
    pub fn open(config: IndexConfig) -> Result<Self> {
        Self::at(
            config,
            &ufm_core::paths::index_dir(),
            ufm_index::data_dirs(),
            ufm_index::default_session(),
        )
    }

    /// Open both indexes under `index_root`, reading applications from `apps_dirs`.
    ///
    /// Everything the daemon needs is a path and a configuration file, so this is
    /// also how a test builds an engine over a fixture tree.
    pub fn at(
        config: IndexConfig,
        index_root: &Path,
        apps_dirs: Vec<PathBuf>,
        session: Option<String>,
    ) -> Result<Self> {
        let mut config = config;
        config.normalize();
        let files = IndexHandle::open(IndexKind::Files, &IndexKind::Files.path(index_root))?;
        let apps = IndexHandle::open(IndexKind::Apps, &IndexKind::Apps.path(index_root))?;
        Ok(Self {
            files,
            apps,
            config: Mutex::new(config),
            note: Mutex::new(None),
            busy: AtomicBool::new(false),
            generation: AtomicU64::new(0),
            apps_dirs,
            session,
        })
    }

    /// The configuration in use.
    #[must_use]
    pub fn config(&self) -> IndexConfig {
        self.config
            .lock()
            .map(|config| config.clone())
            .unwrap_or_default()
    }

    /// Whether a crawl is running right now.
    #[must_use]
    pub fn is_busy(&self) -> bool {
        self.busy.load(Ordering::Acquire)
    }

    /// Take the busy flag, or `None` when a crawl already holds it.
    #[must_use]
    pub fn acquire(&self) -> Option<Busy<'_>> {
        self.busy
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .ok()
            .map(|_| Busy { engine: self })
    }

    /// The last problem worth telling the user about.
    #[must_use]
    pub fn note(&self) -> Option<String> {
        self.note.lock().map_or_else(|_| None, |note| note.clone())
    }

    fn set_note(&self, note: Option<String>) {
        if let Ok(mut slot) = self.note.lock() {
            *slot = note;
        }
    }

    /// Decide what a request needs. Never touches the writer, so it cannot block
    /// behind a crawl — it reports that a crawl is running instead.
    pub fn dispatch(&self, request: &Request) -> std::result::Result<Dispatch, WireError> {
        match request {
            Request::SearchFiles { query } => {
                query.validate()?;
                let reply = Self::page(&self.files, query.clone().into())?;
                Ok(Dispatch::Reply(Reply::Search(reply)))
            }
            Request::SearchApps {
                text,
                labels,
                order,
                page,
                page_size,
            } => {
                let query = wire::Query {
                    text: text.clone(),
                    labels: labels.clone(),
                    order: *order,
                    page: *page,
                    page_size: *page_size,
                    // Body text is the point of an application search: "disk usage"
                    // has to find an entry whose comment mentions it, and the name
                    // alone rarely does.
                    content: true,
                    // Application entries live under `/usr/share/applications` and
                    // are never dotfiles, so excluding hidden ones is a filter clause
                    // bought for nothing.
                    include_hidden: true,
                    ..Default::default()
                };
                query.validate()?;
                let reply = Self::page(&self.apps, query.into())?;
                Ok(Dispatch::Reply(Reply::Search(reply)))
            }
            Request::Status => Ok(Dispatch::Reply(Reply::Status(self.status()))),
            Request::Reindex { root } => {
                let config = self.config();
                let task = Task::reindex(&config, root.as_deref())?;
                self.idle(task)
            }
            Request::SetRoots { roots } => {
                let task = self.set_roots(roots)?;
                self.idle(task)
            }
        }
    }

    /// Refuse `task` when a crawl holds the writer, since a second crawl would fail
    /// on tantivy's directory lock and report that instead of the truth.
    fn idle(&self, task: Task) -> std::result::Result<Dispatch, WireError> {
        if self.is_busy() {
            return Err(WireError::Unavailable);
        }
        Ok(Dispatch::Index { task })
    }

    /// Replace the configured roots and say what to index as a result.
    ///
    /// The whole set is replaced rather than added to, because the caller is a
    /// settings page that shows the list and lets the user edit it: an add/remove
    /// pair would need the list to carry identities, and a stale row would then
    /// silently survive.
    pub fn set_roots(&self, roots: &[String]) -> std::result::Result<Task, WireError> {
        let mut config = self.config();
        config.roots = roots
            .iter()
            .map(|root| Root {
                path: PathBuf::from(root),
                content: ContentMode::Off,
                label: None,
            })
            .collect();
        config.normalize();
        if config.roots.len() != roots.len() {
            // Something was dropped: a relative path, or a duplicate. Refusing is
            // kinder than saving a list the settings page no longer shows.
            return Err(WireError::Invalid);
        }
        self.replace_config(config);
        Ok(Task::all(&self.config()))
    }

    /// Change the content mode of one root, leaving the list alone.
    pub fn set_content_mode(
        &self,
        path: &Path,
        mode: ContentMode,
    ) -> std::result::Result<(), WireError> {
        let mut config = self.config();
        let root = config
            .roots
            .iter_mut()
            .find(|root| root.path == path)
            .ok_or(WireError::Invalid)?;
        root.content = mode;
        self.replace_config(config);
        Ok(())
    }

    fn replace_config(&self, config: IndexConfig) {
        if let Ok(mut slot) = self.config.lock() {
            *slot = config;
        }
    }

    /// The answer to `Status`, without going through [`dispatch`](Self::dispatch).
    #[must_use]
    pub fn status(&self) -> StatusReply {
        StatusReply {
            version: wire::PROTOCOL_VERSION,
            indexes: vec![self.files.status(), self.apps.status()],
            roots: self.config().root_paths(),
            busy: self.is_busy(),
            note: self.note(),
        }
    }

    /// Run one task, reporting progress as it goes.
    ///
    /// Returns the [`Changed`] signal body to emit. The caller owns the thread; this
    /// takes the busy flag itself, so a caller that already holds it via
    /// [`acquire`](Self::acquire) will be told the index is busy rather than
    /// deadlocking on the lock.
    pub fn run(&self, task: &Task, progress: &mut dyn FnMut(&Progress)) -> Result<Changed> {
        let guard = self
            .acquire()
            .ok_or_else(|| Error::Search("a crawl is already running".to_string()))?;
        let outcome = match task {
            Task::Crawl { roots } => self.crawl(roots, progress),
            Task::Resync { path, content } => self.resync(path, *content, progress),
            Task::Apps => self.crawl_apps(progress),
        };
        drop(guard);
        outcome
    }

    fn crawl(&self, roots: &[Root], progress: &mut dyn FnMut(&Progress)) -> Result<Changed> {
        let config = self.config();
        let mut writer = self.files.writer(WritePolicy::default())?;
        let mut total = CrawlStats::default();
        for root in roots {
            let policy = config.scan_policy(root.content);
            let chain = chain_for(root.content);
            let scanner = scanner_for(&policy, &chain);
            // Clear first, then read: see `Scanner::resync_dir`. A root that cannot
            // be read ends up with no records, which is the truth about it.
            let stats = scanner.resync_dir(&root.path, &mut writer)?;
            total.indexed += stats.indexed;
            total.directories += stats.directories;
            total.skipped_encoding += stats.skipped_encoding;
            total.pruned += stats.pruned;
            total.unreadable += stats.unreadable;
            total.extracted += stats.extracted;
            progress(&progress_after(&self.files, &root.display(), &total));
        }
        writer.finish()?;
        Ok(self.changed_after(total, IndexKind::Files))
    }

    fn resync(
        &self,
        path: &Path,
        content: ContentMode,
        progress: &mut dyn FnMut(&Progress),
    ) -> Result<Changed> {
        let config = self.config();
        let policy = config.scan_policy(content);
        let chain = chain_for(content);
        let scanner = scanner_for(&policy, &chain);
        let mut writer = self.files.writer(WritePolicy::default())?;
        let stats = scanner.resync_dir(path, &mut writer)?;
        writer.finish()?;
        progress(&progress_after(
            &self.files,
            &path.display().to_string(),
            &stats,
        ));
        Ok(self.changed_after(stats, IndexKind::Files))
    }

    fn crawl_apps(&self, progress: &mut dyn FnMut(&Progress)) -> Result<Changed> {
        let mut writer = self.apps.writer(WritePolicy::default())?;
        let mut crawler = AppCrawler::with_session(self.apps_dirs.clone(), self.session.clone());
        // The set is small and the whole list is returned, so the clear is by path
        // rather than by query: whatever is no longer installed loses its record.
        let entries = crawler.crawl();
        let mut indexed = 0u64;
        for entry in &entries {
            if let Some(record) = entry.record() {
                writer.upsert(&record)?;
                indexed += 1;
            }
        }
        writer.finish()?;
        let stats = CrawlStats {
            indexed,
            unreadable: u64::try_from(crawler.stats().unreadable).unwrap_or(u64::MAX),
            ..CrawlStats::default()
        };
        progress(&progress_after(&self.apps, "applications", &stats));
        Ok(self.changed_after(stats, IndexKind::Apps))
    }

    /// Fold a crawl's numbers into what the settings page will be shown, and build
    /// the signal that tells clients to re-run their searches.
    fn changed_after(&self, stats: CrawlStats, kind: IndexKind) -> Changed {
        let note = if stats.unreadable > 0 {
            Some(format!(
                "{} director{} could not be read",
                stats.unreadable,
                if stats.unreadable == 1 { "y" } else { "ies" }
            ))
        } else {
            None
        };
        self.set_note(note);
        let handle = match kind {
            IndexKind::Files => &self.files,
            IndexKind::Apps => &self.apps,
        };
        let generation = self.generation.fetch_add(1, Ordering::AcqRel) + 1;
        Changed {
            kind: kind.as_str().to_string(),
            generation,
            records: handle.num_records(),
        }
    }

    /// Run a search and shape the answer into a reply that fits one message.
    fn page(
        handle: &IndexHandle,
        request: SearchRequest,
    ) -> std::result::Result<SearchReply, WireError> {
        let hits = search(handle, &request).map_err(|_| WireError::Unavailable)?;
        let mut reply = SearchReply {
            total: hits.total,
            page: hits.page,
            page_size: hits.page_size,
            hits: hits.hits.iter().map(wire_hit).collect(),
            truncated: None,
        };
        fit(&mut reply)?;
        Ok(reply)
    }
}

/// One search phase, for a progress signal.
fn progress_after(handle: &IndexHandle, phase: &str, stats: &CrawlStats) -> Progress {
    Progress {
        kind: handle.kind().as_str().to_string(),
        phase: phase.to_string(),
        done: stats.indexed,
        total: handle.num_records(),
    }
}

/// The extractors a root's mode asks for.
fn chain_for(mode: ContentMode) -> Chain {
    let limit = Limit {
        max_output: MAX_CONTENT_BYTES,
        ..Limit::default()
    };
    match mode {
        ContentMode::Off => Chain::none(),
        ContentMode::Text => Chain::text(limit),
        ContentMode::Rich => Chain::rich(limit),
    }
}

/// A scanner over a chain, with no extractor at all when the mode reads nothing —
/// passing an empty chain would still make the crawler visit every file.
fn scanner_for<'p>(policy: &'p ScanPolicy, chain: &'p Chain) -> Scanner<'p, Chain> {
    if chain.is_empty() {
        Scanner::new(policy, None)
    } else {
        Scanner::new(policy, Some(chain))
    }
}

/// One index hit as it crosses the bus.
fn wire_hit(hit: &ufm_index::Hit) -> Hit {
    Hit {
        path: hit.path.clone(),
        name: hit.name.clone(),
        kind: hit.kind.facet_component().to_string(),
        mime: hit.mime.clone(),
        size: hit.size,
        modified: hit.modified,
        hidden: hit.hidden,
        labels: hit.labels.clone(),
        score: hit.score,
        snippet: hit.snippet.as_ref().map(|snippet| wire::Snippet {
            text: snippet.text.clone(),
            marks: snippet
                .marks
                .iter()
                .map(|(from, to)| [*from, *to])
                .collect(),
        }),
    }
}

/// Shrink a reply until it fits, or say that it cannot.
///
/// Looping here rather than at the call site keeps the "we never send an oversized
/// message" guarantee in one place; every `Reply::Search` leaves this function
/// inside the budget.
fn fit(reply: &mut SearchReply) -> std::result::Result<(), WireError> {
    while reply.encoded_len() > MAX_REPLY {
        if reply.shrink().is_none() {
            return Err(WireError::TooLarge);
        }
    }
    Ok(())
}

/// The order a wire request asks for, exposed so a caller can log what it sent.
#[must_use]
pub const fn order_of(order: wire::Order) -> wire::Order {
    order
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// A fixture: a tree to index, an empty place to put the index, an engine.
    struct Fixture {
        dir: tempfile::TempDir,
        root: PathBuf,
        engine: Engine,
    }

    fn fixture(roots: Vec<Root>) -> Fixture {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let root = dir.path().join("home");
        fs::create_dir_all(root.join("Documents")).expect("mkdir");
        fs::write(root.join("Documents/notes.txt"), "the annual report\n").expect("write");
        fs::write(root.join("todo.md"), "report the annual figures\n").expect("write");
        let config = IndexConfig {
            roots,
            ..Default::default()
        };
        let engine = Engine::at(
            config,
            &dir.path().join("index"),
            vec![dir.path().join("apps")],
            None,
        )
        .expect("an engine over a fixture tree");
        Fixture { dir, root, engine }
    }

    fn reply(engine: &Engine, request: &Request) -> Reply {
        match engine.dispatch(request).expect("a dispatchable request") {
            Dispatch::Reply(reply) => reply,
            Dispatch::Index { .. } => panic!("this request does not index anything"),
        }
    }

    fn search_files(engine: &Engine, query: wire::Query) -> SearchReply {
        match reply(engine, &Request::SearchFiles { query }) {
            Reply::Search(reply) => reply,
            other => panic!("expected a search reply, got {other:?}"),
        }
    }

    fn paths_of(reply: &SearchReply) -> Vec<String> {
        reply.hits.iter().map(|hit| hit.name.clone()).collect()
    }

    /// A crawl task over the fixture tree that reads file bodies, for the tests
    /// whose search words appear only inside them. `ContentMode::Off` would index
    /// the names and find nothing, which is the right behaviour and the wrong test.
    fn read(root: &Path) -> Task {
        Task::Crawl {
            roots: vec![Root {
                path: root.to_path_buf(),
                content: ContentMode::Text,
                label: None,
            }],
        }
    }

    #[test]
    fn a_fresh_engine_answers_status_without_being_crawled() {
        let fixture = fixture(vec![Root::names("/nowhere")]);
        let Reply::Status(status) = reply(&fixture.engine, &Request::Status) else {
            panic!("status asks for status");
        };
        assert_eq!(status.version, wire::PROTOCOL_VERSION);
        assert_eq!(
            status.indexes.len(),
            2,
            "both indexes are reported, even the empty one"
        );
        assert_eq!(status.indexes[0].records, 0);
        assert!(!status.busy);
        assert_eq!(status.roots, vec!["/nowhere"]);
    }

    #[test]
    fn a_crawl_makes_files_findable_and_reports_the_signal() {
        let fixture = fixture(vec![Root::names("/nowhere")]);
        let task = read(&fixture.root);
        let changed = fixture
            .engine
            .run(&task, &mut |_| {})
            .expect("a crawl of the fixture tree");
        assert_eq!(changed.kind, "files");
        assert_eq!(changed.generation, 1);
        assert_eq!(
            changed.records, 4,
            "the root, Documents, notes.txt and todo.md"
        );
        let reply = search_files(
            &fixture.engine,
            wire::Query {
                text: "report".to_string(),
                content: true,
                ..Default::default()
            },
        );
        assert_eq!(reply.total, 2, "{:?}", paths_of(&reply));
        assert!(
            !fixture.engine.is_busy(),
            "the busy flag is released even though the crawl succeeded"
        );
    }

    #[test]
    fn a_second_crawl_while_one_runs_is_refused_as_busy() {
        let fixture = fixture(vec![Root::names("/nowhere")]);
        let task = Task::all(&fixture.engine.config());
        let guard = fixture.engine.acquire().expect("the flag is free");
        let err = fixture
            .engine
            .dispatch(&Request::Reindex { root: None })
            .expect_err("a re-index cannot queue behind another");
        assert_eq!(err, WireError::Unavailable);
        let err = fixture
            .engine
            .run(&task, &mut |_| {})
            .expect_err("and the executor says so too, rather than deadlocking");
        assert!(err.to_string().contains("already running"), "{err}");
        drop(guard);
        assert!(
            matches!(
                fixture.engine.dispatch(&Request::Reindex { root: None }),
                Ok(Dispatch::Index { .. })
            ),
            "and once the flag is released the request is accepted again"
        );
    }

    #[test]
    fn a_recrawl_forgets_a_file_that_was_deleted() {
        let root = Root::names("/nowhere");
        let fixture = fixture(vec![root]);
        let task = Task::Crawl {
            roots: vec![Root::names(fixture.root.display().to_string())],
        };
        fixture
            .engine
            .run(&task, &mut |_| {})
            .expect("the first crawl");
        fs::remove_file(fixture.root.join("todo.md")).expect("rm");
        let changed = fixture
            .engine
            .run(&task, &mut |_| {})
            .expect("the second crawl");
        assert_eq!(
            changed.records, 3,
            "the stale record is gone, not merely unranked"
        );
    }

    #[test]
    fn a_resync_touches_only_the_directory_it_was_asked_about() {
        let fixture = fixture(vec![Root::names("/nowhere")]);
        let task = read(&fixture.root);
        fixture.engine.run(&task, &mut |_| {}).expect("crawl");
        fs::write(fixture.root.join("todo.md"), "changed\n").expect("write");
        fs::write(fixture.root.join("Documents/new.txt"), "changed\n").expect("write");
        let task = Task::Resync {
            path: fixture.root.join("Documents"),
            content: ContentMode::Text,
        };
        fixture.engine.run(&task, &mut |_| {}).expect("resync");
        let reply = search_files(
            &fixture.engine,
            wire::Query {
                text: "changed".to_string(),
                content: true,
                ..Default::default()
            },
        );
        assert_eq!(
            paths_of(&reply),
            vec!["new.txt"],
            "the file outside the resynced folder keeps its old record"
        );
    }

    #[test]
    fn an_unreadable_root_is_a_note_and_not_a_failure() {
        let fixture = fixture(vec![Root::names("/nowhere")]);
        let task = Task::Crawl {
            roots: vec![Root::names("/definitely/not/here")],
        };
        fixture.engine.run(&task, &mut |_| {}).expect("crawl");
        let note = fixture
            .engine
            .note()
            .expect("an unreadable root is worth showing");
        assert!(
            note.contains("1 directory"),
            "the wording has to be usable in a settings page: {note}"
        );
    }

    #[test]
    fn an_app_crawl_finds_an_entry_by_its_description() {
        let fixture = fixture(vec![Root::names("/nowhere")]);
        let apps = fixture.dir.path().join("apps");
        fs::create_dir_all(&apps).expect("mkdir");
        fs::write(
            apps.join("baob.desktop"),
            "[Desktop Entry]\nType=Application\nName=Disk Usage Analyzer\n\
             GenericName=Storage tool\nComment=Scan folders and draw pie charts\n\
             Categories=System;\nExec=baob\n",
        )
        .expect("a fixture entry");
        fixture
            .engine
            .run(&Task::Apps, &mut |_| {})
            .expect("an application crawl");
        let Reply::Search(reply) = reply(
            &fixture.engine,
            &Request::SearchApps {
                text: "pie".to_string(),
                labels: Vec::new(),
                order: wire::Order::default(),
                page: 0,
                page_size: 20,
            },
        ) else {
            panic!("an application search asks for a search reply");
        };
        assert_eq!(reply.total, 1, "{:?}", paths_of(&reply));
        assert_eq!(reply.hits[0].labels, vec!["System"]);
    }

    #[test]
    fn a_reply_is_shrunk_rather_than_refused() {
        let mut reply = SearchReply {
            total: 2,
            page: 0,
            page_size: 200,
            hits: vec![
                Hit {
                    path: "/a".to_string(),
                    name: "a".to_string(),
                    kind: "file".to_string(),
                    mime: None,
                    size: 1,
                    modified: 0,
                    hidden: false,
                    labels: Vec::new(),
                    score: 1.0,
                    snippet: Some(wire::Snippet {
                        text: "s".repeat(MAX_REPLY),
                        marks: Vec::new(),
                    }),
                },
                Hit {
                    path: "/b".to_string(),
                    name: "b".to_string(),
                    kind: "file".to_string(),
                    mime: None,
                    size: 1,
                    modified: 0,
                    hidden: false,
                    labels: Vec::new(),
                    score: 1.0,
                    snippet: None,
                },
            ],
            truncated: None,
        };
        fit(&mut reply).expect("giving up the snippet is enough");
        assert!(reply.hits[0].snippet.is_none());
        assert_eq!(reply.hits.len(), 2, "both results survive");
        assert_eq!(
            reply.truncated.as_deref(),
            Some("snippets"),
            "and the client is told what was given up"
        );
    }

    #[test]
    fn a_small_reply_does_not_claim_to_be_truncated() {
        let mut reply = SearchReply {
            total: 0,
            page: 0,
            page_size: 200,
            hits: Vec::new(),
            truncated: None,
        };
        fit(&mut reply).expect("a small reply fits");
        assert_eq!(reply.truncated, None);
    }

    #[test]
    fn setting_roots_rejects_a_relative_path_and_replaces_the_list() {
        let fixture = fixture(vec![Root::names("/nowhere")]);
        let err = fixture
            .engine
            .set_roots(&["relative".to_string()])
            .expect_err("a relative root names nothing the daemon can read");
        assert_eq!(err, WireError::Invalid);
        assert_eq!(fixture.engine.config().root_paths(), vec!["/nowhere"]);

        fixture
            .engine
            .set_roots(&["/home/u".to_string(), "/srv".to_string()])
            .expect("two absolute roots");
        assert_eq!(
            fixture.engine.config().root_paths(),
            vec!["/home/u", "/srv"]
        );
    }

    #[test]
    fn reindexing_a_root_that_is_not_configured_is_refused() {
        let fixture = fixture(vec![Root::names("/nowhere")]);
        let err = fixture
            .engine
            .dispatch(&Request::Reindex {
                root: Some("/somewhere/else".to_string()),
            })
            .expect_err("the daemon will not crawl a directory nobody asked it to watch");
        assert_eq!(err, WireError::Invalid);
    }

    #[test]
    fn a_reindex_returns_the_work_rather_than_doing_it() {
        let fixture = fixture(vec![Root::names("/nowhere")]);
        let dispatch = fixture
            .engine
            .dispatch(&Request::Reindex { root: None })
            .expect("a re-index of a configured set");
        let Dispatch::Index { task } = dispatch else {
            panic!("a re-index has to hand back work, or the reply would block on it");
        };
        assert_eq!(task, Task::all(&fixture.engine.config()));
        assert_eq!(task.kind(), IndexKind::Files);
        assert!(
            fixture.engine.note().is_none(),
            "deciding to crawl does not change any state"
        );
    }

    #[test]
    fn a_hit_reports_its_kind_in_the_words_the_view_expects() {
        let fixture = fixture(vec![Root::names("/nowhere")]);
        let root = fixture.root.clone();
        fixture
            .engine
            .run(
                &Task::Crawl {
                    roots: vec![Root::names(root.display().to_string())],
                },
                &mut |_| {},
            )
            .expect("crawl");
        let reply = search_files(
            &fixture.engine,
            wire::Query {
                text: "Documents".to_string(),
                ..Default::default()
            },
        );
        let folder = &reply.hits[0];
        assert_eq!(folder.kind, "dir");
        assert_eq!(folder.size, 0, "a folder's size is not its contents");
        assert!(!folder.hidden);
        assert_eq!(folder.path, root.join("Documents").display().to_string());
    }
}
