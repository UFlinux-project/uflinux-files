//! Getting a directory listing off the main thread and back.
//!
//! A read runs on a worker thread and reports through a [`Queue`], which is a
//! mutex-guarded list of events plus a self-pipe wired to a `GLib` fd source.
//! The
//! pipe is why this works at all: [`glib::MainContext::invoke`] needs the
//! closure to be `Send`, and no widget is, so a worker cannot hand a listing to
//! the main loop by calling a closure. It writes one byte instead, and the
//! handler that runs on the main thread takes the events out of the mutex.
//!
//! One worker per navigation, not a pool. A read is either running or
//! cancelled, so the number of live threads is the number of windows plus the
//! one the user just navigated away from, and a thread that outlives its
//! navigation notices the [`Cancel`] flag and stops at the next chunk boundary.

use std::io;
use std::mem;
use std::os::fd::RawFd;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};

use glib::ControlFlow;
use glib::source::{SourceId, unix_fd_add_local};

use ufm_core::entry::Entry;
use ufm_core::location::Location;
use ufm_vfs::{FsHints, Tally};

use crate::runner::Report;
use crate::ui::ops::Outcome;

/// Something a worker wants the main thread to know. The listing variants each
/// name the folder they were read from, because a worker cannot know whether the
/// user has navigated away since the chunk it produced was queued.
#[derive(Debug)]
pub enum Event {
    /// A chunk of a listing that is still being read, in read order.
    Batch {
        location: Location,
        entries: Vec<Entry>,
    },
    /// The complete listing, filtered and sorted. The read is over.
    Finished {
        location: Location,
        entries: Vec<Entry>,
        hints: Option<FsHints>,
    },
    /// The read failed. The message is for the banner, so it is already worded.
    Failed { location: Location, message: String },
    /// A network share was connected, or failed to. The one variant that is not
    /// about a listing: mounting runs off-thread because the backend helper
    /// blocks, so its result has to reach the main loop the same way a listing
    /// does. The [`Report`] carries the share id, which is what lets the window
    /// re-read the mount table and move into the new mount point.
    Mounted { report: Report },
    /// A share was disconnected, or could not be. The same shape as
    /// [`Event::Mounted`] and for the same reason — the helper blocks, so the
    /// answer has to reach the main loop through the pipe — but handled
    /// differently: stepping into the share that just left would land the window
    /// in an empty mount point, so the window repaints and says what happened
    /// instead. `My Computer` is where the answer is read, which is also where
    /// the button that asked for it is.
    Unmounted { report: Report },
    /// A file operation finished. The other variant that is not about a listing,
    /// and shaped on the same principle: a worker that changed something reports
    /// what it did and which folders it left behind, and the window decides
    /// whether that is worth a re-read. The message is shown whatever the
    /// outcome, because an operation that silently did nothing is
    /// indistinguishable from one that is still running.
    Operation { outcome: Outcome },
    /// A picture is ready, or was not to be had. The third variant that is not
    /// about a folder. `token` names the widget that asked — see [`super::thumb`]
    /// — so the answer can be dropped when that widget has since been rebound,
    /// which is the ordinary outcome of scrolling rather than a failure. Only
    /// encoded bytes travel, because a `Pixbuf` is not `Send`.
    Thumb { token: u64, png: Option<Vec<u8>> },
    /// A folder has been walked and its total is known. The fourth variant that
    /// is not about a listing, and the only one whose destination is a dialog:
    /// the properties window asked for the number (see [`super::props`]) and the
    /// window is only the postman. `path` is the folder that was walked, which is
    /// what lets an answer for a dialog the user has since closed be dropped.
    Sized { path: PathBuf, tally: Tally },
    /// One member of an archive has been written out, so the chooser waiting for
    /// it can open. The fifth variant that is not about a folder, and the reason
    /// it carries an id rather than a path is that a `gio::AppInfo` is neither
    /// `Send` nor `Sync`: the list of programs that could take the file cannot
    /// travel to the worker and back, so the worker brings the bytes and the
    /// dialog reads the list itself (see [`super::openwith`]).
    Materialised { id: usize, path: PathBuf },
    /// The head of a text file, read for the preview popover. The sixth variant
    /// that is not about a folder, and shaped the way [`Event::Sized`] is: the
    /// popover asked, the window is only the postman, and `token` names the
    /// popover that asked so an answer for one that has since been dismissed is
    /// dropped rather than painted (see [`super::preview`]). The bytes arrive
    /// already decoded and already cut down, because a worker that read a whole
    /// 2 MiB document into a popover would cost more than the popover is worth.
    Preview { token: u64, text: Option<String> },
    /// Where a file operation has got. The seventh variant that is not about a
    /// folder, and the only one that arrives more than once per job: a worker
    /// that is copying reports every so often (see [`super::jobs::Reporter`],
    /// which is what enforces the pause between reports), and the window folds
    /// each one into the strip it already showed. `report.id` names the job, so
    /// a report that arrives after its job finished is dropped rather than
    /// reopening a strip nobody is waiting for.
    Progress { report: super::jobs::Progress },
    /// A file operation is over, and here is what it did. The same shape as
    /// [`Event::Operation`] — the outcome is worded for the banner and names the
    /// folders it left behind — with a job id attached so the strip can be
    /// struck off by the same event that paints the answer. `Operation` stays for
    /// the refusals and one-shot reports that never had a job to show.
    JobFinished {
        id: super::jobs::JobId,
        outcome: Outcome,
    },
}

/// Closes a file descriptor when the last owner drops it, so a worker that
/// finishes after its window is gone cannot close a descriptor some unrelated
/// code has meanwhile been given the same number for.
struct Fd(RawFd);

impl Drop for Fd {
    fn drop(&mut self) {
        // SAFETY: the descriptor came from `pipe2` and this is its only owner.
        unsafe { libc::close(self.0) };
    }
}

/// The worker's end. Cheap to clone, safe to move to another thread.
#[derive(Clone)]
pub struct Handle {
    out: Arc<Mutex<Vec<Event>>>,
    write: Arc<Fd>,
}

impl Handle {
    /// Queue one event and wake the main loop. A failed write means the queue
    /// is gone — the window closed — and the worker will notice on the next
    /// cancellation check, so there is nothing to report here.
    pub fn send(&self, event: Event) {
        self.out.lock().map(|mut q| q.push(event)).ok();
        let byte: [u8; 1] = [1];
        // SAFETY: `write` is a valid, open descriptor for the pipe.
        let written = unsafe { libc::write(self.write.0, byte.as_ptr().cast(), byte.len()) };
        let _ = written;
    }
}

/// The main thread's end. Dropping it stops the listing from being delivered.
pub struct Queue {
    out: Arc<Mutex<Vec<Event>>>,
    read: Arc<Fd>,
    write: Arc<Fd>,
    /// `Option` because removing a source consumes its id, and a `Drop` impl
    /// cannot move a field out of itself.
    source: Option<SourceId>,
}

impl Queue {
    /// Build a queue and attach its wake-up source to the thread-default main
    /// context, which must be the one GTK runs — call this from the main thread.
    ///
    /// `on_event` is called once per queued event, oldest first, and only for
    /// events that were queued since the last wake-up.
    pub fn new(mut on_event: impl FnMut(Event) + 'static) -> io::Result<Self> {
        let (read, write, out) = pipe()?;
        let watched = Arc::clone(&out);
        let watched_fd = Arc::clone(&read);
        let source = unix_fd_add_local(read.0, glib::IOCondition::IN, move |_fd, _condition| {
            // Drain the pipe so the source does not fire again immediately, and
            // ignore the result: `WouldBlock` is the normal end of a drain.
            let mut buf = [0_u8; 64];
            // SAFETY: `watched_fd` is a valid, open descriptor.
            while unsafe { libc::read(watched_fd.0, buf.as_mut_ptr().cast(), buf.len()) } > 0 {}
            let queued: Vec<Event> =
                mem::take(&mut *watched.lock().unwrap_or_else(PoisonError::into_inner));
            for event in queued {
                on_event(event);
            }
            ControlFlow::Continue
        });
        Ok(Self {
            out,
            read,
            write,
            source: Some(source),
        })
    }

    /// A queue with no fd source attached, for tests that only care about the
    /// pipe and the mutex.
    ///
    /// Attaching a source claims the thread's ownership of the default main
    /// context, and the widget tests hand that to one thread for the whole run —
    /// so a test on a harness thread of its own could not attach one at all. The
    /// source is what turns a write into a callback, and these tests never run a
    /// main loop to receive it.
    #[cfg(test)]
    pub(crate) fn without_source() -> io::Result<Self> {
        let (read, write, out) = pipe()?;
        Ok(Self {
            out,
            read,
            write,
            source: None,
        })
    }

    /// The end to hand a worker.
    #[must_use]
    pub fn handle(&self) -> Handle {
        Handle {
            out: Arc::clone(&self.out),
            write: Arc::clone(&self.write),
        }
    }

    /// Everything queued so far, taken out of the list.
    ///
    /// The window reads its queue from the main-loop source instead; a test that
    /// never runs a loop needs this to see what a worker reported.
    #[cfg(test)]
    pub(crate) fn take(&self) -> Vec<Event> {
        mem::take(&mut *self.out.lock().unwrap_or_else(PoisonError::into_inner))
    }
}

impl Drop for Queue {
    fn drop(&mut self) {
        // The source holds `on_event`, which holds whatever the window gave it.
        // Without this the source outlives the window and keeps it alive.
        if let Some(source) = self.source.take() {
            source.remove();
        }
    }
}

/// What an unset descriptor looks like. `pipe2` fills both slots on success.
const NO_FD: RawFd = -1;

/// The three parts a queue is built from: the read end, the write end, and the
/// event list both sides share.
type Parts = (Arc<Fd>, Arc<Fd>, Arc<Mutex<Vec<Event>>>);

/// The self-pipe and the shared event list, without any main-loop source.
///
/// Split out from [`Queue::new`] so that a test can build a queue on a thread
/// that does not own the default main context.
fn pipe() -> io::Result<Parts> {
    let mut fds = [NO_FD; 2];
    // SAFETY: `fds` is two `c_int`s and `pipe2` writes both on success.
    let ok = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_NONBLOCK | libc::O_CLOEXEC) };
    if ok != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((
        Arc::new(Fd(fds[0])),
        Arc::new(Fd(fds[1])),
        Arc::new(Mutex::new(Vec::new())),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event() -> Event {
        Event::Failed {
            location: Location::file("/nowhere"),
            message: "no".to_string(),
        }
    }

    #[test]
    fn a_handle_can_send_without_a_running_loop() {
        // The queue is dropped straight away, which closes the read end; the
        // write must still not panic, because a worker racing a window close is
        // the common case rather than an edge case.
        let queue = Queue::without_source().expect("a pipe can be made");
        let handle = queue.handle();
        handle.send(event());
        drop(queue);
        handle.send(event());
    }

    #[test]
    fn events_queue_in_order_until_someone_reads_them() {
        let queue = Queue::without_source().expect("a pipe can be made");
        let handle = queue.handle();
        handle.send(event());
        handle.send(event());
        assert_eq!(queue.out.lock().expect("not poisoned").len(), 2);
    }

    #[test]
    fn a_handle_is_send() {
        fn assert_send<T: Send>() {}
        assert_send::<Handle>();
    }
}
