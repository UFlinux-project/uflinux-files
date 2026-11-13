//! Keeping the index up to date while the daemon is alive.
//!
//! [`ufm_vfs::watch::Watcher`] is inotify with the three properties that make it usable
//! as an indexer's input rather than a UI's: events are coalesced per directory, a
//! directory is reported only once it has been quiet for 300 ms, and at most
//! [`Watcher::MAX_WATCHES`] directories are watched at all. The cap is the interesting
//! one. A home directory routinely holds hundreds of thousands of subdirectories, so the
//! daemon watches the roots it was configured with plus the application directories — a
//! set that is small by construction — and treats everything below a root as covered by
//! re-reading that one directory whenever it changes.
//!
//! Two cases are not covered by that, and both are handled here rather than ignored:
//!
//! - The kernel's event queue overflowed, which means events were dropped and no watched
//!   directory can be trusted. The response is a crawl of everything, because "something
//!   changed somewhere" is what an overflow actually means.
//! - A root the watcher could not watch at all: the cap was reached, or
//!   `inotify_add_watch` failed on a share that had just been unmounted, or on a
//!   directory the user cannot read. Those are re-read on a timer, which is the bounded
//!   periodic partial rescan `docs/search-indexer.md` promises. Partial is the point:
//!   the watched roots are live and do not need a timer repeating what inotify already
//!   reported.
//!
//! Everything here runs on the main context. The fd source is a *local* one, so the
//! callback runs there and cannot race with [`Live::tick`]. The state is still behind a
//! `Mutex` rather than a `Cell`, because the daemon holds this type in an `Arc` alongside
//! the `Arc` it holds of the service, and an `Arc` around a type that is not `Sync`
//! advertises a sharing that is not available. The worker is reached only by pushing onto
//! its queue, which is what makes that safe.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use ufm_search::{ContentMode, Engine, IndexConfig, Root, Task};
use ufm_vfs::watch::Watcher;

use crate::worker::Queue;

/// How often the roots the watcher could not cover are re-read.
///
/// Long, because the timer exists to catch what inotify cannot rather than to keep up
/// with it: a share that was unmounted and remounted, a directory that only became
/// readable, a root added while the daemon sat idle. Five minutes is short enough that
/// someone who mounts something and walks away finds it indexed later, and long enough
/// that the rescan is not what fills the disk.
pub const RESCAN: Duration = Duration::from_secs(300);

/// What the periodic pass remembers between runs. Built by hand rather than derived:
/// `Instant` has no `Default`, and a pass that is due immediately is the only sensible
/// starting state anyway.
struct Pass {
    /// Which directories the watcher actually accepted, so the rescan timer knows what
    /// it is responsible for.
    covered: Vec<PathBuf>,
    /// When the next pass is due.
    due: Instant,
}

/// Live updates for the configured roots and the application directories.
pub struct Live {
    watcher: Watcher,
    engine: Arc<Engine>,
    queue: Arc<Queue>,
    /// The fd source. Held so it can be removed on drop: dropping a `SourceId` on the
    /// floor leaves the source running, and a source that returns `Continue` needs no
    /// handle for anything else.
    source: Mutex<Option<glib::SourceId>>,
    pass: Mutex<Pass>,
}

impl Live {
    /// Start watching. `None` when inotify or a wake descriptor cannot be created, which
    /// leaves the daemon serving queries from an index that only a `Reindex` call or the
    /// rescan timer will update — the same fallback as a file manager window that cannot
    /// watch the folder it is showing.
    pub fn start(engine: Arc<Engine>, queue: Arc<Queue>) -> Option<Arc<Self>> {
        let watcher = Watcher::new()?;
        let live = Arc::new(Self {
            watcher,
            engine,
            queue,
            source: Mutex::new(None),
            // Due immediately: the first pass is what covers a root that arrived after
            // startup, and waiting five minutes for it would look like a broken watcher.
            pass: Mutex::new(Pass {
                covered: Vec::new(),
                due: Instant::now(),
            }),
        });
        // Weak, so the source cannot keep the watcher alive: the source is owned by
        // `live`, and a strong reference in the callback would be a cycle.
        let weak = Arc::downgrade(&live);
        let source = glib::source::unix_fd_add_local(
            live.watcher.notify_fd(),
            glib::IOCondition::IN,
            move |_, _| match weak.upgrade() {
                Some(live) => {
                    live.on_ready();
                    glib::ControlFlow::Continue
                }
                None => glib::ControlFlow::Break,
            },
        );
        *live.lock() = Some(source);
        live.refresh();
        Some(live)
    }

    /// The periodic pass: re-declare the watch set when the last one is old, then re-read
    /// whatever the watcher is not covering.
    ///
    /// Returns whether anything was queued. Called from the daemon's tick, which runs far
    /// more often than [`RESCAN`]; the interval is decided here rather than by the caller
    /// so that the daemon needs only one timer.
    pub fn tick(&self) -> bool {
        {
            let now = Instant::now();
            let mut pass = self.pass();
            if now < pass.due {
                return false;
            }
            pass.due = now + RESCAN;
        }
        self.refresh();
        let config = self.engine.config();
        let covered = self.pass();
        let mut queued = false;
        let roots = uncovered(&config.roots, &covered.covered);
        if !roots.is_empty() {
            queued |= self.queue.push(Task::Crawl { roots });
        }
        // The application directories are watched rather than polled — they are a
        // handful of paths, and they change when a package is installed, which is exactly
        // when a start menu wants to know. So if one of them is *not* covered, the
        // application index is the thing that has gone stale.
        let stale_apps = candidates(&config, self.engine.apps_dirs())
            .iter()
            .any(|path| !covered.covered.contains(path));
        if stale_apps {
            queued |= self.queue.push(Task::Apps);
        }
        queued
    }

    /// Re-declare the set of directories to watch from the configuration in force.
    ///
    /// Run by [`tick`](Self::tick), which is also what picks up a share mounted while the
    /// daemon was idle or a root added by `SetRoots`: a root that could not be watched
    /// before may be watchable now.
    fn refresh(&self) {
        let config = self.engine.config();
        let wanted = wanted(&config, self.engine.apps_dirs());
        // Anything the configuration no longer mentions stops being watched: a stale
        // watch would keep reporting a directory the user removed from the list.
        for path in self.watcher.watched() {
            if !wanted.contains(&path) {
                self.watcher.unwatch(&path);
            }
        }
        for path in &wanted {
            self.watcher.watch(path);
        }
        // `watch` says nothing about whether it succeeded — it is silent both when the cap
        // is reached and when `inotify_add_watch` fails — so coverage is read back from
        // the watcher rather than assumed from what was asked for.
        self.pass().covered = self.watcher.watched();
    }

    /// Drain the watcher and queue a re-read per directory that settled.
    fn on_ready(&self) {
        let config = self.engine.config();
        for path in self.watcher.ready() {
            let content = content_for(&config, &path);
            let _ = self.queue.push(Task::Resync { path, content });
        }
        if self.watcher.take_overflow() {
            // Events were dropped, so the set of what changed is unknown rather than
            // empty. A crawl is the only honest answer, and the queue coalesces it into
            // one even if an overflow is reported several times.
            let _ = self.queue.push(Task::all(&config));
        }
    }

    fn lock(&self) -> MutexGuard<'_, Option<glib::SourceId>> {
        self.source.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn pass(&self) -> MutexGuard<'_, Pass> {
        self.pass.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        // Removing the source first means no callback can run against a watcher whose
        // thread has already been joined.
        if let Some(source) = self.lock().take() {
            source.remove();
        }
    }
}

/// Every directory the daemon would like to watch: the configured roots, then the places
/// desktop entries live. Duplicates are dropped and the roots keep their order, which is
/// the order the user listed them in.
fn candidates(config: &IndexConfig, apps_dirs: &[PathBuf]) -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = config.roots.iter().map(|root| root.path.clone()).collect();
    for dir in apps_dirs {
        if !paths.contains(dir) {
            paths.push(dir.clone());
        }
    }
    paths
}

/// The directories that can actually be watched right now.
///
/// Asking for a path that is not a directory is not merely wasted: it would leave that
/// path in the uncovered set with no way to ever leave it, so the timer would re-crawl a
/// directory that does not exist every five minutes forever. A directory that appears
/// later is picked up by the next pass.
fn wanted(config: &IndexConfig, apps_dirs: &[PathBuf]) -> Vec<PathBuf> {
    candidates(config, apps_dirs)
        .into_iter()
        .filter(|path| path.is_dir())
        .collect()
}

/// The roots the watcher is not covering, which are the ones the timer has to re-read.
///
/// A root counts as covered only if the watcher accepted that exact path. A parent
/// directory being watched says nothing about a root nested inside it, because the events
/// that matter arrive on the child's descriptor.
fn uncovered(roots: &[Root], covered: &[PathBuf]) -> Vec<Root> {
    roots
        .iter()
        .filter(|root| !covered.contains(&root.path))
        .cloned()
        .collect()
}

/// How much of a changed directory to read: the mode of the deepest configured root it
/// sits under, or names only.
///
/// Deepest rather than first, because a user who indexes `$HOME` by name and
/// `~/Documents` by content has said something specific, and answering a change in
/// `~/Documents/notes` with the outer root's mode would silently drop the text they asked
/// for.
fn content_for(config: &IndexConfig, path: &Path) -> ContentMode {
    config
        .roots
        .iter()
        .filter(|root| path.starts_with(&root.path))
        .max_by_key(|root| root.path.components().count())
        .map_or(ContentMode::Off, |root| root.content)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root(path: &str, content: ContentMode) -> Root {
        Root {
            path: PathBuf::from(path),
            content,
            label: None,
        }
    }

    fn config_with(roots: Vec<Root>) -> IndexConfig {
        IndexConfig {
            roots,
            ..Default::default()
        }
    }

    #[test]
    fn a_root_the_watcher_accepted_is_not_rescanned() {
        let roots = vec![root("/home/u", ContentMode::Off)];
        let covered = vec![PathBuf::from("/home/u")];
        assert!(uncovered(&roots, &covered).is_empty());
    }

    #[test]
    fn a_root_the_watcher_refused_is_rescanned() {
        let roots = vec![
            root("/home/u", ContentMode::Off),
            root("/srv/share", ContentMode::Off),
        ];
        let covered = vec![PathBuf::from("/home/u")];
        assert_eq!(
            uncovered(&roots, &covered),
            vec![root("/srv/share", ContentMode::Off)],
            "the share that could not be watched is the one the timer is for"
        );
    }

    #[test]
    fn watching_a_parent_does_not_cover_a_nested_root() {
        let roots = vec![
            root("/home/u", ContentMode::Off),
            root("/home/u/Docs", ContentMode::Text),
        ];
        let covered = vec![PathBuf::from("/home/u")];
        assert_eq!(
            uncovered(&roots, &covered),
            vec![root("/home/u/Docs", ContentMode::Text)],
            "events for a nested root arrive on its own descriptor, so a parent watch \
             says nothing about it"
        );
    }

    #[test]
    fn a_change_uses_the_mode_of_the_deepest_root_it_sits_under() {
        let config = config_with(vec![
            root("/home/u", ContentMode::Off),
            root("/home/u/Docs", ContentMode::Text),
        ]);
        assert_eq!(
            content_for(&config, Path::new("/home/u/Docs/notes.txt")),
            ContentMode::Text,
            "the inner root's choice wins"
        );
        assert_eq!(
            content_for(&config, Path::new("/home/u/Downloads/x.bin")),
            ContentMode::Off,
            "outside it, the outer root's choice applies"
        );
    }

    #[test]
    fn a_change_outside_every_root_is_indexed_by_name_only() {
        let config = config_with(vec![root("/home/u", ContentMode::Text)]);
        assert_eq!(
            content_for(&config, Path::new("/srv/other/file")),
            ContentMode::Off,
            "nothing was asked for here, so nothing expensive is done"
        );
    }

    #[test]
    fn the_wanted_set_is_the_roots_plus_the_app_directories() {
        let config = config_with(vec![
            root("/home/u", ContentMode::Off),
            root("/usr/share", ContentMode::Off),
        ]);
        let apps = [PathBuf::from("/usr/share/applications")];
        assert_eq!(
            candidates(&config, &apps),
            vec![
                PathBuf::from("/home/u"),
                PathBuf::from("/usr/share"),
                PathBuf::from("/usr/share/applications"),
            ]
        );
    }

    #[test]
    fn an_app_directory_that_is_already_a_root_is_not_asked_for_twice() {
        let config = config_with(vec![root("/usr/share/applications", ContentMode::Off)]);
        let apps = [PathBuf::from("/usr/share/applications")];
        assert_eq!(candidates(&config, &apps).len(), 1);
    }

    #[test]
    fn a_directory_that_does_not_exist_is_not_asked_for() {
        let dir = tempfile::tempdir().expect("a temporary root");
        let real = dir.path().join("applications");
        std::fs::create_dir(&real).expect("create the application directory");
        let missing = dir.path().join("autostart");
        let config = config_with(vec![root("/definitely/not/here", ContentMode::Off)]);
        assert_eq!(
            wanted(&config, &[real.clone(), missing]),
            vec![real],
            "a directory that is not there is left to the timer rather than watched, \
             failed, and retried forever"
        );
    }
}
