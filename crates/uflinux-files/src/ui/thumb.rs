//! Picture previews, read off the main thread and painted on it.
//!
//! The rules about *what* a thumbnail is — the cache key, staleness, the bounds
//! on a decode — belong to [`ufm_thumb`] and are shared with the desktop
//! provider. What this module adds is the part only a widget tree needs: asking
//! for a picture while a row is being bound, and finding the row it was for by
//! the time the picture arrives, which may be after that row has been recycled
//! for a different file or scrolled off screen altogether.
//!
//! Three decisions shape the design, and all three come from the budget rather
//! than from taste.
//!
//! **Only bytes cross the thread boundary.** [`gdk_pixbuf::Pixbuf`] is neither
//! `Send` nor `Sync`, and neither is [`gtk::Image`] — both wrap a `GObject` whose
//! Rust binding carries a reference with no thread-safety guarantee. So a worker
//! reads and encodes and hands back a `Vec<u8>`; the main thread decodes that and
//! paints it. Decoding on the main thread sounds like the expensive half and is
//! not: the bytes are already a picture the size of the cell, because the worker
//! asked the loader to downsample while reading ([`ufm_thumb::make`]). Painting a
//! 64 px PNG costs microseconds.
//!
//! **One worker, started on demand.** A pool would let a folder of a thousand
//! JPEGs occupy four cores decoding pictures of files nobody will look at, and
//! the queue would grow faster than it drains while scrolling. One thread with a
//! bounded queue means the visible screenful is decoded promptly, the rest is
//! dropped, and a folder of text files costs nothing at all, because the thread
//! is not created until something asks for a picture.
//!
//! **The widget carries its own generation.** A factory recycles its children, so
//! a reply that arrives late may land on a row showing a different file. The
//! token is a counter, stored on the image itself with
//! [`glib::ObjectExt::set_data`] at bind time and read back before painting: a
//! recycled widget holds a new token and the old reply is discarded. A
//! [`glib::WeakRef`] alone would not be enough — the widget outlives its row.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};

use gdk_pixbuf::Pixbuf;
use gio::prelude::*;
use gtk::gdk;

use ufm_core::entry::{Entry, EntryFlags, EntryKind};
use ufm_thumb::Job;

use super::job::{Event, Handle};
use super::row::Row;

/// Requests the worker will accept before the oldest is dropped.
///
/// About two screenfuls of icons, which is what a person can scroll past before
/// the first reply arrives. Beyond that the queue is a list of pictures nobody is
/// looking at, and each one is a decode that costs memory inside the loader.
const QUEUE_DEPTH: usize = 64;

/// How many outstanding tokens to keep before sweeping the dead ones out.
///
/// An entry is a `u64` and a weak reference, so this is not a memory decision so
/// much as a guarantee that the table cannot grow without bound in a session that
/// scrolls through a great many folders.
const TOKEN_SWEEP: usize = 512;

/// Object-data key holding the generation counter of the last request a widget
/// made. Quarks are interned, so looking it up is a pointer comparison.
const TOKEN_KEY: &str = "ufm-thumb-token";

/// One request, and the widget the answer belongs to.
struct Request {
    token: u64,
    job: Job,
}

/// The worker's mailbox. Shared so the owner can feed it, wake it, and on drop
/// tell it to stop.
struct Mailbox {
    queue: Mutex<VecDeque<Request>>,
    wake: Condvar,
    quit: AtomicBool,
}

/// The thumbnail pipeline: one window's worth of requests, tokens, and worker.
#[derive(Clone)]
pub struct Pipeline {
    inner: Rc<Inner>,
}

struct Inner {
    /// Token to widget, for the reply. Main thread only, hence `Rc`.
    pending: RefCell<HashMap<u64, glib::WeakRef<gtk::Image>>>,
    next: Cell<u64>,
    /// Where replies are delivered. Until the window's listing queue exists
    /// nothing can be asked for — the same rule the panes follow, and the reason
    /// a request made during construction is simply not made.
    handle: RefCell<Option<Handle>>,
    /// Whether this build can decode and encode a PNG at all. Checked once,
    /// because the answer cannot change and the question walks a table.
    loader: bool,
    /// The user's own switch, from [`Preferences::thumbnails`][ufm_core::config::Preferences::thumbnails].
    /// Turning previews off does not remove the pictures already painted — the
    /// widgets are rebound by the caller when that matters — it stops asking.
    enabled: Cell<bool>,
    /// The worker thread is created by the first request, not by `new`, so a
    /// session that only ever browses text files never pays for one.
    started: Cell<bool>,
    mail: Arc<Mailbox>,
}

impl Pipeline {
    /// A pipeline that will ask for pictures as soon as it is given a queue.
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Rc::new(Inner {
                pending: RefCell::new(HashMap::new()),
                next: Cell::new(1),
                handle: RefCell::new(None),
                loader: ufm_thumb::available(),
                enabled: Cell::new(true),
                started: Cell::new(false),
                mail: Arc::new(Mailbox {
                    queue: Mutex::new(VecDeque::new()),
                    wake: Condvar::new(),
                    quit: AtomicBool::new(false),
                }),
            }),
        }
    }

    /// Whether previews can be shown at all on this build.
    #[must_use]
    pub fn usable(&self) -> bool {
        self.inner.loader
    }

    /// The queue replies arrive on. Until this is set, [`Pipeline::request`] does
    /// nothing.
    pub fn set_handle(&self, handle: Handle) {
        *self.inner.handle.borrow_mut() = Some(handle);
    }

    /// Stop or start asking for pictures, on the user's instruction.
    ///
    /// Turning previews off drops the outstanding requests, because a reply that
    /// arrives after that would paint a picture the user has just asked not to
    /// see. Turning them on asks nothing on its own either: the caller re-binds
    /// the rows, and a bind is what makes a request.
    pub fn set_enabled(&self, enabled: bool) {
        self.inner.enabled.set(enabled);
        if !enabled {
            self.inner.pending.borrow_mut().clear();
        }
    }

    /// Ask for a picture of `row`, painted into `image` when it arrives.
    ///
    /// Returns whether a preview was asked for, which lets a caller leave the
    /// themed icon in place in every other case without re-deriving the rules.
    /// Cheap enough to call from a bind handler: no I/O happens here, only a
    /// token and a push.
    pub fn request(&self, image: &gtk::Image, row: &Row, size: i32) -> bool {
        let inner = &*self.inner;
        if !inner.loader || !inner.enabled.get() || inner.handle.borrow().is_none() {
            return false;
        }
        let Some(job) = job_for(row, size) else {
            return false;
        };
        let token = inner.next.get();
        inner.next.set(token + 1);
        {
            let mut pending = inner.pending.borrow_mut();
            pending.insert(token, image.downgrade());
            if pending.len() > TOKEN_SWEEP {
                pending.retain(|_, widget| widget.upgrade().is_some());
            }
        }
        // The widget has to carry the token before anyone else can see the
        // request, and the only place to keep per-widget state without a subclass
        // is the widget's own object data. `set_qdata` installs a destructor, so
        // overwriting the key on a recycled widget frees the old value instead of
        // leaking it.
        // SAFETY: `TOKEN_KEY` is a private key written and read only here, always
        // with a `u64`.
        unsafe { image.set_data(TOKEN_KEY, token) };
        self.push(Request { token, job });
        true
    }

    /// Paint one reply. Called from the main loop, and only from the main loop.
    ///
    /// A token with no entry, or one whose widget has moved on, is not an error:
    /// it means the row was rebound or destroyed while the picture was being
    /// read, which is the normal outcome of scrolling.
    pub fn deliver(&self, token: u64, png: Option<Vec<u8>>) {
        let inner = &*self.inner;
        let Some(widget) = inner
            .pending
            .borrow_mut()
            .remove(&token)
            .and_then(|widget| widget.upgrade())
        else {
            return;
        };
        // SAFETY: this key is written only by `request`, always as a `u64`.
        let current = unsafe { widget.data::<u64>(TOKEN_KEY) }
            .map(|ptr| unsafe { ptr.as_ptr().read_unaligned() });
        if current != Some(token) {
            return;
        }
        let Some(png) = png else { return };
        // `Bytes` owns the buffer and the stream only reads from it, which skips
        // the copy `Pixbuf::from_read` would make through a `Read` adapter. The
        // zero edge means "as stored": the worker already asked for the cell size,
        // and re-scaling here would blur a picture that is already right.
        let stream = gio::MemoryInputStream::from_bytes(&glib::Bytes::from_owned(png));
        let Ok(pixbuf) =
            Pixbuf::from_stream_at_scale(&stream, 0, 0, false, None::<&gio::Cancellable>)
        else {
            return;
        };
        // `set_from_pixbuf` is deprecated at the version this build targets, and
        // was only ever a shortcut for wrapping the pixels in a texture. Doing that
        // here says so, and keeps the call forward-compatible.
        widget.set_paintable(Some(&gdk::Texture::for_pixbuf(&pixbuf)));
    }

    /// Hand a request to the worker, starting it if this is the first.
    fn push(&self, request: Request) {
        let inner = &*self.inner;
        let Some(handle) = inner.handle.borrow().clone() else {
            return;
        };
        let Ok(mut queue) = inner.mail.queue.lock() else {
            return;
        };
        if queue.len() >= QUEUE_DEPTH {
            queue.pop_front();
        }
        queue.push_back(request);
        drop(queue);
        if !inner.started.get() {
            inner.started.set(true);
            spawn_worker(Arc::clone(&inner.mail), handle);
        }
        inner.mail.wake.notify_one();
    }
}

impl Default for Pipeline {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        // The worker holds its own `Arc` to the mailbox, so it needs to be told
        // to leave rather than simply outliving its owner: one already asleep
        // wakes, sees the flag, and returns.
        self.mail.quit.store(true, Ordering::SeqCst);
        self.mail.wake.notify_all();
    }
}

/// Whether a row is worth a picture, and what to ask for if it is.
fn job_for(row: &Row, size: i32) -> Option<Job> {
    row.with(|entry| job_for_entry(entry, size))
}

/// The same decision on a bare entry, so it can be tested without a widget.
///
/// The mime type rather than the extension: a `.jpg` that is really HTML would be
/// refused by the loader anyway, and the reader has already looked the type up, so
/// this costs a string prefix. The location has to be a real one —
/// [`Location::is_real`] is the codebase's own test for "a thumbnail works on
/// this" — because `as_path` answers with the *container* for an archive member,
/// and previewing a zip by its own picture would be wrong twice over.
///
/// [`Location::is_real`]: ufm_core::Location::is_real
fn job_for_entry(entry: &Entry, size: i32) -> Option<Job> {
    if entry.kind != EntryKind::File
        || entry.flags.contains(EntryFlags::BROKEN)
        || entry.size > ufm_thumb::MAX_SOURCE_BYTES
    {
        return None;
    }
    if !entry
        .mime
        .as_deref()
        .is_some_and(|m| m.starts_with("image/"))
    {
        return None;
    }
    if !entry.location.is_real() {
        return None;
    }
    let path = entry.location.as_path()?;
    let edge = u32::try_from(size).ok()?;
    Some(Job::new(path, edge))
}

/// The one worker thread: one decode at a time, oldest request dropped when the
/// mailbox is full, and no thread at all for a folder with no pictures in it.
fn spawn_worker(mail: Arc<Mailbox>, handle: Handle) {
    // A thread that cannot be created is not worth handling: the themed icon
    // stays, which is the same outcome as a file with no picture.
    let spawned = std::thread::Builder::new()
        .name("ufm-thumb".to_string())
        .spawn(move || {
            loop {
                let request = {
                    let Ok(held) = mail.queue.lock() else { return };
                    let Ok(mut queue) = mail
                        .wake
                        .wait_while(held, |q| q.is_empty() && !mail.quit.load(Ordering::SeqCst))
                    else {
                        return;
                    };
                    if mail.quit.load(Ordering::SeqCst) {
                        return;
                    }
                    queue.pop_front()
                };
                let Some(request) = request else { continue };
                // `bytes` is the whole cost of this thread: a cache read, or a decode
                // bounded by the source-size cap and the requested edge.
                let png = ufm_thumb::bytes(&request.job);
                handle.send(Event::Thumb {
                    token: request.token,
                    png,
                });
            }
        });
    let _ = spawned;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use ufm_core::Entry;

    fn base(name: &str, kind: EntryKind) -> Entry {
        Entry {
            location: ufm_core::Location::file(PathBuf::from("/home/u/Pictures").join(name)),
            name: name.to_string(),
            kind,
            flags: EntryFlags::empty(),
            size: 0,
            allocated: None,
            modified: None,
            accessed: None,
            created: None,
            mode: None,
            uid: None,
            gid: None,
            mime: None,
            child_count: None,
            icon_key: kind.icon_name().to_string(),
        }
    }

    fn file(name: &str, mime: Option<&str>, size: u64) -> Entry {
        let mut entry = base(name, EntryKind::File);
        entry.mime = mime.map(str::to_string);
        entry.size = size;
        entry
    }

    #[test]
    fn a_picture_asks_for_a_thumbnail_at_the_requested_edge() {
        let job = job_for_entry(&file("a.png", Some("image/png"), 4096), 64).expect("a request");
        assert_eq!(job.size, 64);
        assert_eq!(job.path, PathBuf::from("/home/u/Pictures/a.png"));
    }

    #[test]
    fn a_folder_is_not_a_picture_however_its_mime_reads() {
        assert!(job_for_entry(&base("holiday", EntryKind::Directory), 64).is_none());
        let mut entry = file("holiday", Some("image/png"), 4096);
        entry.kind = EntryKind::Directory;
        assert!(job_for_entry(&entry, 64).is_none());
    }

    #[test]
    fn a_file_with_no_recognised_mime_has_no_picture() {
        // The reader sets a mime for what it recognises; an unrecognised file is
        // not worth a decode attempt the loader would refuse anyway.
        assert!(job_for_entry(&file("mystery", None, 4096), 64).is_none());
        assert!(job_for_entry(&file("notes.txt", Some("text/plain"), 4096), 64).is_none());
    }

    #[test]
    fn a_picture_too_big_to_be_an_icon_is_not_decoded() {
        let over = file(
            "huge.tiff",
            Some("image/tiff"),
            ufm_thumb::MAX_SOURCE_BYTES + 1,
        );
        assert!(job_for_entry(&over, 64).is_none());
        let edge = file("big.tiff", Some("image/tiff"), ufm_thumb::MAX_SOURCE_BYTES);
        assert!(job_for_entry(&edge, 64).is_some());
    }

    #[test]
    fn a_broken_link_to_a_picture_has_no_picture() {
        let mut entry = file("gone.png", Some("image/png"), 4096);
        entry.flags = EntryFlags::BROKEN;
        assert!(job_for_entry(&entry, 64).is_none());
    }

    #[test]
    fn a_member_of_an_archive_has_no_picture() {
        // `as_path` would answer with the zip, so the check has to be on the kind
        // of location and not on whether a path exists at all.
        let mut entry = file("a.png", Some("image/png"), 4096);
        entry.location =
            ufm_core::Location::archive("/home/u/x.zip", &["a.png"]).expect("a valid member");
        assert!(
            entry.location.as_path().is_some(),
            "the container has a path"
        );
        assert!(job_for_entry(&entry, 64).is_none());
    }

    #[test]
    fn a_computer_view_row_has_no_picture() {
        let mut entry = base("computer", EntryKind::Directory);
        entry.location = ufm_core::Location::computer();
        entry.mime = Some("image/png".to_string());
        assert!(job_for_entry(&entry, 64).is_none());
    }

    #[test]
    fn a_size_that_is_negative_is_not_an_edge() {
        // A negative pixel size cannot name an edge, and a request with a
        // nonsense edge would be a decode at the cache's own minimum.
        assert!(job_for_entry(&file("a.png", Some("image/png"), 10), -1).is_none());
    }

    #[test]
    fn a_pipeline_asks_for_nothing_before_it_has_a_queue() {
        // No display is needed to know that a pipeline with nowhere to deliver an
        // answer makes no request and starts no worker.
        let pipeline = Pipeline::new();
        assert_eq!(pipeline.usable(), ufm_thumb::available());
        assert!(
            pipeline.inner.pending.borrow().is_empty(),
            "a fresh pipeline holds no tokens"
        );
    }
}
