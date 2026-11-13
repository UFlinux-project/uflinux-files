//! The queue of indexing work, and the one thread that runs it.
//!
//! [`Engine::dispatch`] deliberately does not index: it hands back a [`Task`] and
//! says "run this". This module is where that happens. There is exactly one worker
//! thread, which is what lets the engine hand out tantivy's writer without
//! coordinating with a second one, and it matches the budget in
//! `docs/search-indexer.md`: a crawl is I/O bound on a slow disk, and each additional
//! tantivy writer costs its own eagerly allocated table.
//!
//! The queue coalesces rather than growing. A directory that changes twice needs one
//! re-read, not two, and a second `Reindex` while the first is still queued means the
//! client was impatient rather than that there is more to do. Without coalescing, a
//! `cp -r` of a tree with a thousand folders would queue a thousand re-reads of the
//! same directories, each of which clears and re-lists what the previous one just
//! wrote.
//!
//! Results come back as [`Event`] values handed to a callback the caller owns, which
//! the daemon wires to `glib::MainContext::invoke` so that signal emission happens on
//! the thread that owns the bus connection. The worker never touches GLib itself.

use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;

use ufm_search::{Changed, Engine, Progress, Task};

/// Queued tasks at any one time. Past this, the oldest re-sync is dropped: a
/// directory that changed and was forgotten will be picked up by the periodic
/// rescan, whereas a queued crawl represents something the user asked for.
pub const MAX_PENDING: usize = 64;

/// What the worker has to report. Delivered on the main context.
#[derive(Debug, Clone)]
pub enum Event {
    /// A crawl says what it is doing. Emitted as the `Progress` signal.
    Progress(Progress),
    /// A task committed. Emitted as the `Changed` signal.
    Changed(Changed),
    /// A task failed. Logged, and folded into the `Status` note; a failed crawl
    /// leaves the previous index in place, which is still a usable answer.
    Failed {
        /// `"files"` or `"apps"`, so the message can name the right index.
        kind: &'static str,
        message: String,
    },
}

/// A callback that can be called from the worker thread.
pub type Sink = Arc<dyn Fn(Event) + Send + Sync>;

/// The pending tasks, shared between the main context and the worker.
///
/// `Default` so a daemon that has not opened its indexes yet can still hold one.
#[derive(Default)]
pub struct Queue {
    state: Mutex<State>,
    wake: Condvar,
}

#[derive(Default)]
struct State {
    pending: VecDeque<Task>,
    closed: bool,
}

impl Queue {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a task, collapsing it into an identical one already waiting.
    ///
    /// Returns `false` when the queue was full and there was nothing to evict,
    /// which is the caller's cue to say "try again" rather than to pretend the work
    /// was accepted.
    pub fn push(&self, task: Task) -> bool {
        let mut state = self.lock();
        if state.closed {
            return false;
        }
        if state.pending.contains(&task) {
            return true;
        }
        if state.pending.len() >= MAX_PENDING {
            // The oldest re-sync is the least valuable thing here: it is one
            // directory, and the periodic rescan will find it again.
            match state.pending.iter().position(is_resync) {
                Some(oldest) => {
                    state.pending.remove(oldest);
                }
                None => return false,
            }
        }
        state.pending.push_back(task);
        self.wake.notify_one();
        true
    }

    /// The next task, or `None` once [`close`](Self::close) has been called and the
    /// queue has drained. Blocks otherwise, which is how the worker sleeps between
    /// crawls instead of polling a timer.
    pub fn take_blocking(&self) -> Option<Task> {
        let mut state = self.lock();
        loop {
            if let Some(task) = state.pending.pop_front() {
                return Some(task);
            }
            if state.closed {
                return None;
            }
            state = self
                .wake
                .wait(state)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }

    /// The next task without waiting.
    ///
    /// The worker always wants [`Queue::take_blocking`]. This exists so a test can look
    /// at the backlog without parking, and so a `Status` reply can report how deep the
    /// queue is without waking anyone.
    #[allow(dead_code)]
    pub fn take(&self) -> Option<Task> {
        self.lock().pending.pop_front()
    }

    /// Whether `task` is already waiting, so a caller can tell a client the work is
    /// queued rather than queueing a duplicate.
    ///
    /// Only the tests call this: [`Queue::push`] makes the same check while it already
    /// holds the lock. Kept because a queue whose deduplication cannot be observed is a
    /// queue whose deduplication will be broken by the next change.
    #[allow(dead_code)]
    #[must_use]
    pub fn contains(&self, task: &Task) -> bool {
        self.lock().pending.contains(task)
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.lock().pending.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Wake the worker and stop it once the queue drains.
    pub fn close(&self) {
        self.lock().closed = true;
        self.wake.notify_all();
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// The single indexing thread.
pub struct Worker {
    queue: Arc<Queue>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl Worker {
    /// Start the thread against `queue`. It runs until [`shutdown`](Self::shutdown),
    /// sleeping on the queue rather than on a timer.
    ///
    /// The queue is passed in rather than made here, because the part of the daemon
    /// that decides what to index — the bus handler, the file watcher — holds it too,
    /// and a worker that owned its own queue could not be handed work by either.
    pub fn spawn(queue: Arc<Queue>, engine: Arc<Engine>, sink: Sink) -> Self {
        let thread = std::thread::Builder::new()
            .name("ufm-index".to_string())
            // A stack smaller than tantivy's default would be a surprise deep inside
            // a merge, so the size is set explicitly rather than left to chance.
            .stack_size(2 * 1024 * 1024)
            .spawn({
                let queue = Arc::clone(&queue);
                move || run(engine, queue, sink)
            });
        Self {
            queue,
            thread: Mutex::new(thread.ok()),
        }
    }

    /// Ask the thread to stop and wait for it. Safe to call from the main context:
    /// the worker only ever *queues* work there, it never waits for it, so joining
    /// cannot deadlock against a callback that has not run yet.
    pub fn shutdown(&self) {
        self.queue.close();
        if let Some(handle) = self.handle() {
            let _ = handle.join();
        }
    }

    fn handle(&self) -> Option<JoinHandle<()>> {
        self.thread.lock().ok().and_then(|mut slot| slot.take())
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// The worker loop: take a task, run it, report, repeat.
fn run(engine: Arc<Engine>, queue: Arc<Queue>, sink: Sink) {
    while let Some(task) = queue.take_blocking() {
        let kind = task.kind().as_str();
        let mut report = {
            let sink = Arc::clone(&sink);
            move |progress: &Progress| sink(Event::Progress(progress.clone()))
        };
        match engine.run(&task, &mut report) {
            Ok(changed) => sink(Event::Changed(changed)),
            Err(e) => sink(Event::Failed {
                kind,
                message: e.to_string(),
            }),
        }
    }
}

/// Whether a task is the cheap kind that a periodic rescan will redo anyway, which
/// is what makes it the one to evict when the queue is full.
fn is_resync(task: &Task) -> bool {
    matches!(task, Task::Resync { .. })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use ufm_search::{ContentMode, Root};

    fn resync(path: &str) -> Task {
        Task::Resync {
            path: PathBuf::from(path),
            content: ContentMode::Off,
        }
    }

    fn crawl(path: &str) -> Task {
        Task::Crawl {
            roots: vec![Root::names(path)],
        }
    }

    #[test]
    fn a_task_queued_twice_is_queued_once() {
        let queue = Queue::new();
        assert!(queue.push(resync("/home/u/Documents")));
        assert!(queue.push(resync("/home/u/Documents")));
        assert_eq!(queue.len(), 1);
    }

    #[test]
    fn different_directories_are_different_tasks() {
        let queue = Queue::new();
        assert!(queue.push(resync("/home/u/Documents")));
        assert!(queue.push(resync("/home/u/Downloads")));
        assert_eq!(queue.len(), 2);
    }

    #[test]
    fn a_task_is_taken_in_the_order_it_arrived() {
        let queue = Queue::new();
        queue.push(resync("/a"));
        queue.push(resync("/b"));
        assert_eq!(queue.take(), Some(resync("/a")));
        assert_eq!(queue.take(), Some(resync("/b")));
        assert_eq!(queue.take(), None);
    }

    #[test]
    fn a_full_queue_drops_the_oldest_resync_and_keeps_the_crawl() {
        let queue = Queue::new();
        queue.push(crawl("/home/u"));
        for index in 0..MAX_PENDING {
            queue.push(resync(&format!("/home/u/dir-{index}")));
        }
        assert_eq!(queue.len(), MAX_PENDING);
        // One more: the oldest re-sync goes, the crawl stays, the newcomer is in.
        assert!(queue.push(resync("/home/u/fresh")));
        assert_eq!(queue.len(), MAX_PENDING);
        assert_eq!(queue.take(), Some(crawl("/home/u")));
        assert!(queue.contains(&resync("/home/u/fresh")));
        assert!(!queue.contains(&resync("/home/u/dir-0")));
    }

    #[test]
    fn a_queue_of_only_crawls_refuses_instead_of_losing_one() {
        let queue = Queue::new();
        for index in 0..MAX_PENDING {
            assert!(queue.push(crawl(&format!("/srv/vol-{index}"))));
        }
        // Nothing to evict without throwing away work somebody asked for.
        assert!(!queue.push(crawl("/srv/one-more")));
        assert_eq!(queue.len(), MAX_PENDING);
    }

    #[test]
    fn closing_wakes_a_waiting_worker_and_lets_the_backlog_drain() {
        let queue = Arc::new(Queue::default());
        queue.push(resync("/home/u/first"));
        let taken = Arc::new(Mutex::new(Vec::new()));
        let thread = {
            let queue = Arc::clone(&queue);
            let taken = Arc::clone(&taken);
            std::thread::spawn(move || {
                while let Some(task) = queue.take_blocking() {
                    taken.lock().expect("lock").push(task);
                }
            })
        };
        queue.push(resync("/home/u/second"));
        queue.close();
        thread.join().expect("the worker returns");
        let taken = taken.lock().expect("lock");
        assert_eq!(
            *taken,
            vec![resync("/home/u/first"), resync("/home/u/second")]
        );
    }

    #[test]
    fn nothing_is_accepted_after_close() {
        let queue = Queue::new();
        queue.close();
        assert!(!queue.push(resync("/home/u/late")));
        assert!(queue.is_empty());
    }
}
