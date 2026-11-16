//! The record of what the window has running, and the strip that says so.
//!
//! A copy that takes four seconds used to report once, at the end, which made a
//! working program and a hung one look identical from the outside. This module is
//! the answer to that: an operation that can take a while registers a [`Job`]
//! here before it starts, reports into it as it goes, and is struck off when it
//! reports its outcome. The strip is the visible half. The table underneath is
//! also what a later undo will read, which is the other reason the record lives
//! here rather than inside whichever widget started the work.
//!
//! The table is main-thread state, and that is a decision rather than an
//! accident. The conflict prompt can block a paste inside a nested main loop
//! (see [`super::conflict`]), so a record kept on the worker would be written
//! from two places at once; instead a worker only ever sends [`Progress`]
//! reports down the queue, and the main thread folds them in. The one thing that
//! does cross threads is the [`Brake`], because a button has to be able to stop
//! a worker that is listening to nothing else — `GIO`'s cancellable and the
//! crate's own [`ufm_vfs::Cancel`] flag are both cheap to share, and a worker
//! already knows how to watch the second.
//!
//! The widgets are built by [`Jobs::install`] rather than by [`Jobs::new`], so
//! that the record exists on its own: an operation started before a window has
//! drawn anything still gets a row in the table, and a test of the bookkeeping
//! needs no display.

use std::cell::{Cell, RefCell, RefMut};
use std::fmt;
use std::time::{Duration, Instant};

use gio::prelude::*;
use gtk::prelude::*;
use ufm_core::entry::human_size;
use ufm_vfs::Cancel;

use super::job::{Event, Handle};
use super::undo::Record;

/// How often a worker may report. The queue behind a report is a self-pipe — one
/// event costs a write and a main-loop wake-up — so a copy of a hundred thousand
/// files would spend more on announcements than on copying if every chunk of
/// every file produced one. A report dropped for timing is not lost: the strip
/// keeps showing the last one that arrived, and the finish report always gets
/// through.
const THROTTLE: Duration = Duration::from_millis(100);

/// What a running job is doing, in the one word the strip needs. The record
/// keeps the word rather than the kind, so the strip can paint without knowing
/// what an operation was — only what it should be called.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Copying rows to a new folder.
    Copy,
    /// Moving rows, which is what a cut means.
    Move,
    /// Sending rows to the trash.
    Trash,
    /// Removing rows for good.
    Delete,
    /// Unpacking a container.
    Extract,
    /// Putting the last operation back. Its own kind rather than a reuse of
    /// `Move`, because restoring from the trash moves rows that were not moved
    /// out in the first place and the strip's word has to match what the bar is
    /// counting.
    Undo,
}

impl Kind {
    /// The present-tense word for the strip.
    #[must_use]
    pub fn gerund(self) -> &'static str {
        match self {
            Self::Copy => "Copying",
            Self::Move => "Moving",
            Self::Trash => "Moving to trash",
            Self::Delete => "Deleting",
            Self::Extract => "Extracting",
            Self::Undo => "Undoing",
        }
    }
}

/// What a job's `done` and `total` count. The worker knows which one it is before
/// it writes anything — a paste of plain files has a size for every row already,
/// a paste containing a folder does not — and the strip cannot guess it, because
/// `7 of 9` and `7 MiB of 9 MiB` are the same two numbers read two ways.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unit {
    /// Rows, files, members: things that are counted whole.
    Rows,
    /// Bytes, spelled as a size on the strip.
    Bytes,
}

/// Which job a report belongs to. Opaque on purpose: the id is handed out by
/// [`Jobs::begin`] and only ever compared, never read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct JobId(u64);

/// The cancel handle for one running job, shared between the window that shows
/// the button and the worker that watches the flag.
///
/// Two flags behind one handle, because the two halves of the work watch
/// different ones: `GIO`'s copy and move calls take a [`gio::Cancellable`], and
/// everything this program loops over itself — the directory recursion, the
/// archive member walk — already watches a [`ufm_vfs::Cancel`]. Pulling the
/// handle fires both, so one button stops an operation whichever half it is
/// currently in. Both halves are cheap to share: `GIO`'s flag is reference
/// counted and thread safe, and the crate's is an `Arc<AtomicBool>`.
#[derive(Debug, Clone)]
pub struct Brake {
    gio: gio::Cancellable,
    vfs: Cancel,
}

impl Brake {
    /// A brake nobody has pulled. Normally handed out by [`Jobs::begin`]; crate
    /// public so an operation that has no job to show can still be cancellable
    /// from inside its own worker.
    #[must_use]
    pub(crate) fn new() -> Self {
        Self {
            gio: gio::Cancellable::new(),
            vfs: Cancel::default(),
        }
    }

    /// Stop the job. Safe to pull twice, and safe to pull after the work is
    /// over — a finished `GIO` call ignores the flag, and a worker that has
    /// already reported will not look at it again.
    pub fn pull(&self) {
        self.gio.cancel();
        self.vfs.cancel();
    }

    /// The flag for `GIO` calls. Callers wrap it in `Some` at the argument
    /// position, which takes an optional one because a call with no way to be
    /// cancelled is a legal call.
    #[must_use]
    pub fn gio(&self) -> &gio::Cancellable {
        &self.gio
    }

    /// The flag for this program's own loops.
    #[must_use]
    pub fn vfs(&self) -> &Cancel {
        &self.vfs
    }

    /// Whether the button has been pulled. The worker asks this after each step,
    /// both to stop early and to word its report: an operation the user stopped
    /// is not one that failed, and saying "failed" reads as though something
    /// broke.
    #[must_use]
    pub fn pulled(&self) -> bool {
        self.gio.is_cancelled()
    }
}

/// One word from a worker about a job it is running: what it is touching now,
/// how far along it is, and how far along it expects to be.
///
/// `done` and `total` count bytes for a byte job and rows for a row job, and
/// which one this is is the worker's business — the strip prints them the way
/// the worker said. A paste of plain files counts bytes, because the listing
/// already knew every size; a paste containing a folder counts rows, because
/// the size of a tree is not known until it has been walked and inventing one
/// would make the bar lie.
#[derive(Debug, Clone)]
pub struct Progress {
    /// Which job this is about. A report for a job that has since finished is
    /// dropped, which is the ordinary end of a job that finished while its last
    /// report was still in the pipe.
    pub id: JobId,
    /// What the job is at right now — the row being written, the container being
    /// unpacked. Shown as-is, so it has to read well on its own.
    pub detail: String,
    /// How much has been written, or how many rows are done.
    pub done: u64,
    /// The whole, when it is knowable ahead of time. `None` is the honest answer
    /// for a folder whose contents have not been listed, and for a helper
    /// command doing the work out of sight; the strip shows a moving bar for
    /// those rather than a percentage that would be invented.
    pub total: Option<u64>,
    /// What `done` and `total` count.
    pub unit: Unit,
}

/// The worker's pen. Reports go down the queue like everything else, and the
/// throttle lives here rather than in the callers so that no operation can
/// forget it and flood the pipe.
///
/// The first report goes out whatever the clock says: a job that takes two
/// seconds should say so within its first moment, and a job that takes two
/// hundred milliseconds only ever produces that first one anyway.
pub struct Reporter {
    handle: Handle,
    id: JobId,
    last: Option<Instant>,
}

impl Reporter {
    #[must_use]
    pub(crate) fn new(handle: &Handle, id: JobId) -> Self {
        Self {
            handle: handle.clone(),
            id,
            last: None,
        }
    }

    /// Say where the job has got, unless it said so less than [`THROTTLE`] ago.
    pub fn update(&mut self, detail: &str, done: u64, total: Option<u64>, unit: Unit) {
        let now = Instant::now();
        if self
            .last
            .is_some_and(|then| now.duration_since(then) < THROTTLE)
        {
            return;
        }
        self.last = Some(now);
        self.handle.send(Event::Progress {
            report: Progress {
                id: self.id,
                detail: detail.to_string(),
                done,
                total,
                unit,
            },
        });
    }
}

/// One running operation, as the table knows it.
#[derive(Debug)]
struct Job {
    id: JobId,
    /// `"Copying"`, `"Extracting"` — see [`Kind`].
    gerund: &'static str,
    detail: String,
    done: u64,
    total: Option<u64>,
    unit: Unit,
    brakes: Vec<Brake>,
}

impl Job {
    /// The strip's label: what is happening, to what.
    fn label(&self) -> String {
        format!("{} {}", self.gerund, self.detail)
    }

    /// The bar's text: `4 of 9`, or `512 KiB of 1.0 MiB`. Nothing when the total
    /// is not knowable — a moving bar says that better than words would.
    fn tally(&self) -> Option<String> {
        let total = self.total?;
        let text = match self.unit {
            Unit::Bytes => format!("{} of {}", human_size(self.done), human_size(total)),
            Unit::Rows => format!("{} of {}", self.done, total),
        };
        Some(text)
    }

    fn fraction(&self) -> f64 {
        match self.total {
            Some(total) if total > 0 => (self.done as f64 / total as f64).clamp(0.0, 1.0),
            _ => 0.0,
        }
    }
}

/// The widgets, once the window has asked for them.
#[derive(Debug)]
struct Strip {
    root: gtk::Box,
    label: gtk::Label,
    bar: gtk::ProgressBar,
}

/// What the table can answer about itself at the moment something changed: is
/// anything running, and is there an operation to put back. Both go out together
/// because both are read by the same callback, which sets two action sensitivities
/// and costs one call either way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Status {
    /// Whether pulling the cancel button would stop something.
    pub running: bool,
    /// Whether the Undo button has anything to undo.
    pub undoable: bool,
}

/// What the table calls when the answer to either of those changes.
///
/// Named rather than written out at the field, because the type as written is
/// long enough that clippy calls it complex, and the name says what the boxed
/// closure is for.
type OnChange = Box<dyn FnMut(Status)>;

/// The table of running jobs and the strip that shows them. One per window,
/// owned by the window's handlers the way the popup and the preview are.
pub struct Jobs {
    next: Cell<u64>,
    /// In the order they started, which is the order the strip prefers: the
    /// oldest job is the one the user is still waiting for. A handful of entries
    /// at most — only the operations that spawn a worker register here — so a
    /// `Vec` beats a map and keeps the choice deterministic.
    runs: RefCell<Vec<Job>>,
    /// The last operation that can be put back, one deep. Kept here rather than
    /// on the window because the table is already the thing that hears about
    /// operations finishing, and already the thing the strip's sensitivity is
    /// read from.
    ///
    /// One rather than a stack: each new record sweeps the previous one's stash,
    /// so what a session holds is bounded by the last operation's displaced files
    /// rather than by everything the window has done since it opened.
    record: RefCell<Option<Record>>,
    /// `None` until [`Jobs::install`], which is what lets the record exist in a
    /// program with no window, and in a test with no display.
    strip: RefCell<Option<Strip>>,
    /// Called whenever the answer to "is anything running?" or "is there anything
    /// to undo?" changes, which is how the window keeps the two actions sensitive
    /// exactly when pulling them would do something.
    on_change: RefCell<Option<OnChange>>,
}

impl Jobs {
    /// An empty table with no strip. Nothing runs until [`Jobs::begin`].
    #[must_use]
    pub fn new() -> Self {
        Self {
            next: Cell::new(1),
            runs: RefCell::new(Vec::new()),
            record: RefCell::new(None),
            strip: RefCell::new(None),
            on_change: RefCell::new(None),
        }
    }

    /// Build the strip, wire its button to `win.cancel`, and put it under
    /// `parent`. Where in the window it goes is the window's decision; the strip
    /// only knows it is hidden while nothing is running.
    ///
    /// The button names the action rather than carrying a handler, so that the
    /// keyboard, the menu and the mouse all reach the cancellation by the same
    /// door and the button's sensitivity is `GTK`'s to maintain.
    pub fn install(&self, parent: &gtk::Box) {
        let root = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        root.add_css_class("ufm-jobs");
        root.set_visible(false);
        let label = gtk::Label::new(None);
        label.add_css_class("ufm-meta");
        label.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
        label.set_hexpand(true);
        label.set_xalign(0.0);
        let bar = gtk::ProgressBar::new();
        bar.add_css_class("ufm-usage");
        bar.set_size_request(120, -1);
        let cancel = gtk::Button::with_label("Cancel");
        cancel.set_valign(gtk::Align::Center);
        cancel.set_action_name(Some("win.cancel"));
        root.append(&label);
        root.append(&bar);
        root.append(&cancel);
        parent.append(&root);
        *self.strip.borrow_mut() = Some(Strip { root, label, bar });
        self.paint();
    }

    /// Hear about the strip going empty and non-empty and about the last
    /// operation becoming available to undo, which is how the window keeps
    /// `win.cancel` and `win.undo` sensitive exactly when pulling them would do
    /// something.
    pub fn on_change(&self, on_change: impl FnMut(Status) + 'static) {
        *self.on_change.borrow_mut() = Some(Box::new(on_change));
    }

    /// Register a job about to run and hand out its id and its brake.
    ///
    /// The caller must eventually report [`Event::JobFinished`] for the id — the
    /// table has no other way to learn that a job is over, and a strip left on
    /// screen after its job died is worse than no strip at all.
    #[must_use]
    pub fn begin(
        &self,
        kind: Kind,
        detail: &str,
        total: Option<u64>,
        unit: Unit,
    ) -> (JobId, Brake) {
        let id = JobId(self.next.get());
        self.next.set(self.next.get() + 1);
        self.runs.borrow_mut().push(Job {
            id,
            gerund: kind.gerund(),
            detail: detail.to_string(),
            done: 0,
            total,
            unit,
            brakes: Vec::new(),
        });
        self.paint();
        (id, Brake::new())
    }

    /// Register the brake so the button can reach the job. A job with several
    /// brakes is stopped by pulling any of them, so the strip pulls them all.
    pub fn add_brake(&self, id: JobId, brake: &Brake) {
        if let Some(mut job) = self.find_mut(id) {
            job.brakes.push(brake.clone());
        }
    }

    /// Fold one report in. A report for a job that is no longer running is
    /// dropped rather than resurrected.
    pub fn apply(&self, report: &Progress) {
        {
            let Some(mut job) = self.find_mut(report.id) else {
                return;
            };
            job.detail.clone_from(&report.detail);
            job.done = report.done;
            job.total = report.total;
            job.unit = report.unit;
        }
        self.paint();
    }

    /// Strike a job off. Called for every outcome, including the ones that
    /// failed and the ones the user stopped.
    pub fn finish(&self, id: JobId) {
        self.runs.borrow_mut().retain(|job| job.id != id);
        self.paint();
    }

    /// Pull every brake of every running job. The action behind the button, and
    /// honest about its name: one button stops everything this window started,
    /// because a strip that shows one job while another runs behind it would
    /// have to say which one the button means.
    pub fn pull_all(&self) {
        for job in self.runs.borrow().iter() {
            for brake in &job.brakes {
                brake.pull();
            }
        }
    }

    /// Whether anything is running — the answer the cancel action mirrors.
    #[must_use]
    pub fn is_running(&self) -> bool {
        !self.runs.borrow().is_empty()
    }

    /// Take the record of the last operation, making the table offer it back. The
    /// bytes a replacement were kept in are swept from the previous record, which
    /// is what bounds what a session holds on disk.
    pub fn record(&self, record: Record) {
        if let Some(old) = self.record.borrow_mut().replace(record) {
            old.sweep();
        }
        self.paint();
    }

    /// Take the record out, which is what the Undo action does before running it:
    /// the operation is spent either way, so a second press must not repeat it and
    /// an undo that failed cannot be retried from a record that no longer describes
    /// the folder.
    pub fn take(&self) -> Option<Record> {
        let record = self.record.borrow_mut().take();
        if record.is_some() {
            self.paint();
        }
        record
    }

    /// Whether there is an operation to put back — the answer the undo action
    /// mirrors.
    #[must_use]
    pub fn is_undoable(&self) -> bool {
        self.record.borrow().is_some()
    }

    /// What the strip's label says right now. Empty when there is no strip or no
    /// job. For tests, and for nothing that has to be fast.
    #[must_use]
    pub fn label_text(&self) -> String {
        self.strip
            .borrow()
            .as_ref()
            .map_or_else(String::new, |strip| strip.label.label().to_string())
    }

    /// The bar's text and fraction, for a test that wants to check what a report
    /// did without reaching into the table. The text is only reported while the bar
    /// is showing one, which is the same rule the widget is under.
    #[must_use]
    pub fn reading(&self) -> (Option<String>, f64) {
        let strip = self.strip.borrow();
        match strip.as_ref() {
            Some(strip) => {
                let text = if strip.bar.shows_text() {
                    strip.bar.text().map(|text| text.to_string())
                } else {
                    None
                };
                (text, strip.bar.fraction())
            }
            None => (None, 0.0),
        }
    }

    /// Whether the strip is on screen.
    #[must_use]
    pub fn shown(&self) -> bool {
        self.strip
            .borrow()
            .as_ref()
            .is_some_and(|strip| strip.root.get_visible())
    }

    fn find_mut(&self, id: JobId) -> Option<RefMut<'_, Job>> {
        let Ok(runs) = self.runs.try_borrow_mut() else {
            return None;
        };
        let index = runs.iter().position(|job| job.id == id)?;
        Some(RefMut::map(runs, |jobs| &mut jobs[index]))
    }

    /// Repaint the strip from the table and announce any change in what the two
    /// actions can do. The oldest job wins the label: it is the one the user asked
    /// for first, and the one most likely to be why the window feels slow.
    fn paint(&self) {
        let runs = self.runs.borrow();
        if let (Some(strip), Some(job)) = (self.strip.borrow().as_ref(), runs.first()) {
            strip.label.set_label(&job.label());
            strip.bar.set_fraction(job.fraction());
            let tally = job.tally();
            strip.bar.set_text(tally.as_deref());
            strip.bar.set_show_text(tally.is_some());
        }
        let status = Status {
            running: !runs.is_empty(),
            undoable: self.record.borrow().is_some(),
        };
        if let Some(strip) = self.strip.borrow().as_ref() {
            strip.root.set_visible(status.running);
        }
        drop(runs);
        if let Some(on_change) = self.on_change.borrow_mut().as_mut() {
            on_change(status);
        }
    }
}

impl Default for Jobs {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for Jobs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Jobs")
            .field("running", &self.runs.borrow().len())
            .field("undoable", &self.record.borrow().is_some())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::super::job::Queue;
    use super::*;
    use crate::ui::tk;
    use std::cell::RefCell;
    use std::rc::Rc;
    use ufm_core::Location;

    /// A table with its strip built, which is what a window has and what most of
    /// these assertions are about.
    fn installed() -> (Jobs, gtk::Box) {
        let jobs = Jobs::new();
        let parent = gtk::Box::new(gtk::Orientation::Vertical, 0);
        jobs.install(&parent);
        (jobs, parent)
    }

    #[test]
    fn the_record_needs_no_display_and_says_nothing_until_installed() {
        let jobs = Jobs::new();
        let (id, _) = jobs.begin(Kind::Copy, "report.pdf", Some(9), Unit::Rows);
        assert!(jobs.is_running(), "the job is on the books");
        assert!(!jobs.shown(), "and there is no strip to show");
        jobs.finish(id);
        assert!(!jobs.is_running());
    }

    #[test]
    fn a_running_job_shows_the_strip_and_names_itself() {
        tk::ui(|| {
            let (jobs, _parent) = installed();
            let (id, brake) = jobs.begin(Kind::Copy, "report.pdf", Some(9), Unit::Rows);
            assert!(jobs.shown(), "the strip says so");
            assert_eq!(jobs.label_text(), "Copying report.pdf");
            assert!(!brake.pulled());
            jobs.finish(id);
            assert!(!jobs.shown(), "and it goes away when the job does");
        });
    }

    #[test]
    fn a_report_moves_the_bar_and_a_byte_total_is_spelled_as_size() {
        tk::ui(|| {
            let (jobs, _parent) = installed();
            let (id, _) = jobs.begin(Kind::Copy, "movie.mp4", Some(1_048_576), Unit::Bytes);
            jobs.apply(&Progress {
                id,
                detail: "movie.mp4".to_string(),
                done: 524_288,
                total: Some(1_048_576),
                unit: Unit::Bytes,
            });
            let (tally, fraction) = jobs.reading();
            assert_eq!(tally.as_deref(), Some("512 KiB of 1.0 MiB"));
            assert!((fraction - 0.5).abs() < 0.01);
        });
    }

    #[test]
    fn a_job_with_no_known_total_shows_no_tally_and_stays_at_zero() {
        tk::ui(|| {
            let (jobs, _parent) = installed();
            let (id, _) = jobs.begin(Kind::Extract, "bundle.zip", None, Unit::Rows);
            jobs.apply(&Progress {
                id,
                detail: "bundle.zip".to_string(),
                done: 7,
                total: None,
                unit: Unit::Rows,
            });
            let (tally, fraction) = jobs.reading();
            assert_eq!(tally, None, "an invented total would lie");
            assert!(fraction < f64::EPSILON, "a bar with no total is empty");
            assert!(jobs.shown(), "and the strip still says the job is running");
        });
    }

    #[test]
    fn pulling_the_strip_stops_everything_and_the_brake_notices() {
        tk::ui(|| {
            let (jobs, _parent) = installed();
            let (id, brake) = jobs.begin(Kind::Trash, "3 items", Some(3), Unit::Rows);
            jobs.add_brake(id, &brake);
            jobs.pull_all();
            assert!(brake.pulled());
            assert!(
                brake.vfs().ensure_active().is_err(),
                "the flag the crate's own loops watch is set too"
            );
            jobs.finish(id);
        });
    }

    #[test]
    fn the_second_job_keeps_the_strip_and_the_first_job_keeps_the_label() {
        tk::ui(|| {
            let (jobs, _parent) = installed();
            let (first, _) = jobs.begin(Kind::Copy, "a.txt", Some(1), Unit::Rows);
            let (second, _) = jobs.begin(Kind::Move, "b.txt", Some(1), Unit::Rows);
            jobs.finish(second);
            assert!(jobs.shown(), "one is still running");
            assert_eq!(
                jobs.label_text(),
                "Copying a.txt",
                "the label names the oldest job, which is the one still owed an answer"
            );
            jobs.finish(first);
            assert!(!jobs.shown());
        });
    }

    #[test]
    fn the_change_callback_hears_about_empty_and_non_empty() {
        tk::ui(|| {
            let jobs = Jobs::new();
            let heard = Rc::new(RefCell::new(String::new()));
            {
                let heard = Rc::clone(&heard);
                jobs.on_change(move |status: Status| {
                    heard
                        .borrow_mut()
                        .push_str(if status.running { "y" } else { "n" });
                });
            }
            let (id, _) = jobs.begin(Kind::Delete, "gone.txt", Some(1), Unit::Rows);
            jobs.finish(id);
            assert_eq!(heard.borrow().as_str(), "yn");
        });
    }

    #[test]
    fn the_same_callback_hears_about_a_record_arriving_and_being_spent() {
        tk::ui(|| {
            let jobs = Jobs::new();
            let heard = Rc::new(RefCell::new(String::new()));
            {
                let heard = Rc::clone(&heard);
                jobs.on_change(move |status: Status| {
                    heard
                        .borrow_mut()
                        .push_str(if status.undoable { "u" } else { "-" });
                });
            }
            jobs.record(record_at("/tmp/a"));
            assert!(jobs.take().is_some(), "the record is there to be spent");
            assert!(jobs.take().is_none(), "and spending it is not repeatable");
            assert_eq!(heard.borrow().as_str(), "u-");
        });
    }

    #[test]
    fn a_record_makes_the_undo_action_sensitive_and_a_taken_one_does_not() {
        let jobs = Jobs::new();
        assert!(!jobs.is_undoable(), "nothing to put back in a fresh window");
        jobs.record(record_at("/tmp/a"));
        assert!(jobs.is_undoable());
        let _taken = jobs.take();
        assert!(!jobs.is_undoable());
    }

    #[test]
    fn a_newer_record_sweeps_what_the_older_one_was_keeping() {
        let dir = std::env::temp_dir().join("ufm-jobs-sweep");
        std::fs::create_dir_all(&dir).expect("a stash to sweep");
        std::fs::write(dir.join("old"), b"bytes").expect("a kept file");
        let jobs = Jobs::new();
        let mut keeping = Record::created(Location::file("/tmp/new"));
        keeping.stash = Some(dir.clone());
        jobs.record(keeping);
        jobs.record(record_at("/tmp/b"));
        assert!(
            !dir.exists(),
            "the displaced bytes of the operation the window forgot are gone too"
        );
    }

    /// A record with no bytes behind it, for assertions about the slot rather
    /// than about what is in it.
    fn record_at(path: &str) -> Record {
        Record::moved(&Location::file(path), &Location::file("/tmp/elsewhere"))
    }

    #[test]
    fn a_report_for_a_finished_job_is_dropped() {
        tk::ui(|| {
            let (jobs, _parent) = installed();
            let (id, _) = jobs.begin(Kind::Copy, "late.txt", Some(1), Unit::Rows);
            jobs.finish(id);
            jobs.apply(&Progress {
                id,
                detail: "late.txt".to_string(),
                done: 1,
                total: Some(1),
                unit: Unit::Rows,
            });
            assert!(!jobs.shown(), "a stale report does not reopen the strip");
        });
    }

    #[test]
    fn the_reporter_speaks_once_and_then_waits() {
        tk::ui(|| {
            let queue = Queue::without_source().expect("a pipe can be made");
            let jobs = Jobs::new();
            let (id, _) = jobs.begin(Kind::Copy, "slow.bin", Some(2), Unit::Bytes);
            let mut reporter = Reporter::new(&queue.handle(), id);
            reporter.update("slow.bin", 1, Some(2), Unit::Bytes);
            reporter.update("slow.bin", 2, Some(2), Unit::Bytes);
            let reports: Vec<Progress> = queue
                .take()
                .into_iter()
                .filter_map(|event| match event {
                    Event::Progress { report } => Some(report),
                    _ => None,
                })
                .collect();
            assert_eq!(reports.len(), 1, "the second report was inside the pause");
            assert_eq!(reports[0].done, 1);
            assert_eq!(reports[0].id, id);
            assert_eq!(
                reports[0].unit,
                Unit::Bytes,
                "the worker said what it counts"
            );
        });
    }
}
