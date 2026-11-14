//! Directory change notifications.
//!
//! inotify is used directly through `libc` rather than through a wrapper crate:
//! the whole requirement is "tell me which directory went dirty", which is about
//! a hundred lines of syscall code, and a crate that spawns its own thread per
//! watch, or allocates an event object per kernel event, costs more than the
//! feature is worth on a low-memory machine.
//!
//! Three properties matter here.
//!
//! Coalescing: a `cp -r` of 5 000 files produces 10 000 events. Each one is
//! folded into a set of dirty directories, so the view re-reads once.
//!
//! Debouncing: a directory is only reported after it has been quiet for
//! [`Watcher::DEBOUNCE`]. Without this, a save loop inside an editor repaints the
//! folder on every write and the UI spends its time re-reading.
//!
//! Bounding: at most [`Watcher::MAX_WATCHES`] directories are watched, which is
//! the visible column chain plus the current folder. Watching a whole home
//! directory recursively would cost one kernel watch per subdirectory, and on a
//! tree with 50 000 folders that is memory the file manager does not have.

// This module is the syscall boundary: every block here is a direct `libc` call
// with arguments this file controls.
#![allow(unsafe_code)]
#![allow(clippy::undocumented_unsafe_blocks)]

use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Events that change what a listing shows. `IN_MODIFY` on a file is excluded on
/// purpose: the size column would then repaint while a file is being written,
/// and the name, kind and icon have not changed.
const MASK: u32 = libc::IN_CREATE
    | libc::IN_DELETE
    | libc::IN_MOVED_FROM
    | libc::IN_MOVED_TO
    | libc::IN_DELETE_SELF
    | libc::IN_MOVE_SELF
    | libc::IN_ATTRIB
    | libc::IN_CLOSE_WRITE;

/// One inotify watch, and the directory it stands for.
struct Watch {
    wd: i32,
    path: PathBuf,
}

#[derive(Default)]
struct State {
    watches: Vec<Watch>,
    /// wd -> path, so an event names a directory without a linear scan.
    by_wd: HashMap<i32, PathBuf>,
    /// Directories that changed, and when they last changed.
    dirty: HashMap<PathBuf, Instant>,
    /// Debounced and ready to hand to the UI.
    ready: VecDeque<PathBuf>,
    /// The kernel queue overflowed, so events were lost and everything must be
    /// re-read rather than trusted.
    overflowed: bool,
    closed: bool,
}

/// Watches a set of directories and reports the ones that changed, debounced and
/// coalesced.
///
/// A single background thread waits on the inotify descriptor. The UI drains
/// [`ready`](Self::ready) when its own idle source fires, or wakes on
/// [`notify_fd`](Self::notify_fd), which is an `EFD_NONBLOCK` descriptor a
/// `GSource` can poll.
pub struct Watcher {
    inotify: RawFd,
    notify: RawFd,
    /// Shutdown signal for the watcher thread. Separate from `notify`, which
    /// belongs to the UI: the thread polls this one so it wakes on shutdown,
    /// and never reads it, so its own wake-ups cannot make it spin.
    stop: RawFd,
    state: Arc<Mutex<State>>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

type RawFd = i32;

impl Watcher {
    /// How long a directory must be quiet before it is reported.
    pub const DEBOUNCE: Duration = Duration::from_millis(300);

    /// Watched directories at any one time. The visible Miller column chain plus
    /// the current folder never exceeds a handful.
    pub const MAX_WATCHES: usize = 16;

    /// Start watching. Returns `None` if the kernel refuses an inotify instance,
    /// which happens when `fs.inotify.max_user_instances` is exhausted; the
    /// caller should then fall back to re-reading on focus or on navigation.
    pub fn new() -> Option<Self> {
        // SAFETY: inotify_init1 has no arguments beyond flags and either returns
        // a new descriptor or fails.
        let inotify = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
        if inotify < 0 {
            return None;
        }
        // SAFETY: eventfd with flags only.
        let notify = unsafe { libc::eventfd(0, libc::EFD_NONBLOCK | libc::EFD_CLOEXEC) };
        if notify < 0 {
            close(inotify);
            return None;
        }
        // A second eventfd, written only on shutdown and read by nobody. Closing
        // a descriptor that another thread is blocked in `poll` on does not
        // reliably wake it, so the thread needs something to become readable.
        // SAFETY: eventfd with flags only.
        let stop = unsafe { libc::eventfd(0, libc::EFD_NONBLOCK | libc::EFD_CLOEXEC) };
        if stop < 0 {
            close(inotify);
            close(notify);
            return None;
        }
        let state = Arc::new(Mutex::new(State::default()));
        let spawned = {
            let state = Arc::clone(&state);
            std::thread::Builder::new()
                .name("ufm-watch".to_string())
                // A watcher thread that cannot start must not stop the file
                // manager; the caller treats None as "no live updates".
                .spawn(move || run(inotify, notify, stop, state))
        };
        let Ok(thread) = spawned else {
            close(inotify);
            close(notify);
            close(stop);
            return None;
        };
        Some(Self {
            inotify,
            notify,
            stop,
            state,
            thread: Mutex::new(Some(thread)),
        })
    }

    /// Watch `path`, replacing any previous watch on it. Silently does nothing
    /// once [`MAX_WATCHES`](Self::MAX_WATCHES) is reached, keeping the oldest set.
    pub fn watch(&self, path: &Path) {
        let Ok(c_path) = CString::new(path.as_os_str().as_bytes()) else {
            return;
        };
        let mut state = self.state();
        if state.watches.iter().any(|w| w.path == path) {
            return;
        }
        if state.watches.len() >= Self::MAX_WATCHES {
            return;
        }
        // SAFETY: c_path is a valid NUL-terminated path and `inotify` is an open
        // descriptor owned by this instance.
        let wd = unsafe { libc::inotify_add_watch(self.inotify, c_path.as_ptr(), MASK) };
        if wd < 0 {
            return; // A remote mount that went away, or a permission hole.
        }
        state.by_wd.insert(wd, path.to_path_buf());
        state.watches.push(Watch {
            wd,
            path: path.to_path_buf(),
        });
    }

    /// Make the watch set exactly `paths`.
    ///
    /// For a view whose watched set is whatever happens to be on screen, which
    /// changes as a whole rather than one directory at a time. Watches that are
    /// in both sets are left alone rather than removed and re-added: a watch that
    /// is briefly gone misses whatever happened in that instant, and the point of
    /// this method is to keep up with a listing without losing events.
    ///
    /// Directories past [`MAX_WATCHES`](Self::MAX_WATCHES) are not watched, same
    /// as [`watch`](Self::watch), so the set can end up smaller than asked.
    pub fn sync(&self, paths: &[PathBuf]) {
        let wanted: HashSet<&PathBuf> = paths.iter().collect();
        let stale: Vec<PathBuf> = self
            .watched()
            .into_iter()
            .filter(|path| !wanted.contains(path))
            .collect();
        for path in stale {
            self.unwatch(&path);
        }
        for path in paths {
            self.watch(path);
        }
    }

    /// Stop watching `path` and drop any pending change for it.
    pub fn unwatch(&self, path: &Path) {
        let mut state = self.state();
        let keep: Vec<Watch> = std::mem::take(&mut state.watches)
            .into_iter()
            .filter(|w| {
                if w.path == path {
                    // SAFETY: wd belongs to this instance's descriptor.
                    unsafe { libc::inotify_rm_watch(self.inotify, w.wd) };
                    state.by_wd.remove(&w.wd);
                    state.dirty.remove(&w.path);
                    false
                } else {
                    true
                }
            })
            .collect();
        state.watches = keep;
    }

    #[must_use]
    pub fn watched(&self) -> Vec<PathBuf> {
        self.state()
            .watches
            .iter()
            .map(|w| w.path.clone())
            .collect()
    }

    /// Directories that changed and have settled, oldest first. Each appears at
    /// most once per call.
    pub fn ready(&self) -> Vec<PathBuf> {
        let mut state = self.state();
        let out: Vec<PathBuf> = state.ready.drain(..).collect();
        drop(state);
        self.drain_notify();
        out
    }

    /// Every watched directory, when the kernel dropped events and nothing can be
    /// trusted. Reported once.
    pub fn take_overflow(&self) -> bool {
        let mut state = self.state();
        std::mem::replace(&mut state.overflowed, false)
    }

    /// A descriptor that becomes readable when [`ready`](Self::ready) has
    /// something. Lets the UI sleep instead of polling.
    #[must_use]
    pub fn notify_fd(&self) -> RawFd {
        self.notify
    }

    fn drain_notify(&self) {
        let mut buf = [0u8; 8];
        // SAFETY: writing to a buffer we own, on a descriptor we own.
        unsafe {
            libc::read(self.notify, buf.as_mut_ptr().cast(), buf.len());
        }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        self.state().closed = true;
        // Waking the thread is what asks it to stop. Closing a descriptor another
        // thread is blocked on does not reliably return from `poll`, so the stop
        // eventfd is written first and the descriptors are closed only once the
        // thread has returned, which also keeps them from being reused under it.
        wake(self.stop);
        if let Some(handle) = self.thread.lock().ok().and_then(|mut t| t.take()) {
            let _ = handle.join();
        }
        close(self.inotify);
        close(self.notify);
        close(self.stop);
    }
}

fn close(fd: RawFd) {
    // SAFETY: fd is an open descriptor owned by the caller.
    unsafe { libc::close(fd) };
}

/// The watcher thread: wait for events, fold them into the dirty set, and flush
/// a directory once it has been quiet for [`Watcher::DEBOUNCE`].
///
/// `stop` is polled alongside inotify and is what ends the loop; it is never
/// read, so the thread simply re-checks the closed flag every time it fires.
fn run(inotify: RawFd, notify: RawFd, stop: RawFd, state: Arc<Mutex<State>>) {
    let mut buffer = [0u8; 8192];
    while let Some(timeout) = wait_timeout(&state) {
        let mut pfd = [
            libc::pollfd {
                fd: inotify,
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: stop,
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        // SAFETY: `pfd` is a valid array of the given length.
        let ready =
            unsafe { libc::poll(pfd.as_mut_ptr(), pfd.len().try_into().unwrap_or(0), timeout) };
        if ready < 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            break;
        }
        if ready > 0 && pfd[0].revents != 0 && !drain_events(inotify, &state, &mut buffer) {
            break; // The descriptor is gone; the Watcher was dropped.
        }
        flush(&state, notify);
    }
}

/// How long to wait: the remaining debounce of the soonest dirty directory, or
/// -1 to block until an event arrives. `None` means the watcher is shutting down.
fn wait_timeout(state: &Arc<Mutex<State>>) -> Option<i32> {
    let guard = state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if guard.closed {
        return None;
    }
    let wait = guard
        .dirty
        .values()
        .map(|when| {
            let elapsed = when.elapsed();
            if elapsed >= Watcher::DEBOUNCE {
                Duration::ZERO
            } else {
                // Saturating rather than plain subtraction: the branch above makes
                // the result exact, and a `Duration` that went negative between the
                // comparison and the arithmetic would panic instead of polling
                // immediately, which is the same answer.
                Watcher::DEBOUNCE.saturating_sub(elapsed)
            }
        })
        .min()
        .unwrap_or(Duration::MAX);
    Some(if wait == Duration::MAX {
        -1
    } else {
        wait.as_millis().try_into().unwrap_or(1)
    })
}

/// Read every queued event into the dirty set. Returns false when the descriptor
/// has been closed.
fn drain_events(inotify: RawFd, state: &Arc<Mutex<State>>, buffer: &mut [u8]) -> bool {
    loop {
        // SAFETY: buffer is a writable slice of the given size on a valid fd.
        let n = unsafe { libc::read(inotify, buffer.as_mut_ptr().cast(), buffer.len()) };
        match n {
            0 => return true,
            -1 => {
                // Only a descriptor that has gone away means the Watcher was
                // dropped. EAGAIN means the queue is drained, and anything else is
                // transient, so both keep the thread alive.
                let err = std::io::Error::last_os_error();
                return !matches!(err.raw_os_error(), Some(libc::EBADF | libc::EINVAL));
            }
            count => {
                let Ok(len) = usize::try_from(count) else {
                    return true;
                };
                consume(&buffer[..len], state);
            }
        }
    }
}

/// Walk one `read` worth of `inotify_event` records.
fn consume(bytes: &[u8], state: &Arc<Mutex<State>>) {
    // `{ wd: i32, mask: u32, cookie: u32, len: u32 }`, so 16 bytes of four
    // 32-bit fields. Read as scalars rather than by casting the slice: the kernel
    // aligns what it writes, but a `&mut [u8]` buffer makes no such promise, and
    // this skips the unsafe entirely.
    const HEADER: usize = 16;
    const _: () = assert!(std::mem::size_of::<libc::inotify_event>() == HEADER);
    let mut guard = state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut rest = bytes;
    while rest.len() >= HEADER {
        let Some(wd) = rest
            .get(0..4)
            .and_then(|b| i32::from_ne_bytes(b.try_into().ok()?).into())
        else {
            break;
        };
        let mask = rest
            .get(4..8)
            .map_or(0, |b| u32::from_ne_bytes(b.try_into().unwrap_or([0; 4])));
        let name_len = rest.get(12..16).map_or(0, |b| {
            usize::try_from(u32::from_ne_bytes(b.try_into().unwrap_or([0; 4]))).unwrap_or(0)
        });
        let len = HEADER + name_len;
        if len > rest.len() {
            break;
        }
        if mask & libc::IN_Q_OVERFLOW != 0 {
            guard.overflowed = true;
        }
        if mask & libc::IN_IGNORED != 0 {
            // The kernel removed the watch, usually because the directory itself
            // went away. Report it once so the view can leave or show an error.
            if let Some(path) = guard.by_wd.remove(&wd) {
                guard.watches.retain(|w| w.wd != wd);
                guard.dirty.entry(path).or_insert_with(Instant::now);
            }
        } else if let Some(path) = guard.by_wd.get(&wd) {
            let path = path.clone();
            guard.dirty.insert(path, Instant::now());
        }
        rest = &rest[len..];
    }
}

/// Move directories that have been quiet long enough into the ready queue.
fn flush(state: &Arc<Mutex<State>>, notify: RawFd) {
    let mut guard = state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if guard.dirty.is_empty() {
        return;
    }
    let settled: Vec<PathBuf> = guard
        .dirty
        .iter()
        .filter(|(_, when)| when.elapsed() >= Watcher::DEBOUNCE)
        .map(|(path, _)| path.clone())
        .collect();
    if settled.is_empty() {
        return;
    }
    let mut seen = HashSet::new();
    for path in settled {
        guard.dirty.remove(&path);
        // A directory can be both watched and a parent of a watched directory;
        // reporting it twice would re-read it twice.
        if seen.insert(path.clone()) && !guard.ready.contains(&path) {
            guard.ready.push_back(path);
        }
    }
    drop(guard);
    wake(notify);
}

fn wake(notify: RawFd) {
    let one: u64 = 1;
    // SAFETY: writing eight bytes to an eventfd we own.
    unsafe {
        libc::write(notify, (&raw const one).cast(), 8);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ufm_core::paths;

    /// A directory that no other test touches.
    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ufm-watch-{}-{tag}", paths::uid()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Poll `ready` until `dir` shows up or the deadline passes. The watcher runs
    /// on its own thread and reports dirty directories rather than names, so a
    /// test must wait rather than assume, and cannot assert which file changed.
    fn wait_for(watcher: &Watcher, dir: &Path) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            let ready = watcher.ready();
            if ready.iter().any(|p| p == dir) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    #[test]
    fn a_new_file_is_reported_once_its_directory_settles() {
        let dir = scratch("create");
        let watcher = Watcher::new().expect("inotify should be available");
        watcher.watch(&dir);
        assert_eq!(watcher.watched(), vec![dir.clone()]);

        std::fs::write(dir.join("new.txt"), b"x").unwrap();
        assert!(
            wait_for(&watcher, &dir),
            "a create must be reported after the debounce"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_burst_of_writes_coalesces_into_one_report() {
        let dir = scratch("coalesce");
        let watcher = Watcher::new().unwrap();
        watcher.watch(&dir);

        for i in 0..200 {
            std::fs::write(dir.join(format!("f{i}")), b"x").unwrap();
        }
        assert!(wait_for(&watcher, &dir));
        // Whatever else arrives, the directory must not be reported repeatedly.
        let second = watcher.ready();
        assert!(
            !second.iter().any(|p| p == &dir),
            "a settled directory is reported exactly once"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_unwatched_directory_is_silent() {
        let watched = scratch("on");
        let other = scratch("off");
        let watcher = Watcher::new().unwrap();
        watcher.watch(&watched);

        std::fs::write(other.join("x"), b"").unwrap();
        let deadline = Instant::now() + Duration::from_millis(600);
        while Instant::now() < deadline {
            assert!(!watcher.ready().iter().any(|p| p == &other));
            std::thread::sleep(Duration::from_millis(20));
        }
        std::fs::remove_dir_all(&watched).unwrap();
        std::fs::remove_dir_all(&other).unwrap();
    }

    #[test]
    fn unwatching_removes_the_directory_from_the_set() {
        let dir = scratch("unwatch");
        let watcher = Watcher::new().unwrap();
        watcher.watch(&dir);
        watcher.unwatch(&dir);
        assert!(watcher.watched().is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_watch_set_is_bounded() {
        let dir = scratch("bound");
        let watcher = Watcher::new().unwrap();
        for i in 0..(Watcher::MAX_WATCHES * 3) {
            let sub = dir.join(format!("d{i}"));
            std::fs::create_dir_all(&sub).unwrap();
            watcher.watch(&sub);
        }
        assert!(
            watcher.watched().len() <= Watcher::MAX_WATCHES,
            "{} watches, the cap is {}",
            watcher.watched().len(),
            Watcher::MAX_WATCHES
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn syncing_replaces_the_whole_set() {
        let keep = scratch("sync-keep");
        let gone = scratch("sync-gone");
        let fresh = scratch("sync-fresh");
        let watcher = Watcher::new().unwrap();
        watcher.watch(&keep);
        watcher.watch(&gone);

        watcher.sync(&[keep.clone(), fresh.clone()]);

        let mut want = vec![keep.clone(), fresh.clone()];
        want.sort();
        let mut got = watcher.watched();
        got.sort();
        assert_eq!(got, want, "the set becomes exactly what was asked for");
        std::fs::remove_dir_all(&keep).unwrap();
        std::fs::remove_dir_all(&gone).unwrap();
        std::fs::remove_dir_all(&fresh).unwrap();
    }

    #[test]
    fn syncing_with_a_directory_that_cannot_be_watched_keeps_the_rest() {
        let dir = scratch("sync-missing");
        let watcher = Watcher::new().unwrap();
        watcher.watch(&dir);

        watcher.sync(&[
            dir.clone(),
            PathBuf::from("/definitely-not-a-directory-ufm-sync"),
        ]);

        assert_eq!(
            watcher.watched(),
            vec![dir.clone()],
            "a directory the kernel refuses is left out rather than failing the set"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_missing_directory_is_ignored_not_fatal() {
        let watcher = Watcher::new().unwrap();
        watcher.watch(Path::new("/definitely-not-a-directory-ufm-watch"));
        assert!(
            watcher.watched().is_empty(),
            "adding a watch that fails leaves no trace"
        );
    }

    #[test]
    fn dropping_the_watcher_stops_its_thread() {
        let dir = scratch("drop");
        let watcher = Watcher::new().unwrap();
        watcher.watch(&dir);
        drop(watcher);
        // If the thread did not stop, the descriptor would be closed under it and
        // the next test would see a spurious error. Reaching here is the assert.
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_truncated_event_buffer_is_survivable() {
        // A hand-built buffer whose last record claims to be longer than it is.
        let mut bytes = Vec::new();
        let header = libc::inotify_event {
            wd: 1,
            mask: libc::IN_CREATE,
            cookie: 0,
            len: 0,
        };
        // SAFETY: inotify_event is plain bytes with no padding requirements beyond
        // its own alignment, and we are copying it into a byte vector.
        bytes.extend_from_slice(unsafe {
            std::slice::from_raw_parts(
                (&raw const header).cast::<u8>(),
                std::mem::size_of::<libc::inotify_event>(),
            )
        });
        let state = Arc::new(Mutex::new(State::default()));
        consume(&bytes, &state);
        assert!(
            state.lock().unwrap().dirty.is_empty(),
            "an unknown wd must not invent a path"
        );
        consume(&[0, 1, 2], &state);
        consume(&[], &state);
    }
}
