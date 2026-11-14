//! The window: header bar, sidebar, one view, and the navigation between them.
//!
//! Everything that can take longer than a frame runs on a worker thread and
//! reports back through [`crate::ui::job`]. The window itself is bookkeeping: a
//! history of locations, the listing currently on screen, and the cancellation
//! flag of the read in flight.
//!
//! Two ownership rules run through this file.
//!
//! The controllers are kept alive by a thread-local list rather than by the GTK
//! window they drive. Storing one on the other would make the pair own each
//! other and survive being closed, with the listing still attached.
//!
//! Signal handlers hold weak references back to the components they watch. A
//! handler is owned by the widget it watches, so a strong reference back would be
//! a cycle: `Panel` would own the closure that owns the `Panel`, and closing the
//! window would free nothing.

use std::cell::Cell;
use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use gio::prelude::*;
use glib::Propagation;
use glib::variant::ToVariant;
use gtk::prelude::*;

use ufm_core::Location;
use ufm_core::config::{Preferences, ViewMode};
use ufm_core::entry::Entry;
use ufm_core::paths;
use ufm_core::sort::SortKey;
use ufm_vfs::{Cancel, FsHints, Readers};

use super::columns::Columns;
use super::conflict;
use super::connect;
use super::extract;
use super::job::{Event, Handle, Queue};
use super::ops;
use super::panel::{Panel, Place, Usage};
use super::popup::{self, Popup};
use super::props;
use super::row::Row;
use super::search::{Found, Outcome, Search};
use super::thumb::Pipeline;
use super::view::View;
use super::watch::Live;
use crate::runner::{Layout, Report, Runner};

// The windows this process has open. A window leaves the list when it is
// closed, which is what frees its widgets and the listing they hold.
thread_local! {
    static OPEN: RefCell<Vec<Window>> = const { RefCell::new(Vec::new()) };
}

/// How often to look for a mount that has gone idle. The threshold itself belongs
/// to [`crate::runner`], so a window and `--sweep-mounts` cannot disagree about
/// how long a share is allowed to sit unused.
const SWEEP_INTERVAL: Duration = Duration::from_secs(60);

/// Open a window showing `urls`, or the home folder when given none.
///
/// Preferences are read here rather than passed in: a second window opened from
/// the first should start in the mode the first one just saved.
pub fn open(app: &gtk::Application, urls: &[String]) {
    let prefs = Preferences::load(&Preferences::path()).unwrap_or_default();
    let readers = Arc::new(Readers::with_archive(Box::new(
        ufm_archive::ArchiveReader::new(),
    )));
    let start = initial(urls, &prefs);
    let mode = prefs.view_mode_for(&start);
    let key = prefs.sort_for(&start);

    let window = gtk::ApplicationWindow::new(app);
    window.set_default_size(1080, 680);
    window.set_title(Some(&start.display()));

    // One menu per window, built before anything can ask for it. Parenting it to
    // the window is what lets its rows carry `win.` action names, so a menu row is
    // greyed out in exactly the places the header menu and the keyboard are greyed
    // out rather than having a second set of rules of its own.
    let popup = Popup::install(&window);

    let panel = Rc::new(Panel::new());
    // One preview pipeline per window: the worker, the queue and the token table
    // are per-window state, because a window is the unit that has a viewport.
    let thumbs = Pipeline::new();
    thumbs.set_enabled(prefs.thumbnails);
    let view = Rc::new(View::new(key, thumbs.clone()));
    view.set_mode(mode);
    let columns = Rc::new(Columns::new(
        Arc::clone(&readers),
        key,
        prefs.show_hidden,
        thumbs.clone(),
    ));
    let (banner, banner_label) = banner_bar();
    let path = path_label(&start);
    // The bar starts out knowing the same folder the window will open on and
    // with the same rule about dotfiles, so that its first query cannot return a
    // dotfile the view would have hidden.
    let find = Search::new(&prefs.search_scope, prefs.search_content, prefs.show_hidden);

    let shared = Rc::new(Shared {
        prefs: RefCell::new(prefs),
        state: RefCell::new(State::new(&start)),
        readers,
        missed: Cell::new(false),
        reading: Cell::new(false),
    });

    // The presentations that are not the grid/list stack. Columns is a sibling
    // of the view rather than a child of it, because it keeps one model per
    // folder instead of one model for the window.
    let modes = gtk::Stack::new();
    modes.add_named(view.widget(), Some("view"));
    modes.add_named(columns.widget(), Some("columns"));
    modes.set_hexpand(true);
    modes.set_vexpand(true);
    if mode == ViewMode::Columns {
        modes.set_visible_child_name("columns");
    }

    let ctx = Ctx {
        window: window.downgrade(),
        panel: Rc::downgrade(&panel),
        view: Rc::downgrade(&view),
        columns: Rc::clone(&columns),
        modes,
        path,
        banner,
        banner_label,
        search: Rc::new(RefCell::new(None)),
        find,
        shared: Rc::clone(&shared),
        handle: Rc::new(RefCell::new(None)),
        popup: Rc::clone(&popup),
        thumbs: thumbs.clone(),
        // Filled in right after the queue, because the answer to a change is a
        // re-read and a re-read needs somewhere to report to.
        live: Rc::new(RefCell::new(None)),
    };

    let queue = match Queue::new({
        let ctx = ctx.clone();
        move |event| apply(&ctx, event)
    }) {
        Ok(queue) => queue,
        Err(error) => {
            // Without the wake-up pipe no listing can ever be delivered, so say
            // so in the terminal rather than showing an empty window forever.
            eprintln!("uflinux-files: cannot set up the listing pipe: {error}");
            return;
        }
    };
    *ctx.handle.borrow_mut() = Some(queue.handle());
    ctx.columns.set_handle(queue.handle());
    // Previews travel on the same pipe as listings. A second one would be a
    // second fd source waking the main loop to do the same work.
    thumbs.set_handle(queue.handle());
    let live = watch_for(&ctx);
    wire_find(&ctx);

    let content = build_content(&ctx, &panel, &view);
    window.set_child(Some(&content));
    window.set_titlebar(Some(&header(&ctx, &menu())));

    panel.set_places(places(&shared));
    panel.set_usage(usages(&shared));
    wire_handlers(&panel, &view, &columns, &ctx);

    wire_actions(app, &window, &ctx);
    wire_close(&window, &shared);

    app.add_window(&window);
    window.present();
    navigate(&ctx, start, false);
    sweep_on_idle(Rc::downgrade(&shared));
    OPEN.with(|open| {
        open.borrow_mut().push(Window {
            top: window,
            _panel: panel,
            _view: view,
            _columns: columns,
            _shared: shared,
            _thumbs: thumbs,
            _queue: queue,
            _live: live,
            _popup: popup,
        });
    });
}

/// Save preferences and drop the window from the list that keeps its widgets
/// alive. The handler holds a weak reference and a cloned `Rc` rather than the
/// window itself: it is owned by the window, and a strong reference back would be
/// the cycle this file's second ownership rule is about.
fn wire_close(window: &gtk::ApplicationWindow, shared: &Rc<Shared>) {
    let shared = Rc::clone(shared);
    let weak = window.downgrade();
    window.connect_close_request(move |_window| {
        // Saving before the widgets go away keeps a crash after closing from
        // losing the mode the user last chose.
        save(&shared);
        if let Some(window) = weak.upgrade() {
            OPEN.with(|open| {
                let mut open = open.borrow_mut();
                if let Some(at) = open.iter().position(|w| w.top == window) {
                    open.remove(at);
                }
            });
        }
        Propagation::Proceed
    });
}

/// Say what the window should do about an answer from the index.
///
/// Registered here rather than in `wire_handlers` because the bar reports to the
/// window rather than to a widget: the answer replaces the listing, which is the
/// window's own state, and the columns have to be told to stand aside.
fn wire_find(ctx: &Ctx) {
    ctx.find.on_reply({
        let ctx = ctx.clone();
        move |outcome| match outcome {
            Outcome::Cleared => restored(&ctx),
            Outcome::Found(page) => found(&ctx, page),
            Outcome::Failed(message) => show_banner(&ctx, &message),
        }
    });
}

/// The window's body: the error strip, the search bar, the sidebar beside the
/// presentations, and the status line.
fn build_content(ctx: &Ctx, panel: &Panel, view: &View) -> gtk::Box {
    let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
    content.append(&ctx.banner);
    // Under the header rather than in it: the bar carries a scope selector and a
    // status line, which is too wide for a header bar and too tall for a
    // toolbar. It is hidden until asked for, so it costs nothing otherwise.
    content.append(ctx.find.widget());
    let split = gtk::Paned::new(gtk::Orientation::Horizontal);
    split.set_start_child(Some(panel.widget()));
    split.set_end_child(Some(&ctx.modes));
    split.set_shrink_start_child(false);
    split.set_position(220);
    content.append(&split);
    content.append(&status_bar(view));
    content
}

/// Everything a handler touches, by value or weakly. Handlers cannot borrow the
/// components, because they outlive the call that installed them.
#[derive(Clone)]
struct Ctx {
    window: glib::WeakRef<gtk::ApplicationWindow>,
    panel: Weak<Panel>,
    view: Weak<View>,
    /// Strong rather than weak: the columns widget owns the listings its panes
    /// cached, and the handlers that write into it hold this same `Rc`. The
    /// cycle is broken by the window, which is what drops the last one.
    columns: Rc<Columns>,
    /// Grid/list against columns. The view keeps its own stack inside this one.
    modes: gtk::Stack,
    path: gtk::Label,
    banner: gtk::Box,
    banner_label: gtk::Label,
    search: Rc<RefCell<Option<gtk::SearchEntry>>>,
    /// Previews for this window's rows. The window answers for it because a
    /// reply arrives on the listing queue, and the queue reports here.
    thumbs: Pipeline,
    /// The bar that asks the index, as opposed to the filter above, which only
    /// narrows what this window already read.
    find: Search,
    shared: Rc<Shared>,
    /// Filled in once the queue exists, which is after the handlers are built.
    handle: Rc<RefCell<Option<Handle>>>,
    /// The right-click menu. Strong rather than weak: the handlers that fill it are
    /// owned by widgets inside the window, and the menu is parented to that window,
    /// so nothing else in the tree would keep it alive.
    popup: Rc<Popup>,
    /// The folders this window has on screen, watched. `None` when the kernel
    /// refused an inotify instance, which leaves the window refreshing on
    /// navigation alone exactly as it did before there were watches.
    ///
    /// `Rc` because it is filled in after the handlers are built, and shared rather
    /// than copied like [`Ctx::handle`]. Weak inside, because the source that wakes
    /// this window up is owned by the main loop: a strong reference here would be
    /// held by a callback the watcher itself keeps alive.
    live: Rc<RefCell<Option<Weak<Live>>>>,
}

/// State shared by every handler in one window.
struct Shared {
    prefs: RefCell<Preferences>,
    state: RefCell<State>,
    readers: Arc<Readers>,
    /// Whether the kernel reported a change to the folder on screen while a
    /// listing for it was already on its way from a worker.
    ///
    /// The reason this exists is the ordering. A listing is read on a worker, so
    /// the window cannot simply re-read on every event: a change arriving while a
    /// read is in flight would cancel it and start another, and a program writing
    /// ten files would cost ten walks of the folder instead of one. Such a change
    /// is noted here instead, and answered by the event that closes the read —
    /// which is also where the flag is cleared, so a burst of writes costs one
    /// extra walk rather than one per write.
    missed: Cell<bool>,
    /// Whether a listing for the folder on screen is on its way from a worker.
    ///
    /// While one is, a change reported by the watcher is only recorded rather than
    /// acted on, for the reason above. Cleared by the event that answers the read.
    reading: Cell<bool>,
}

impl Shared {
    /// Filesystem capacity for one path, read through the same reader the
    /// listings come from so the two cannot disagree about what exists.
    fn hints(&self, path: &std::path::Path) -> FsHints {
        self.readers.local().hints(path)
    }
}

/// The mutable part of one window's view of the filesystem.
struct State {
    current: Option<Location>,
    history: History,
    /// The read in flight, if any. Replaced and cancelled on every navigation.
    cancel: Option<Cancel>,
    /// The listing as read, before the search filter. Kept so that deleting a
    /// character from the query restores rows instead of asking the disk again.
    shown: Vec<Entry>,
    /// Index results standing in for the listing, or `None` while the folder
    /// itself is on screen. Kept apart from `shown` so that putting the search
    /// bar away can bring the folder back without reading it again.
    results: Option<Vec<Entry>>,
    query: String,
    runner: Runner,
    /// What this window has picked up for a paste. Per window rather than
    /// shared: a cut in one window pasting into another would need rows that the
    /// other window has never read.
    clipboard: ops::Clipboard,
}

impl State {
    /// What the in-folder filter narrows: the results when there are results,
    /// and the folder otherwise.
    fn base(&self) -> &[Entry] {
        self.results.as_deref().unwrap_or(&self.shown)
    }
}

impl State {
    fn new(start: &Location) -> Self {
        let mut runner = Runner::open(Layout::system());
        runner.refresh();
        let mut history = History::default();
        history.push(start.clone());
        Self {
            current: Some(start.clone()),
            history,
            cancel: None,
            shown: Vec::new(),
            results: None,
            query: String::new(),
            runner,
            clipboard: ops::Clipboard::default(),
        }
    }

    /// Whether `location` is still the folder on screen. Events from a read the
    /// user has navigated away from are dropped rather than painted over the
    /// folder they can no longer describe.
    fn shows(&self, location: &Location) -> bool {
        self.current.as_ref() == Some(location)
    }
}

/// Where the user has been. A list with a cursor, not a stack: going back must
/// not forget the forward half of the trip.
#[derive(Debug, Default)]
struct History {
    items: Vec<Location>,
    index: usize,
}

impl History {
    /// Record a new location, dropping everything ahead of the cursor. Going
    /// somewhere new from the middle of the history invalidates the forward
    /// branch, which is what every browser does and what users expect.
    ///
    /// The location already visited is recognised by comparing it with the entry
    /// under the cursor rather than with the tip of the list. Comparing with the
    /// tip looks equivalent and is not: after up and back, the folder gone up
    /// from is still the tip while the cursor has moved off it, and a push that
    /// then returns early leaves the cursor where it was — so the window shows
    /// the parent folder while the history believes it shows the child, and
    /// `Back` has nowhere to go from the first entry.
    fn push(&mut self, location: Location) {
        if self
            .items
            .get(self.index)
            .is_some_and(|here| *here == location)
        {
            return;
        }
        self.items.truncate(self.index + 1);
        self.items.push(location);
        self.index = self.items.len() - 1;
    }

    fn go_back(&mut self) -> Option<Location> {
        let next = self.index.checked_sub(1)?;
        self.index = next;
        self.items.get(next).cloned()
    }

    fn go_forward(&mut self) -> Option<Location> {
        let next = self.index + 1;
        let location = self.items.get(next).cloned()?;
        self.index = next;
        Some(location)
    }

    fn can_back(&self) -> bool {
        self.index > 0
    }

    fn can_forward(&self) -> bool {
        self.index + 1 < self.items.len()
    }
}

/// The window, held by [`OPEN`] so that closing it is what frees it.
struct Window {
    /// Called `top` rather than `window`: a field named after its own struct
    /// reads as a typo, and clippy says so too.
    top: gtk::ApplicationWindow,
    _panel: Rc<Panel>,
    _view: Rc<View>,
    _columns: Rc<Columns>,
    _shared: Rc<Shared>,
    /// The preview worker and its token table. Dropping this is what tells the
    /// worker to stop, so it has to outlive the widgets it paints into.
    _thumbs: Pipeline,
    /// Removing the fd source is what stops the wake-ups; the field owns it.
    _queue: Queue,
    /// Likewise the watch: dropping this detaches the source and releases the
    /// kernel watches. `None` when inotify refused an instance.
    _live: Option<Rc<Live>>,
    /// The right-click menu. Parented to `top`, so it goes with the window; held
    /// because a popover with no other owner is freed as soon as the call that
    /// built it returns.
    _popup: Rc<Popup>,
}

/// Where the first window should start.
fn initial(urls: &[String], prefs: &Preferences) -> Location {
    urls.iter()
        .find_map(|url| Location::parse(url).ok())
        .or_else(|| prefs.startup_location.clone())
        .unwrap_or_else(|| Location::file(paths::home()))
}

/// Show `location`, reading it on a worker thread.
///
/// `record` says whether this is somewhere the user went to, as opposed to a
/// refresh of where they already are; a refresh must not push the history.
fn navigate(ctx: &Ctx, location: Location, record: bool) {
    let Some(view) = ctx.view.upgrade() else {
        return;
    };
    let cancel = Cancel::new();

    {
        let mut state = ctx.shared.state.borrow_mut();
        if record {
            state.history.push(location.clone());
        }
        // The old read keeps running until it notices. Its events are dropped by
        // the check in `apply`, which is cheaper than joining the thread.
        if let Some(old) = state.cancel.replace(cancel.clone()) {
            old.cancel();
        }
        state.current = Some(location.clone());
        state.shown = Vec::new();
        state.results = None;
        state.query.clear();
        // A change noted against the folder being left says nothing about the one
        // arriving, whose listing is being read right now.
        ctx.shared.missed.set(false);
        // A listing read through a mount counts as using it, so auto-unmount
        // does not cut a share the user is still looking at.
        let used = state
            .runner
            .table()
            .iter()
            .find(|record| location.is_within(&Location::file(record.mountpoint.clone())))
            .map(|record| record.id.clone());
        if let Some(id) = used {
            state.runner.touch(&id);
        }
    }

    if let Some(window) = ctx.window.upgrade() {
        window.set_title(Some(&location.display()));
    }
    ctx.path.set_label(&location.display());
    ctx.banner.set_visible(false);
    // A menu left open over the folder being left would offer rows for rows that
    // are about to be replaced, and points at a widget that may not survive the
    // repaint.
    ctx.popup.hide();
    // Results describe a query, not this folder, so they leave when the folder
    // does. `reset` rather than `clear`: the listing coming on screen is the
    // thing that replaces them, and reporting `Cleared` here would paint over it.
    ctx.find.reset();
    ctx.find.set_folder(location.as_path());
    view.set_entries(Vec::new());
    // Only while columns are on screen: building a chain reads every folder on
    // the path, and in grid mode that is work for panes nobody can see.
    if columns_on(ctx) {
        ctx.columns.show_chain(&location);
    }
    if let Some(panel) = ctx.panel.upgrade() {
        match place_for(&location) {
            Some(place) => panel.select(&place),
            None => panel.clear_selection(),
        }
    }
    sync_buttons(ctx);

    spawn_read(ctx, location, cancel, &view);
    sync_watches(ctx);
}

/// Re-read the folder on screen because the kernel said it changed.
///
/// Not [`navigate`], which does four things a change made by another program must
/// not: it empties the view first, so the window would flash blank on every save
/// in an editor; it drops the in-folder filter, so a query typed by the user would
/// disappear under them; it resets the index bar, which is showing results the
/// folder has nothing to do with; and it repaints the path label and the sidebar
/// selection, which have not changed. What is left is the read itself, which is
/// [`spawn_read`], and the leaf column, which has to be told to expect the listing
/// the window is about to deliver.
fn reread(ctx: &Ctx, location: Location) {
    let Some(view) = ctx.view.upgrade() else {
        return;
    };
    let cancel = Cancel::new();
    {
        let mut state = ctx.shared.state.borrow_mut();
        // The user moved on between the event and now. The read that is in flight
        // is the one answering where they went, and cancelling it would leave the
        // window showing nothing.
        if state.current.as_ref() != Some(&location) {
            return;
        }
        if let Some(old) = state.cancel.replace(cancel.clone()) {
            old.cancel();
        }
    }
    ctx.columns.expect_leaf();
    spawn_read(ctx, location, cancel, &view);
    // Re-declaring the watches is what picks up a folder that was deleted and
    // created again: deleting a directory makes the kernel retire its watch, and
    // this is the moment the window has just learned the directory is back.
    sync_watches(ctx);
}

/// Read `location` on a worker and report the listing through the queue.
///
/// The caller owns the bookkeeping: `location` has to be the window's current
/// folder and `cancel` has to be the flag in `State`, because [`apply`] drops
/// events whose location the window no longer shows.
fn spawn_read(ctx: &Ctx, location: Location, cancel: Cancel, view: &View) {
    let Some(handle) = ctx.handle.borrow().clone() else {
        return;
    };
    let readers = Arc::clone(&ctx.shared.readers);
    let key = view.sort_key();
    let show_hidden = ctx.shared.prefs.borrow().show_hidden;
    // From here until the event answering this read, a change reported by the
    // watcher is only recorded. See [`Shared::missed`].
    ctx.shared.reading.set(true);
    let token = cancel.clone();
    // Taken before the location moves into the worker, for the one message that
    // is written after the spawn.
    let display = location.display();
    let spawned = thread::Builder::new()
        .stack_size(256 * 1024)
        .spawn(move || {
            let mut sink = Sink {
                handle: handle.clone(),
                cancel: token.clone(),
                location: location.clone(),
                final_entries: Vec::new(),
            };
            match readers.read_streaming(&location, key, show_hidden, None, &token, &mut sink) {
                Ok(_) if token.is_cancelled() => {}
                Ok(_) => {
                    let hints = location.as_path().map(|path| readers.local().hints(path));
                    handle.send(Event::Finished {
                        location,
                        entries: sink.final_entries,
                        hints,
                    });
                }
                Err(error) if error.is_cancelled() => {}
                Err(error) => handle.send(Event::Failed {
                    location,
                    message: error.to_string(),
                }),
            }
        });
    if spawned.is_err() {
        // Out of threads. The window stays on the folder it had, which is
        // confusing but recoverable, so say which folder was not read.
        ctx.shared.reading.set(false);
        view.set_entries(Vec::new());
        show_banner(ctx, &format!("{display}: cannot start a reader."));
    }
}

/// Keep the watched set in step with what this window has on screen.
///
/// A window whose watcher could not be created skips this and behaves the way it
/// did before live updates existed: the listing changes when the user asks.
fn sync_watches(ctx: &Ctx) {
    let Some(live) = ctx.live.borrow().as_ref().and_then(Weak::upgrade) else {
        return;
    };
    let current = ctx.shared.state.borrow().current.clone();
    live.sync(current.as_ref(), &ctx.columns.pane_locations());
}

/// Start watching this window's folders, or explain why nothing will be watched.
///
/// Built after the queue rather than with the rest of the window because the
/// answer to a change is a re-read, and a re-read that cannot report back is a
/// directory walk nobody will ever look at.
fn watch_for(ctx: &Ctx) -> Option<Rc<Live>> {
    let shared = Rc::clone(&ctx.shared);
    let weak = ctx.clone();
    let live = Live::new(move |paths, overflowed| changed(&shared, &weak, paths, overflowed))?;
    *ctx.live.borrow_mut() = Some(Rc::downgrade(&live));
    Some(live)
}

/// The worker's sink: batches go out as they are read, the sorted listing is
/// kept for the one `Finished` event.
struct Sink {
    handle: Handle,
    cancel: Cancel,
    location: Location,
    final_entries: Vec<Entry>,
}

impl ufm_vfs::ListingSink for Sink {
    fn batch(&mut self, entries: &[Entry]) {
        if self.cancel.is_cancelled() {
            return;
        }
        self.handle.send(Event::Batch {
            location: self.location.clone(),
            entries: entries.to_vec(),
        });
    }

    fn sorted(&mut self, entries: &[Entry]) {
        self.final_entries.clear();
        self.final_entries.extend_from_slice(entries);
    }
}

/// Put one delivered event on screen.
fn apply(ctx: &Ctx, event: Event) {
    // First for the columns, which match events against their own panes rather
    // than against the window's current folder: a parent column's listing is not
    // for the folder in the title bar, and the check below would drop it.
    ctx.columns.apply(&event);
    let Some(view) = ctx.view.upgrade() else {
        return;
    };
    match event {
        Event::Batch { location, entries } => {
            if !ctx.shared.state.borrow().shows(&location) {
                return; // the user moved on while this was read
            }
            // The store currently holds result rows, and appending folder rows
            // under them would mix two unrelated listings.
            if ctx.find.is_open() {
                return;
            }
            view.push_batch(&entries);
        }
        Event::Finished {
            location,
            entries,
            hints,
        } => {
            let query = {
                let state = ctx.shared.state.borrow();
                if !state.shows(&location) {
                    return;
                }
                state.query.clone()
            };
            let filtered = filter(&entries, &query);
            ctx.shared.state.borrow_mut().shown = entries;
            ctx.banner.set_visible(false);
            let _ = hints;
            if let Some(panel) = ctx.panel.upgrade() {
                panel.set_usage(usages(&ctx.shared));
            }
            // Index results are on screen rather than this folder's listing, so
            // the rows go into `shown` and wait there: the folder is still the
            // folder, and putting the bar away should show it without a re-read.
            // Everything above still happens, because a folder that read cleanly
            // is not an error even when its rows are not what is visible.
            if ctx.find.is_open() {
                settled(ctx);
                return;
            }
            view.set_entries(filtered);
            settled(ctx);
        }
        Event::Failed { location, message } => {
            if !ctx.shared.state.borrow().shows(&location) {
                return;
            }
            settled(ctx);
            show_banner(ctx, &format!("{}: {message}", location.display()));
        }
        Event::Mounted { report } => mounted(ctx, report),
        Event::Operation { outcome } => operated(ctx, outcome),
        // Not about a folder, so no location to match against the window: the
        // token says which widget asked, and the pipeline knows whether that
        // widget is still showing the file it asked about.
        Event::Thumb { token, png } => ctx.thumbs.deliver(token, png),
        // Also not about the folder on screen: a folder's total belongs to the
        // dialog that asked for it, which decides whether it is still open.
        Event::Sized {
            ref path,
            ref tally,
        } => props::deliver(path, tally),
    }
}

/// The listing that was on its way has arrived, so a change the watcher reported
/// while it was can be answered now.
///
/// One re-read per round, whatever accumulated: a program that wrote forty files
/// has already been answered by the listing that just landed, and the change it
/// triggered is either in that listing or is a change to something the window is
/// not showing.
fn settled(ctx: &Ctx) {
    ctx.shared.reading.set(false);
    if !ctx.shared.missed.replace(false) {
        return;
    }
    let Some(location) = ctx.shared.state.borrow().current.clone() else {
        return;
    };
    reread(ctx, location);
}

/// The kernel says one of this window's folders changed.
///
/// `paths` are the folders that went quiet long enough to re-read, `overflowed`\
/// says the kernel dropped events and nothing can be trusted. Both are already
/// debounced and coalesced by the watcher; what is left to decide is which half of
/// the window to re-read, and when.
fn changed(shared: &Rc<Shared>, ctx: &Ctx, paths: &[PathBuf], overflowed: bool) {
    let current = shared.state.borrow().current.clone();
    let here = current.as_ref().and_then(Location::as_path);
    let reported = |path: &Path| paths.iter().any(|p| p == path);
    // The folder in the title bar. Its rows belong to the view, so the window
    // re-reads it; the leaf column is told to expect that listing rather than
    // walking the same directory a second time.
    let touched_here = overflowed || here.is_some_and(reported);
    // A column beside it: the rows on screen are unaffected, so only that pane is
    // re-read. Deletions arrive as `IN_DELETE_SELF` for the folder itself, which is
    // why a reported path is taken at face value rather than checked for still
    // existing. An overflow says nothing about which folder moved, so every pane is
    // re-read, which is the same pair the reload button runs.
    //
    // The listings go first, in every branch. A re-read that is served from the
    // cache shows the rows the event was reported in order to get rid of, and a
    // column that is not re-read here still has to forget what it remembered, or
    // the next visit to it shows a folder that no longer exists as it was.
    if overflowed {
        shared.readers.clear_listings();
        ctx.columns.reload();
    } else {
        for path in paths {
            shared.readers.invalidate_around(path);
        }
        for pane in ctx.columns.pane_locations().iter().filter(|pane| {
            pane.as_path()
                .is_some_and(|path| Some(path) != here && reported(path))
        }) {
            ctx.columns.reload_one(pane);
        }
    }
    let Some(location) = current.filter(|_| touched_here) else {
        return;
    };
    if shared.reading.get() {
        // A listing for this folder is already on its way, and it may or may not
        // contain the change. Note it and let the event answering that read decide,
        // which is what keeps a burst of writes at one directory walk.
        shared.missed.set(true);
        return;
    }
    reread(ctx, location);
}

/// A file operation finished on a worker.
///
/// The message goes up whatever the outcome: an operation that reported nothing
/// is indistinguishable from one that is still running, and a banner is the only
/// place a refusal can be explained without interrupting the window.
///
/// The folder on screen is re-read rather than patched. A paste adds rows and a
/// rename reorders them, so the sorted listing the window keeps would have to be
/// rebuilt anyway, and one directory walk costs less than the bookkeeping. Parent
/// columns are the other half: [`Columns::reload`] re-reads every pane but the
/// deepest, so this is the same pair the reload button runs, and the two never
/// read one folder twice.
fn operated(ctx: &Ctx, outcome: ops::Outcome) {
    if !outcome.message.is_empty() {
        show_banner(ctx, &outcome.message);
    }
    let current = ctx.shared.state.borrow().current.clone();
    let here = current.as_ref();
    // The folder in the title bar changed, so its rows and its count are both
    // wrong and only a re-read fixes either.
    let changed_here = outcome.changed.iter().any(|folder| here == Some(folder));
    // A change one column to the left does not touch the rows on screen, but it
    // does touch the column that shows them, and a pane holding a row that has
    // been moved is a pane lying about the folder it names.
    let changed_above = outcome
        .changed
        .iter()
        .any(|folder| here.is_some_and(|location| location.is_within(folder)));
    // Whatever the operation touched has to be forgotten before anything is
    // re-read, and for the folders it touched that are not on screen at all —
    // the source of a move, a directory that was deleted outright — there will be
    // no re-read to do it. `invalidate_around` covers the parent columns a paste
    // empties as well as the folder it fills.
    for folder in &outcome.changed {
        if let Some(path) = folder.as_path() {
            ctx.shared.readers.invalidate_around(path);
        }
    }
    if changed_here || changed_above {
        ctx.columns.reload();
    }
    if changed_here && let Some(location) = current {
        navigate(ctx, location, false);
    }
    // Free space moved, and the sidebar shows free space.
    if let Some(panel) = ctx.panel.upgrade() {
        panel.set_usage(usages(&ctx.shared));
    }
}

/// The window and the queue, for the handlers that need both. Either can be gone
/// by the time a handler runs — a window closes and its queue goes with it — and
/// a command issued to a window that is already gone has nowhere to be reported.
fn sink_for(ctx: &Ctx) -> Option<(gtk::ApplicationWindow, Handle)> {
    let handle = ctx.handle.borrow().clone()?;
    let window = ctx.window.upgrade()?;
    Some((window, handle))
}

/// The rows a file operation should act on: the selection in grid and list, the
/// row the column keyboard is on in columns, and nothing else. Acting on rows the
/// user cannot see is worse than acting on none, which is why the columns report
/// one row rather than every pane's selection.
fn targets(ctx: &Ctx) -> Vec<Entry> {
    if columns_on(ctx) {
        return ctx.columns.selected();
    }
    ctx.view
        .upgrade()
        .map_or_else(Vec::new, |view| view.selected())
}

/// The folder operations write into. Index results can be on screen while the
/// folder behind them is not, so this is the folder in the title bar rather than
/// what the rows describe — which is also what a paste into a search should do.
fn target_folder(ctx: &Ctx) -> Option<Location> {
    let state = ctx.shared.state.borrow();
    if !state.current.as_ref().is_some_and(Location::is_real) {
        return None;
    }
    state.current.clone()
}

/// Put one page of index results on screen, in place of the folder.
///
/// The rows go into the same view the folder uses, so opening a row, sorting,
/// and the in-folder filter all work on them without knowing where they came
/// from. The columns are dropped rather than updated: matches from all over the
/// disk are not a chain of folders, and a pane each would be a pane per match.
/// The flat presentations are also what a ranked list wants, so the stack is
/// switched to one for as long as the results are up.
fn found(ctx: &Ctx, page: Found) {
    let Some(view) = ctx.view.upgrade() else {
        return;
    };
    let filtered = filter(&page.entries, &ctx.shared.state.borrow().query);
    ctx.shared.state.borrow_mut().results = Some(page.entries);
    ctx.columns.forget();
    ctx.modes.set_visible_child_name("view");
    view.set_entries(filtered);
    ctx.path.set_label(&page.status);
}

/// The bar was dismissed, so the folder it was standing in front of comes back —
/// from memory rather than from disk, since the listing was kept for exactly this.
fn restored(ctx: &Ctx) {
    let Some(view) = ctx.view.upgrade() else {
        return;
    };
    let (shown, query, current) = {
        let mut state = ctx.shared.state.borrow_mut();
        state.results = None;
        (
            state.shown.clone(),
            state.query.clone(),
            state.current.clone(),
        )
    };
    view.set_entries(filter(&shown, &query));
    let Some(location) = current else {
        return;
    };
    ctx.path.set_label(&location.display());
    let mode = ctx.shared.prefs.borrow().view_mode_for(&location);
    ctx.modes
        .set_visible_child_name(if mode == ViewMode::Columns {
            "columns"
        } else {
            "view"
        });
    if mode == ViewMode::Columns {
        ctx.columns.show_chain(&location);
    }
}

/// A share the connect dialog asked for has been attempted, on a worker.
///
/// The window's own runner knows nothing about it — the worker owned a runner of
/// its own and wrote the record to disk — so the first thing this does is re-read
/// that file. Without it the sidebar would go on listing the shares that were
/// there when the window opened, and a share that had just connected would look
/// like it never had.
fn mounted(ctx: &Ctx, report: Report) {
    {
        let mut state = ctx.shared.state.borrow_mut();
        state.runner.reload();
    }
    // Before the sidebar is repainted: free space and the trash badge come from
    // the cached filesystem capabilities, and a mount point that has just become a
    // share has both of them wrong until they are probed again.
    ctx.shared.readers.clear_fs_cache();
    if let Some(panel) = ctx.panel.upgrade() {
        panel.set_places(places(&ctx.shared));
        panel.set_usage(usages(&ctx.shared));
    }
    if !report.ok {
        show_banner(ctx, &report.message);
        return;
    }
    // Step into the share that just appeared, which is what a user who typed an
    // address expects to see — and which is the success message, so no banner is
    // needed for it. The id is the record's, so the mount point comes from the
    // table that was just re-read rather than from anything the worker kept.
    let target = report.id.as_deref().and_then(|id| {
        let state = ctx.shared.state.borrow();
        state.runner.table().get(id).map(|record| Place::Mount {
            id: record.id.clone(),
            label: record.share.label(),
        })
    });
    // A mount point is a folder that was there before the share was. If this
    // window had looked at it — the empty folder in the sidebar, or the parent it
    // sits in — the listing it remembered says "empty", and navigating in would
    // repaint that rather than the share. The whole neighbourhood goes, because a
    // parent column is what a sidebar user is looking at while the mount appears.
    // `Place::location` returns a location by value, so it is bound rather than
    // borrowed from: the path inside it would otherwise not outlive the call.
    let mount = target.as_ref().map(Place::location);
    if let Some(path) = mount.as_ref().and_then(Location::as_path) {
        ctx.shared.readers.invalidate_around(path);
    }
    match target {
        Some(place) => navigate(ctx, place.location(), true),
        // Connected but the record is not where it should be: the mount happened,
        // and the only way to say so is in words.
        None => show_banner(ctx, &report.message),
    }
}

/// Re-sort what is on screen and remember the choice for this folder.
fn resort(ctx: &Ctx, key: SortKey) {
    let Some(view) = ctx.view.upgrade() else {
        return;
    };
    view.resort(key);
    ctx.columns.resort(key);
    let current = ctx.shared.state.borrow().current.clone();
    if let Some(location) = current {
        ctx.shared.prefs.borrow_mut().set_sort_for(&location, key);
        save(&ctx.shared);
    }
}

/// React to the user opening a row.
fn activate(ctx: &Ctx, row: &Row) {
    let entry = row.entry();
    activate_entry(ctx, &entry);
}

/// Open one entry. Takes an [`Entry`] rather than a row so that the column
/// panes, whose rows are their own widgets, can share the path.
fn activate_entry(ctx: &Ctx, entry: &Entry) {
    if entry.navigable() {
        navigate(ctx, entry.location.clone(), true);
        return;
    }
    let uri = entry.location.as_path().map_or_else(
        || entry.location.to_string().into(),
        |path| gio::File::for_path(path).uri(),
    );
    if let Err(error) = gio::AppInfo::launch_default_for_uri(&uri, None::<&gio::AppLaunchContext>) {
        show_banner(ctx, &format!("Cannot open {}: {error}", entry.name));
    }
}

/// Go to the folder above the one on screen, from the toolbar or from the
/// keyboard in a column pane.
fn go_up(ctx: &Ctx) {
    let target = ctx
        .shared
        .state
        .borrow()
        .current
        .as_ref()
        .and_then(Location::parent);
    if let Some(location) = target {
        navigate(ctx, location, true);
    }
}

/// Whether the columns are what the user is looking at right now.
fn columns_on(ctx: &Ctx) -> bool {
    ctx.modes
        .visible_child_name()
        .is_some_and(|name| name == "columns")
}

/// Narrow the listing to the rows whose name contains `query`, ignoring case.
///
/// Substring rather than prefix, and no word segmentation: the point is to throw
/// rows away while the user is still typing, and a name typed from the middle or
/// with the wrong case should still match.
fn filter(entries: &[Entry], query: &str) -> Vec<Entry> {
    if query.is_empty() {
        return entries.to_vec();
    }
    let needle = query.to_lowercase();
    entries
        .iter()
        .filter(|entry| entry.name.to_lowercase().contains(&needle))
        .cloned()
        .collect()
}

/// The sidebar rows: the standard places plus whatever is mounted.
fn places(shared: &Rc<Shared>) -> Vec<Place> {
    let mut places = Place::standard();
    let state = shared.state.borrow();
    for record in state.runner.table() {
        places.push(Place::Mount {
            id: record.id.clone(),
            label: record.share.label(),
        });
    }
    places
}

/// Usage bars for the places that have a filesystem behind them.
fn usages(shared: &Rc<Shared>) -> Vec<Usage> {
    let state = shared.state.borrow();
    let mut usages = Vec::new();
    let home = shared.hints(&paths::home());
    usages.push(Usage {
        place: Place::Home,
        total: home.total,
        free: home.free,
    });
    for record in state.runner.table() {
        let hints = shared.hints(&record.mountpoint);
        usages.push(Usage {
            place: Place::Mount {
                id: record.id.clone(),
                label: record.share.label(),
            },
            total: hints.total,
            free: hints.free,
        });
    }
    usages
}

/// The sidebar row a location belongs to, so the panel can agree with the view.
fn place_for(location: &Location) -> Option<Place> {
    Place::standard()
        .into_iter()
        .find(|place| place.location() == *location)
}

fn save(shared: &Rc<Shared>) {
    let prefs = shared.prefs.borrow();
    let _ = prefs.save(&Preferences::path());
}

fn show_banner(ctx: &Ctx, message: &str) {
    ctx.banner_label.set_label(message);
    ctx.banner.set_visible(true);
}

/// Enable or disable the navigation buttons to match the history. GTK disables a
/// button whose action name resolves to a disabled action, so the buttons need no
/// reference here.
fn sync_buttons(ctx: &Ctx) {
    let Some(window) = ctx.window.upgrade() else {
        return;
    };
    let state = ctx.shared.state.borrow();
    let back = state.history.can_back();
    let forward = state.history.can_forward();
    let up = state
        .current
        .as_ref()
        .is_some_and(|here| here.parent().is_some_and(|parent| !parent.is_virtual()));
    drop(state);
    enable(&window, "back", back);
    enable(&window, "forward", forward);
    enable(&window, "up", up);
    // Paste belongs here rather than beside the three above: it is the one action
    // whose availability depends on the folder as well as on the clipboard, and
    // this runs on every navigation.
    drop(window);
    sync_paste(ctx);
    sync_ops(ctx);
}

fn enable(window: &gtk::ApplicationWindow, name: &str, on: bool) {
    let action = ActionMapExt::lookup_action(window, name)
        .and_then(|action| action.downcast::<gio::SimpleAction>().ok());
    if let Some(action) = action {
        action.set_enabled(on);
    }
}

/// The header bar: navigation, the path, and the menu.
fn header(ctx: &Ctx, menu: &gio::Menu) -> gtk::HeaderBar {
    let bar = gtk::HeaderBar::new();
    bar.set_show_title_buttons(true);
    for (name, icon, tip) in [
        ("back", "go-previous-symbolic", "Back"),
        ("forward", "go-next-symbolic", "Forward"),
        ("up", "go-up-symbolic", "Up one folder"),
    ] {
        let button = gtk::Button::from_icon_name(icon);
        button.set_action_name(Some(&format!("win.{name}")));
        button.set_tooltip_text(Some(tip));
        bar.pack_start(&button);
    }
    let refresh = gtk::Button::from_icon_name("view-refresh-symbolic");
    refresh.set_action_name(Some("win.refresh"));
    refresh.set_tooltip_text(Some("Reload this folder"));
    bar.pack_start(&refresh);

    // Connect belongs on the bar rather than only in the menu: a share that is
    // hard to reach is a share nobody connects to, and the menu is three clicks
    // deep for something a network folder needs once per session.
    let connect = gtk::Button::from_icon_name("network-server-symbolic");
    connect.set_action_name(Some("win.connect"));
    connect.set_tooltip_text(Some("Connect to a server"));
    bar.pack_start(&connect);

    let search = folder_filter(ctx);
    *ctx.search.borrow_mut() = Some(search.clone());
    let modes = gtk::MenuButton::builder()
        .icon_name("open-menu-symbolic")
        .menu_model(menu)
        .build();
    bar.pack_end(&modes);
    bar.pack_end(&search);
    bar.set_title_widget(Some(&ctx.path));
    bar
}

/// The box that narrows the listing as the user types. Filtering happens in
/// memory against the listing already on screen, so typing costs no disk reads
/// and a folder that took a second to read does not take a second per keystroke.
fn folder_filter(ctx: &Ctx) -> gtk::SearchEntry {
    let search = gtk::SearchEntry::new();
    search.set_placeholder_text(Some("Search this folder"));
    search.set_visible(false);
    search.connect_search_changed({
        let ctx = ctx.clone();
        move |entry| {
            let text = entry.text().to_string();
            let filtered = {
                let state = ctx.shared.state.borrow();
                filter(state.base(), &text)
            };
            ctx.shared.state.borrow_mut().query = text;
            if let Some(view) = ctx.view.upgrade() {
                view.set_entries(filtered.clone());
            }
            let current = ctx.shared.state.borrow().current.clone();
            if let Some(location) = current {
                ctx.columns.set_filter(&location, &filtered);
            }
        }
    });
    search.connect_stop_search({
        let ctx = ctx.clone();
        move |entry| {
            entry.set_visible(false);
            let shown = ctx.shared.state.borrow().base().to_vec();
            ctx.shared.state.borrow_mut().query.clear();
            if let Some(view) = ctx.view.upgrade() {
                view.set_entries(shown.clone());
            }
            let current = ctx.shared.state.borrow().current.clone();
            if let Some(location) = current {
                ctx.columns.set_filter(&location, &shown);
            }
        }
    });
    search
}

/// The primary menu. The three modes are one action with a string parameter so
/// GTK can draw a radio group without three separate actions.
fn menu() -> gio::Menu {
    let menu = gio::Menu::new();
    let view = gio::Menu::new();
    for mode in [ViewMode::Grid, ViewMode::List, ViewMode::Columns] {
        view.append(
            Some(mode.label()),
            Some(&format!("win.mode('{}')", mode_name(mode))),
        );
    }
    let show = gio::Menu::new();
    show.append(Some("Show hidden files"), Some("win.hidden"));
    show.append(Some("Picture previews"), Some("win.thumbnails"));
    // The file operations, in the order a user reaches for them: make something,
    // name it, get rid of it, move it. `Delete Permanently` is spelled out rather
    // than shortened to `Delete`, because a menu that offers both has to say which
    // one can be undone.
    let files = gio::Menu::new();
    files.append(Some("New Folder"), Some("win.new-folder"));
    files.append(Some("Rename"), Some("win.rename"));
    files.append(Some("Move to Trash"), Some("win.trash"));
    files.append(Some("Delete Permanently"), Some("win.delete"));
    files.append(Some("Copy"), Some("win.copy"));
    files.append(Some("Cut"), Some("win.cut"));
    files.append(Some("Paste"), Some("win.paste"));
    // Its own row rather than a submenu: extracting is the reason most people
    // open an archive at all, and the label says what happens — a folder named
    // after the container, appearing beside it, with no dialog in between.
    files.append(Some("Extract to Folder"), Some("win.extract"));
    // Last, because it is the one command here that changes nothing: a user
    // reaches for it to find out what something is, not to make it become
    // something else.
    files.append(Some("Properties"), Some("win.properties"));
    let network = gio::Menu::new();
    network.append(Some("Connect to Server…"), Some("win.connect"));
    let window = gio::Menu::new();
    window.append(Some("Search this folder"), Some("win.search"));
    window.append(Some("Search files and apps"), Some("win.find"));
    window.append(Some("New window"), Some("win.new-window"));
    menu.append_section(None, &view);
    menu.append_section(None, &files);
    menu.append_section(None, &show);
    menu.append_section(None, &network);
    menu.append_section(None, &window);
    menu
}

fn mode_name(mode: ViewMode) -> &'static str {
    match mode {
        ViewMode::Grid => "grid",
        ViewMode::List => "list",
        ViewMode::Columns => "columns",
    }
}

/// The mode named by a menu parameter.
fn mode_named(name: &str) -> Option<ViewMode> {
    match name {
        "grid" => Some(ViewMode::Grid),
        "list" => Some(ViewMode::List),
        "columns" => Some(ViewMode::Columns),
        _ => None,
    }
}

/// Say what each component does when the user acts on it. Every one of these
/// handlers outlives the call that installed it, so each takes a clone of the
/// context and reaches the component it writes to through a weak reference.
fn wire_handlers(panel: &Rc<Panel>, view: &Rc<View>, columns: &Rc<Columns>, ctx: &Ctx) {
    panel.on_open({
        let ctx = ctx.clone();
        move |place| navigate(&ctx, place.location(), true)
    });
    view.on_activate({
        let ctx = ctx.clone();
        move |row| activate(&ctx, row)
    });
    view.install_headers({
        let ctx = ctx.clone();
        move |sort| resort(&ctx, sort)
    });
    // The columns report an `Entry` rather than a row: a pane's rows are its own
    // widgets, and the entry underneath them is the part the window understands.
    columns.on_open({
        let ctx = ctx.clone();
        move |entry| activate_entry(&ctx, entry)
    });
    columns.on_up({
        let ctx = ctx.clone();
        move || go_up(&ctx)
    });
    // Extract is the one command that has to hear about the selection: whether it
    // applies depends on what the row under the keyboard *is*, and moving that row
    // fires no navigation and changes no clipboard.
    view.on_selection({
        let ctx = ctx.clone();
        move || sync_ops(&ctx)
    });
    columns.on_selection({
        let ctx = ctx.clone();
        move || sync_ops(&ctx)
    });
    // One subscriber for both presentations, so that a right-click answers with the
    // same menu wherever it lands. The row was selected before the press was
    // reported — the view does that for itself, the panes in
    // [`super::columns::build`] — so the menu is built from the selection like
    // every other command is.
    let on_context = {
        let ctx = ctx.clone();
        move |widget: &gtk::Widget, target: popup::Target, x: f64, y: f64| {
            show_context_menu(&ctx, widget, target, x, y);
        }
    };
    view.on_context(on_context.clone());
    columns.on_context(on_context);
}

/// The window actions and their accelerators. Split in three because each part
/// is a coherent set — moving, displaying, and the extras — and one function
/// holding all of them is longer than it is deep.
fn wire_actions(app: &gtk::Application, window: &gtk::ApplicationWindow, ctx: &Ctx) {
    let mut actions = nav_actions(ctx);
    actions.extend(view_actions(app, ctx));
    actions.extend(op_actions(ctx));
    actions.extend(context_actions(ctx));
    for action in &actions {
        ActionMapExt::add_action(window, action.as_ref());
    }
    install_accels(app);
    sync_buttons(ctx);
}

/// Where the window can go: back, forward, up, and a re-read of where we are.
fn nav_actions(ctx: &Ctx) -> Vec<Rc<gio::SimpleAction>> {
    let back = gio::SimpleAction::new("back", None);
    back.connect_activate({
        let ctx = ctx.clone();
        move |_action, _parameter| {
            let target = ctx.shared.state.borrow_mut().history.go_back();
            if let Some(location) = target {
                navigate(&ctx, location, false);
            }
        }
    });

    let forward = gio::SimpleAction::new("forward", None);
    forward.connect_activate({
        let ctx = ctx.clone();
        move |_action, _parameter| {
            let target = ctx.shared.state.borrow_mut().history.go_forward();
            if let Some(location) = target {
                navigate(&ctx, location, false);
            }
        }
    });

    let up = gio::SimpleAction::new("up", None);
    up.connect_activate({
        let ctx = ctx.clone();
        move |_action, _parameter| go_up(&ctx)
    });

    let refresh = gio::SimpleAction::new("refresh", None);
    refresh.connect_activate({
        let ctx = ctx.clone();
        move |_action, _parameter| {
            // The cached parent columns first, then the folder itself, which is
            // what `navigate` re-reads. Without the first half a refresh would
            // leave a stale column on screen next to a fresh one.
            //
            // Everything is forgotten rather than just the folders on screen: the
            // point of the button is that the user distrusts what the window
            // remembers, and a re-read that answers from the cache would prove
            // them right in the worst way.
            ctx.shared.readers.clear_listings();
            ctx.columns.reload();
            let current = ctx.shared.state.borrow().current.clone();
            if let Some(location) = current {
                navigate(&ctx, location, false);
            }
        }
    });

    vec![
        Rc::new(back),
        Rc::new(forward),
        Rc::new(up),
        Rc::new(refresh),
    ]
}

/// What the window shows, and the two commands that are neither. The mode action
/// is in [`mode_action`] and the two search commands in [`search_actions`].
fn view_actions(app: &gtk::Application, ctx: &Ctx) -> Vec<Rc<gio::SimpleAction>> {
    let mut actions = vec![mode_action(ctx)];
    actions.extend(search_actions(ctx));

    let hidden = gio::SimpleAction::new_stateful(
        "hidden",
        Some(glib::VariantTy::BOOLEAN),
        &ctx.shared.prefs.borrow().show_hidden.to_variant(),
    );
    hidden.connect_change_state({
        let ctx = ctx.clone();
        move |action, value| {
            let show = value.and_then(glib::Variant::get::<bool>).unwrap_or(false);
            ctx.shared.prefs.borrow_mut().show_hidden = show;
            action.set_state(&show.to_variant());
            // Cached panes were read under the other rule.
            ctx.columns.set_show_hidden(show);
            // And so was the last page of results, which is why the bar gets the
            // switch too rather than being left to disagree with the view.
            ctx.find.set_show_hidden(show);
            let current = ctx.shared.state.borrow().current.clone();
            if let Some(location) = current {
                navigate(&ctx, location, false);
            }
        }
    });

    // The same shape as `hidden`: a switch whose state is the preference, so the
    // menu shows a checkmark that matches what the window is doing. Unlike
    // `hidden` this one needs no re-read — the rows on screen are the same files
    // either way — so it re-binds them instead, which is what asks the factories
    // for a picture or leaves them with the themed icon.
    let thumbnails = gio::SimpleAction::new_stateful(
        "thumbnails",
        Some(glib::VariantTy::BOOLEAN),
        &ctx.shared.prefs.borrow().thumbnails.to_variant(),
    );
    thumbnails.connect_change_state({
        let ctx = ctx.clone();
        move |action, value| {
            let on = value.and_then(glib::Variant::get::<bool>).unwrap_or(false);
            ctx.shared.prefs.borrow_mut().thumbnails = on;
            action.set_state(&on.to_variant());
            ctx.thumbs.set_enabled(on);
            if let Some(view) = ctx.view.upgrade() {
                view.rebind();
            }
            ctx.columns.rebind();
        }
    });

    let new_window = gio::SimpleAction::new("new-window", None);
    new_window.connect_activate({
        let app = app.clone();
        move |_action, _parameter| open(&app, &[])
    });

    // The dialog needs the window to be transient for and the queue to report
    // through, both of which a handler can only reach by upgrading — the point of
    // `Ctx` being cheap to clone rather than of passing widgets around.
    let connect_server = gio::SimpleAction::new("connect", None);
    connect_server.connect_activate({
        let ctx = ctx.clone();
        move |_action, _parameter| {
            let (Some(window), Some(handle)) = (ctx.window.upgrade(), ctx.handle.borrow().clone())
            else {
                return;
            };
            connect::open(&window, &handle);
        }
    });

    actions.extend([
        Rc::new(hidden),
        Rc::new(thumbnails),
        Rc::new(new_window),
        Rc::new(connect_server),
    ]);
    actions
}

/// The three presentations, as one stateful action so GTK can draw a radio group
/// without three separate actions.
fn mode_action(ctx: &Ctx) -> Rc<gio::SimpleAction> {
    let mode = gio::SimpleAction::new_stateful(
        "mode",
        Some(glib::VariantTy::STRING),
        &mode_name(
            ctx.view
                .upgrade()
                .map_or(ViewMode::Grid, |view| view.mode()),
        )
        .to_variant(),
    );
    mode.connect_activate({
        let ctx = ctx.clone();
        move |action, parameter| {
            let Some(name) = parameter.and_then(glib::Variant::get::<String>) else {
                return;
            };
            let Some(mode) = mode_named(&name) else {
                return;
            };
            let columns = mode == ViewMode::Columns;
            let was_columns = columns_on(&ctx);
            if let Some(view) = ctx.view.upgrade() {
                view.set_mode(mode);
            }
            ctx.modes
                .set_visible_child_name(if columns { "columns" } else { "view" });
            // Coming back to columns after a while, or arriving here for the
            // first time: the chain has to be built, and any pane that was
            // dropped while hidden has to come back.
            if columns && !was_columns {
                let current = ctx.shared.state.borrow().current.clone();
                if let Some(location) = current {
                    ctx.columns.show_chain(&location);
                }
            }
            action.set_state(&name.to_variant());
            let current = ctx.shared.state.borrow().current.clone();
            if let Some(location) = current {
                ctx.shared
                    .prefs
                    .borrow_mut()
                    .set_view_mode_for(&location, mode);
                save(&ctx.shared);
            }
        }
    });
    Rc::new(mode)
}

/// The two search commands. The filter is free and instant but cannot see
/// outside the folder; the bar costs a round trip and finds the file that is
/// three folders down. Opening one hides the other, because the two do not
/// compose — index results are already a narrowed set.
fn search_actions(ctx: &Ctx) -> Vec<Rc<gio::SimpleAction>> {
    let filter = gio::SimpleAction::new("search", None);
    filter.connect_activate({
        let ctx = ctx.clone();
        move |_action, _parameter| {
            ctx.find.clear();
            if let Some(entry) = ctx.search.borrow().as_ref() {
                entry.set_visible(true);
                entry.grab_focus();
            }
        }
    });

    let find = gio::SimpleAction::new("find", None);
    find.connect_activate({
        let ctx = ctx.clone();
        move |_action, _parameter| {
            ctx.find.show();
            if let Some(entry) = ctx.search.borrow().as_ref() {
                entry.set_visible(false);
            }
        }
    });

    vec![Rc::new(filter), Rc::new(find)]
}

/// The file operations: create, rename, throw away, and move between folders.
///
/// Every handler reaches the window and the queue by upgrading, for the same
/// reason the connect action does: a handler outlives the call that installed it
/// and cannot hold either by reference. Nothing here reads a directory. The rows
/// to act on come from the selection, the work happens on a worker, and the
/// answer arrives through [`operated`] — which is also why no handler has to be
/// careful about a folder that changes underneath it.
fn op_actions(ctx: &Ctx) -> Vec<Rc<gio::SimpleAction>> {
    let new_folder = gio::SimpleAction::new("new-folder", None);
    new_folder.connect_activate({
        let ctx = ctx.clone();
        move |_action, _parameter| {
            let Some((window, handle)) = sink_for(&ctx) else {
                return;
            };
            let Some(folder) = target_folder(&ctx) else {
                return;
            };
            ops::ask_new_folder(&window, &handle, &folder);
        }
    });

    let rename = gio::SimpleAction::new("rename", None);
    rename.connect_activate({
        let ctx = ctx.clone();
        move |_action, _parameter| {
            let Some((window, handle)) = sink_for(&ctx) else {
                return;
            };
            // Renaming a selection of three has no obvious answer, so the command
            // asks for exactly one row and stays quiet otherwise rather than
            // guessing which of them the user meant.
            let picked = targets(&ctx);
            let [entry] = picked.as_slice() else {
                return;
            };
            ops::ask_rename(&window, &handle, entry);
        }
    });

    let paste = gio::SimpleAction::new("paste", None);
    paste.connect_activate({
        let ctx = ctx.clone();
        move |_action, _parameter| {
            let Some((window, handle)) = sink_for(&ctx) else {
                return;
            };
            let Some(folder) = target_folder(&ctx) else {
                return;
            };
            // The plan is built while the state is borrowed, and the worker takes
            // it by value: nothing about the operation depends on the clipboard
            // afterwards, which is what lets one cut feed two folders.
            let (plan, transfer) = {
                let state = ctx.shared.state.borrow();
                (state.clipboard.plan(&folder), state.clipboard.transfer())
            };
            // Not `ops::paste`: a plan that ran into a name the folder already
            // has has to ask before it writes anything, and the window is the
            // only thing here that can show the question.
            conflict::paste(&window, &handle, plan, transfer);
        }
    });

    let unpack = gio::SimpleAction::new("extract", None);
    unpack.connect_activate({
        let ctx = ctx.clone();
        move |_action, _parameter| {
            let Some(handle) = ctx.handle.borrow().clone() else {
                return;
            };
            let Some(container) = extract_target(&ctx) else {
                return;
            };
            extract::extract(&handle, container);
        }
    });

    let facts = gio::SimpleAction::new("properties", None);
    facts.connect_activate({
        let ctx = ctx.clone();
        move |_action, _parameter| {
            let Some((window, handle)) = sink_for(&ctx) else {
                return;
            };
            // One row, for the same reason renaming asks for one: a dialog about
            // three files has no obvious shape, and a user who selected three
            // usually wanted the folder they are looking at rather than a summary
            // of a third of it.
            let picked = targets(&ctx);
            let [entry] = picked.as_slice() else {
                return;
            };
            props::show(&window, &handle, entry);
        }
    });

    vec![
        Rc::new(new_folder),
        Rc::new(rename),
        discard_action("trash", false, ctx),
        discard_action("delete", true, ctx),
        pickup_action("copy", ops::Transfer::Copy, ctx),
        pickup_action("cut", ops::Transfer::Move, ctx),
        Rc::new(paste),
        Rc::new(unpack),
        Rc::new(facts),
    ]
}

/// Which container the extract command would unpack, mirroring the sensitivity
/// rule in [`sync_extract`] so the two can never disagree about what is on screen.
///
/// One selected row wins when it names a container. Otherwise the folder in the
/// title bar is the candidate, which is what makes the command work from inside
/// an archive: the user is standing in the thing they want unpacked.
fn extract_target(ctx: &Ctx) -> Option<Location> {
    let picked = targets(ctx);
    if let [entry] = picked.as_slice() {
        // A directory called `bundle.tar` has the right name and is still not
        // something to unpack; the reader would fail on it either way.
        if !entry.kind.is_dir() && extract::container_of(&entry.location).is_some() {
            return Some(entry.location.clone());
        }
    }
    let current = ctx.shared.state.borrow().current.clone()?;
    extract::container_of(&current).is_some().then_some(current)
}

/// Enable or disable the two commands whose answer depends on the selection
/// rather than on the folder or the clipboard. Both are synced from navigation
/// and from every change of selection, which is the only moment their answer can
/// change without a navigation.
fn sync_ops(ctx: &Ctx) {
    sync_extract(ctx);
    sync_properties(ctx);
}

/// Enable or disable extract to match the row under the keyboard. The one file
/// command whose answer depends on *what* is selected rather than on how much;
/// properties depends on the count instead — see [`sync_properties`].
fn sync_extract(ctx: &Ctx) {
    let Some(window) = ctx.window.upgrade() else {
        return;
    };
    enable(&window, "extract", extract_target(ctx).is_some());
}

/// Enable or disable properties: exactly one row, whatever it is. Every row has
/// a name and a type, so unlike extract the answer never depends on what the row
/// turns out to be — only on there being one and only one of them.
fn sync_properties(ctx: &Ctx) {
    let Some(window) = ctx.window.upgrade() else {
        return;
    };
    enable(&window, "properties", targets(ctx).len() == 1);
}

/// `trash` and `delete` differ by one flag, so they are built by one function.
/// Two near-identical closures would be two places to fix.
fn discard_action(name: &'static str, permanent: bool, ctx: &Ctx) -> Rc<gio::SimpleAction> {
    let action = gio::SimpleAction::new(name, None);
    action.connect_activate({
        let ctx = ctx.clone();
        move |_action, _parameter| {
            let Some(handle) = ctx.handle.borrow().clone() else {
                return;
            };
            let picked = targets(&ctx);
            if picked.is_empty() {
                return;
            }
            ops::discard(&handle, &picked, permanent);
        }
    });
    Rc::new(action)
}

/// `copy` and `cut`: remember the selection and say whether a paste should move.
///
/// An empty selection leaves the clipboard alone rather than clearing it. Rows
/// can vanish from under a selection while a folder is being re-read, and losing
/// a carefully built clipboard because one Enter was pressed on a folder would
/// look like the file manager forgot on its own.
fn pickup_action(name: &'static str, transfer: ops::Transfer, ctx: &Ctx) -> Rc<gio::SimpleAction> {
    let action = gio::SimpleAction::new(name, None);
    action.connect_activate({
        let ctx = ctx.clone();
        move |_action, _parameter| {
            let picked = targets(&ctx);
            if picked.is_empty() {
                return;
            }
            ctx.shared
                .state
                .borrow_mut()
                .clipboard
                .set(&picked, transfer);
            sync_paste(&ctx);
        }
    });
    Rc::new(action)
}

/// Enable or disable paste to match the clipboard. Disabled rather than hidden:
/// the menu is where a user looks for the command that is missing, and a
/// greyed-out row explains itself where an absent one does not.
fn sync_paste(ctx: &Ctx) {
    let Some(window) = ctx.window.upgrade() else {
        return;
    };
    enable(&window, "paste", can_paste(ctx));
}

/// Whether Paste would have anything to do here: something on the clipboard, and a
/// real folder to put it in. One answer for the action's sensitivity and for
/// whether the context menu offers the row at all.
fn can_paste(ctx: &Ctx) -> bool {
    let state = ctx.shared.state.borrow();
    !state.clipboard.is_empty() && state.current.as_ref().is_some_and(Location::is_real)
}

/// The two commands the context menu needs that nothing else had reason to have.
///
/// `open` is the menu's version of pressing Enter: the other rows are commands
/// that already have an action, and a menu that could not enter a folder would be
/// a menu of edits only. `context-menu` is the keyboard's and the test harness's
/// way of opening the same popover without a pointer to follow.
fn context_actions(ctx: &Ctx) -> Vec<Rc<gio::SimpleAction>> {
    let open = gio::SimpleAction::new("open", None);
    open.connect_activate({
        let ctx = ctx.clone();
        move |_action, _parameter| {
            // Exactly one row, which is also what the menu offers Open for.
            let picked = targets(&ctx);
            let [entry] = picked.as_slice() else {
                return;
            };
            activate_entry(&ctx, entry);
        }
    });

    let menu = gio::SimpleAction::new("context-menu", None);
    menu.connect_activate({
        let ctx = ctx.clone();
        move |_action, _parameter| {
            let Some(window) = ctx.window.upgrade() else {
                return;
            };
            // No pointer, so the menu points at the focused row instead. The rows
            // are the ones for the folder on screen, which is what a keyboard user
            // standing in a folder means by "the menu here".
            ctx.popup.hide();
            ctx.popup.show(&window, &menu_rows(&ctx, true), None);
        }
    });

    vec![Rc::new(open), Rc::new(menu)]
}

/// The rows for a menu opened without a press, from the state the window is in.
fn menu_rows(ctx: &Ctx, in_current: bool) -> Vec<popup::Command> {
    let picked = targets(ctx);
    let scope = popup::Scope {
        in_current,
        extracting: extract_target(ctx).is_some(),
        pastable: can_paste(ctx),
    };
    popup::rows_for(&picked, scope)
}

/// Open the menu over a press. `widget` and the coordinates are the press itself,
/// so the menu appears under the pointer rather than at some default spot.
///
/// A press off the rows asks for the folder's menu even when rows are still
/// selected: the user clicked the folder, and a menu of row commands there would
/// answer a question that was not asked.
fn show_context_menu(ctx: &Ctx, widget: &gtk::Widget, target: popup::Target, x: f64, y: f64) {
    let Some(window) = ctx.window.upgrade() else {
        return;
    };
    let picked = target.position().map_or_else(Vec::new, |_| targets(ctx));
    let scope = popup::Scope {
        in_current: target.in_current(),
        // Asked of the window rather than of the rows: the answer is the same
        // whenever the menu would have a row to unpack, and the action's own
        // sensitivity is what stops a stale answer from doing anything.
        extracting: extract_target(ctx).is_some(),
        pastable: can_paste(ctx),
    };
    ctx.popup.show(
        &window,
        &popup::rows_for(&picked, scope),
        Some((widget, x, y)),
    );
}

/// The keyboard. Set on the application rather than the window, so a shortcut
/// works in every window the session opens.
fn install_accels(app: &gtk::Application) {
    app.set_accels_for_action("win.back", &["<Alt>Left"]);
    app.set_accels_for_action("win.forward", &["<Alt>Right"]);
    app.set_accels_for_action("win.up", &["<Alt>Up"]);
    app.set_accels_for_action("win.refresh", &["F5"]);
    app.set_accels_for_action("win.hidden", &["<Ctrl>H"]);
    app.set_accels_for_action("win.search", &["<Ctrl>F"]);
    // `<Ctrl>F` is the filter, which is the one a user reaching for search in a
    // folder wants. The index is the wider thing, so it gets the wider chord.
    app.set_accels_for_action("win.find", &["<Ctrl><Shift>F"]);
    app.set_accels_for_action("win.new-window", &["<Ctrl>N"]);
    // `<Ctrl>K` rather than `<Ctrl>L`: the location bar took that one, and a
    // keyboard shortcut for connecting is the difference between trying a share
    // and giving up on it.
    app.set_accels_for_action("win.connect", &["<Ctrl>K"]);
    // The file operations. `Delete` goes to the trash and `<Shift>Delete` skips
    // it, which is the one pair worth the modifier: the difference between the
    // two is whether a mistake can be undone, and the rarer of the two should be
    // the one that costs a chord. `<Ctrl>C`/`<Ctrl>X`/`<Ctrl>V` are the usual
    // three; a focused text entry keeps its own copy and paste, because an entry
    // binds those keys on its own controller and handles the event before the
    // window's accelerator does — which is what makes renaming a file without
    // accidentally pasting into the middle of a name possible.
    app.set_accels_for_action("win.new-folder", &["<Ctrl><Shift>N"]);
    app.set_accels_for_action("win.rename", &["F2"]);
    app.set_accels_for_action("win.trash", &["Delete"]);
    app.set_accels_for_action("win.delete", &["<Shift>Delete"]);
    app.set_accels_for_action("win.copy", &["<Ctrl>C"]);
    app.set_accels_for_action("win.cut", &["<Ctrl>X"]);
    app.set_accels_for_action("win.paste", &["<Ctrl>V"]);
    // `<Ctrl>E` is free here: the trash is on `Delete`, where GNOME keeps it in
    // the keyboard rather than on a chord, so the letter most people expect for
    // extract is not spoken for.
    app.set_accels_for_action("win.extract", &["<Ctrl>E"]);
    // `<Alt>Return` is the GNOME chord for properties and is bound nowhere else:
    // plain `Return` activates a row, which is the one thing a file manager can
    // never give up.
    app.set_accels_for_action("win.properties", &["<Alt>Return"]);
    // The Menu key, and the chord for keyboards without one. Both go to the same
    // action the pointer uses, so the keyboard gets the same menu rather than a
    // shorter list chosen because it was cheaper to build.
    app.set_accels_for_action("win.context-menu", &["Menu", "<Shift>F10"]);
    for mode in [ViewMode::Grid, ViewMode::List, ViewMode::Columns] {
        app.set_accels_for_action(
            &format!("win.mode::{}", mode_name(mode)),
            &[mode.accelerator()],
        );
    }
}

/// Detach a mount that has not been read through recently. The reference is
/// weak: a window that is gone has nothing left to time out.
fn sweep_on_idle(shared: Weak<Shared>) {
    glib::source::timeout_add_local(SWEEP_INTERVAL, move || {
        let Some(shared) = shared.upgrade() else {
            return glib::ControlFlow::Break;
        };
        // The window is not the place to report a mount it failed to release: a
        // banner would interrupt reading for something the user cannot act on.
        // What it does have to do is forget the listings that were read through a
        // share that has just gone away: a cached folder on a detached mount is a
        // folder whose rows no longer exist, and the next visit would paint them.
        if let Ok(sweep) = shared.state.borrow_mut().runner.auto_unmount()
            && !sweep.removed().is_empty()
        {
            shared.readers.clear_fs_cache();
            for item in sweep.removed() {
                shared.readers.invalidate_around(&item.path);
            }
        }
        glib::ControlFlow::Continue
    });
}

/// The path shown in place of a title: ellipsised in the middle, so a long path
/// still shows the folder the user is in.
fn path_label(location: &Location) -> gtk::Label {
    let label = gtk::Label::new(Some(&location.display()));
    label.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
    label.set_max_width_chars(48);
    label.set_selectable(true);
    label
}

/// The error strip. A banner rather than a dialog because a failed folder is
/// normal: a disconnected share, a container another program is writing.
fn banner_bar() -> (gtk::Box, gtk::Label) {
    let box_ = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    box_.add_css_class("ufm-banner");
    box_.set_visible(false);
    let image = gtk::Image::from_icon_name("dialog-warning-symbolic");
    image.set_pixel_size(16);
    let label = gtk::Label::new(None);
    label.set_wrap(true);
    label.set_lines(2);
    label.set_xalign(0.0);
    label.set_hexpand(true);
    let close = gtk::Button::from_icon_name("window-close-symbolic");
    close.set_has_frame(false);
    {
        let box_ = box_.clone();
        close.connect_clicked(move |_| box_.set_visible(false));
    }
    box_.append(&image);
    box_.append(&label);
    box_.append(&close);
    (box_, label)
}

/// The status bar: the view's own item count, the only number that can agree
/// with what the user sees selected.
fn status_bar(view: &View) -> gtk::Box {
    let box_ = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    box_.set_margin_start(6);
    box_.set_margin_end(6);
    box_.append(&view.status_label());
    box_
}

#[cfg(test)]
mod tests {
    use super::*;
    use ufm_core::{EntryFlags, EntryKind};

    fn location(name: &str) -> Location {
        Location::file(format!("/home/u/{name}"))
    }

    fn entry(name: &str) -> Entry {
        Entry {
            location: Location::file(format!("/tmp/{name}")),
            name: name.to_string(),
            kind: EntryKind::File,
            flags: EntryFlags::WRITABLE,
            size: 4,
            allocated: Some(4096),
            modified: None,
            accessed: None,
            created: None,
            mode: Some(0o644),
            uid: None,
            gid: None,
            mime: None,
            child_count: None,
            icon_key: "text/plain".to_string(),
        }
    }

    #[test]
    fn a_fresh_history_has_nowhere_to_go() {
        let history = History::default();
        assert!(!history.can_back());
        assert!(!history.can_forward());
    }

    #[test]
    fn going_back_and_forth_walks_the_whole_history() {
        let mut history = History::default();
        history.push(location("a"));
        history.push(location("b"));
        history.push(location("c"));
        assert_eq!(history.go_back(), Some(location("b")));
        assert_eq!(history.go_back(), Some(location("a")));
        assert_eq!(history.go_back(), None);
        assert_eq!(history.go_forward(), Some(location("b")));
        assert!(history.can_forward());
    }

    #[test]
    fn going_somewhere_new_forgets_the_forward_branch() {
        let mut history = History::default();
        history.push(location("a"));
        history.push(location("b"));
        history.go_back();
        history.push(location("c"));
        assert!(!history.can_forward());
        assert!(history.can_back());
    }

    #[test]
    fn revisiting_a_folder_does_not_grow_the_history() {
        let mut history = History::default();
        history.push(location("a"));
        history.push(location("a"));
        assert_eq!(history.items.len(), 1);
        assert!(!history.can_back());
    }

    // Going up and back and up again, which is the shape the up button makes a
    // user walk: the parent is recorded, the child is returned to, and the parent
    // is visited a second time. The second visit must move the cursor back to the
    // entry that is already there rather than being ignored, because a cursor left
    // behind reports the wrong window for the history and strands `Back`.
    #[test]
    fn returning_to_a_folder_that_is_still_recorded_rejoins_it() {
        let mut history = History::default();
        history.push(location("child"));
        history.push(location("parent"));
        history.go_back();
        assert_eq!(history.items.get(history.index), Some(&location("child")));
        history.push(location("parent"));
        assert_eq!(history.items.len(), 2);
        assert_eq!(history.index, 1);
        assert_eq!(history.go_back(), Some(location("child")));
    }

    #[test]
    fn a_fresh_window_already_counts_as_the_first_stop() {
        let state = State::new(&location("a"));
        assert!(!state.history.can_back());
        assert!(state.shows(&location("a")));
        assert!(!state.shows(&location("b")));
    }

    #[test]
    fn a_url_wins_over_the_saved_startup_folder() {
        let prefs = Preferences::default();
        assert_eq!(
            initial(&["/tmp/x".to_string()], &prefs),
            Location::file("/tmp/x")
        );
    }

    #[test]
    fn a_nonsense_url_falls_back_rather_than_failing() {
        let prefs = Preferences::default();
        assert_eq!(
            initial(&["not a location".to_string()], &prefs),
            Location::file(paths::home())
        );
    }

    #[test]
    fn a_saved_startup_folder_is_used_when_there_are_no_arguments() {
        let prefs = Preferences {
            startup_location: Some(Location::file("/srv/data")),
            ..Preferences::default()
        };
        assert_eq!(initial(&[], &prefs), Location::file("/srv/data"));
    }

    #[test]
    fn the_menu_names_every_mode_the_action_accepts() {
        for mode in [ViewMode::Grid, ViewMode::List, ViewMode::Columns] {
            assert_eq!(mode_named(mode_name(mode)), Some(mode));
        }
        assert_eq!(mode_named("tiles"), None);
    }

    #[test]
    fn an_empty_query_keeps_everything_in_order() {
        let entries = vec![entry("b.txt"), entry("a.txt")];
        let kept = filter(&entries, "");
        assert_eq!(kept.len(), 2);
        assert_eq!(kept[0].name, "b.txt");
    }

    #[test]
    fn a_query_matches_anywhere_in_the_name_and_ignores_case() {
        let entries = vec![entry("Report.pdf"), entry("notes.txt"), entry("image.png")];
        let hits = filter(&entries, "report");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].name, "Report.pdf");
    }

    #[test]
    fn a_query_that_matches_nothing_shows_an_empty_folder_not_an_error() {
        assert!(filter(&[entry("a.txt")], "zzz").is_empty());
    }

    #[test]
    fn filtering_does_not_reorder_what_it_keeps() {
        let entries = vec![entry("z1"), entry("a2"), entry("z3")];
        let kept = filter(&entries, "z");
        assert_eq!(
            kept.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(),
            vec!["z1", "z3"]
        );
    }

    #[test]
    fn a_known_folder_has_a_sidebar_row_and_an_unknown_one_does_not() {
        assert!(place_for(&Location::file(paths::home())).is_some());
        assert!(place_for(&Location::file("/definitely/not/a/place")).is_none());
    }
}
