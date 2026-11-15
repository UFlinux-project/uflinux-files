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
use std::time::{Duration, SystemTime};

use gio::prelude::*;
use glib::Propagation;
use glib::variant::ToVariant;
use gtk::prelude::*;

use ufm_core::Location;
use ufm_core::Starred;
use ufm_core::config::{Bookmark, Bookmarks, Preferences, ViewMode};
use ufm_core::entry::{Entry, EntryFlags};
use ufm_core::location::VirtualDir;
use ufm_core::paths;
use ufm_core::sort::SortKey;
use ufm_mount::table::MountRecord;
use ufm_vfs::{Cancel, FsHints, Readers};

use super::columns::Columns;
use super::computer::{self, Card, CardId, ComputerView};
use super::conflict;
use super::connect;
use super::extract;
use super::job::{Event, Handle, Queue};
use super::jobs::Jobs;
use super::openwith;
use super::ops;
use super::panel::{self, Panel, Place, Usage};
use super::popup::{self, Popup};
use super::preview::{self, Preview};
use super::props;
use super::recent;
use super::row::{Row, format_items};
use super::search::{Found, Outcome, Search};
use super::thumb::Pipeline;
use super::undo;
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

/// The window's state, with the two lists a window keeps on disk read once.
///
/// Both are read here rather than per listing: each is small, each changes only
/// through the window that holds it, and every change writes the file back. A
/// window opened later picks up what an earlier one saved, because this runs per
/// window and the file is the only thing between them.
fn shared_state(prefs: Preferences, readers: Arc<Readers>, start: &Location) -> Rc<Shared> {
    Rc::new(Shared {
        prefs: RefCell::new(prefs),
        starred: RefCell::new(Starred::load(&Starred::path()).unwrap_or_default()),
        bookmarks: RefCell::new(Bookmarks::load(&Bookmarks::path()).unwrap_or_default()),
        state: RefCell::new(State::new(start)),
        readers,
        missed: Cell::new(false),
        reading: Cell::new(false),
        spoken: RefCell::new(None),
    })
}

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
    // The spacebar preview, built closed. Its own popover rather than a second
    // job for the menu's popover: a menu and a preview have nothing in common
    // beyond being popovers, and sharing one would mean a preview that closes
    // the menu it did not open.
    let preview = Preview::install(&window);

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
    // The cards for `computer://`, built empty. The window is what fills them in,
    // because the mount table and the capacities live in the window's state.
    let computer = Rc::new(ComputerView::new());

    let shared = shared_state(prefs, readers, &start);

    // The presentations that are not the grid/list stack.
    let modes = mode_stack(&view, &columns, &computer, mode);

    let ctx = Ctx {
        window: window.downgrade(),
        panel: Rc::downgrade(&panel),
        view: Rc::downgrade(&view),
        columns: Rc::clone(&columns),
        computer: Rc::clone(&computer),
        modes,
        path,
        banner,
        banner_label,
        search: Rc::new(RefCell::new(None)),
        find,
        shared: Rc::clone(&shared),
        handle: Rc::new(RefCell::new(None)),
        popup: Rc::clone(&popup),
        preview: Rc::clone(&preview),
        jobs: Rc::new(Jobs::new()),
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
    wire_jobs(&ctx);
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
            _computer: computer,
            _shared: shared,
            _thumbs: thumbs,
            _queue: queue,
            _live: live,
            _popup: popup,
            _preview: preview,
        });
    });
}

/// The presentations, in a stack that shows whichever one the preferences say
/// this folder should use.
///
/// Columns is a sibling of the view rather than a child of it, because it keeps
/// one model per folder instead of one model for the window. The cards are a
/// sibling for the same reason: they are not a listing, and a folder's rows have
/// nothing to do with a set of filesystems.
fn mode_stack(
    view: &View,
    columns: &Columns,
    computer: &ComputerView,
    mode: ViewMode,
) -> gtk::Stack {
    let modes = gtk::Stack::new();
    modes.add_named(view.widget(), Some("view"));
    modes.add_named(columns.widget(), Some("columns"));
    modes.add_named(computer.widget(), Some("computer"));
    modes.set_hexpand(true);
    modes.set_vexpand(true);
    if mode == ViewMode::Columns {
        modes.set_visible_child_name("columns");
    }
    modes
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

/// Tie the job strip to the actions that answer for what it reports.
///
/// The strip's Cancel button carries `win.cancel`, and the action is registered
/// disabled. What is left is the rule connecting the two: the button is sensitive
/// exactly while something runs, which the table reports rather than the window,
/// because a job can finish on a worker at any moment and the window has no other
/// way to notice. `win.undo` is on the same callback for the same reason: an
/// operation is spent the moment it is undone, and that moment is not one the
/// window chose.
fn wire_jobs(ctx: &Ctx) {
    let window = ctx.window.clone();
    ctx.jobs.on_change(move |status| {
        if let Some(window) = window.upgrade() {
            enable(&window, "cancel", status.running);
            enable(&window, "undo", status.undoable);
        }
    });
}

/// The window's body: the error strip, the search bar, the sidebar beside the
/// presentations, and the status line.
fn build_content(ctx: &Ctx, panel: &Panel, view: &View) -> gtk::Box {
    let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
    content.append(&ctx.banner);
    // Directly under the error strip, above everything that moves: a job that
    // takes long enough to need reporting takes long enough that the user is
    // looking at the folder it is working on, and a strip that pushed the
    // listing around as it appeared would be a worse interruption than the one it
    // reports. It is invisible until a job is registered, so it costs nothing.
    ctx.jobs.install(&content);
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
    /// The cards for `computer://`, for the same reason and by the same argument.
    /// Repainted by the handlers that hear about mounts, unmounts and free space.
    computer: Rc<ComputerView>,
    /// Grid/list against columns against the cards. The view keeps its own stack
    /// inside this one.
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
    /// The spacebar preview, for the same reason and by the same argument.
    preview: Rc<Preview>,
    /// The operations this window has running, and the strip that says so.
    ///
    /// `Rc` because the handlers that start a job and the strip's own widgets
    /// both need it, and `Jobs` is not `Clone` — the table inside is one per
    /// window, which is also the unit a Cancel button speaks for.
    jobs: Rc<Jobs>,
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
    /// The user's stars, kept in memory for the life of the window and saved
    /// on every change. Listings are stamped with it on their way in, which is
    /// what lets a badge appear in a folder, a search page and the collection
    /// itself from one copy of the truth.
    starred: RefCell<Starred>,
    /// The sidebar's bookmarks, kept in memory for the life of the window and
    /// saved on every change — the same shape as [`Shared::starred`], and for the
    /// same reason: the sidebar is rebuilt from it rather than read per row.
    bookmarks: RefCell<Bookmarks>,
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
    /// A message an operation asked the strip to say, waiting out the re-read
    /// that the operation itself caused.
    ///
    /// An operation that touched the folder on screen re-reads it, and a clean
    /// read normally hides the strip — which would hide the message a fraction of
    /// a second after it appeared, leaving a user who moved, renamed or threw
    /// away a row with no answer at all. So the message is kept here and put back
    /// by the event answering the read. Only an operation's own report is kept:
    /// a read failure describes a broken folder and must not outlive the next
    /// clean read of it, while "Moved back 1 item" stays true however often the
    /// folder is walked. Navigation clears it — a message about the folder being
    /// left should not follow the window to the next one.
    spoken: RefCell<Option<String>>,
}

impl Shared {
    /// Filesystem capacity for one path, read through the same reader the
    /// listings come from so the two cannot disagree about what exists.
    fn hints(&self, path: &std::path::Path) -> FsHints {
        self.readers.local().hints(path)
    }

    /// The bookmarks, as an owned list. Copied rather than borrowed because the
    /// callers build a sidebar out of them and cannot hold the borrow across a
    /// call that repaints — the panel is free to ask what the places are while
    /// the store is still being read.
    fn bookmark_items(&self) -> Vec<Bookmark> {
        self.bookmarks.borrow().items.clone()
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
    /// Likewise the cards. Their widget is inside `modes`, which is inside the
    /// window, but the handlers that repaint them hold this `Rc` and must not be
    /// the only thing keeping the flow alive.
    _computer: Rc<ComputerView>,
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
    /// Likewise the preview popover.
    _preview: Rc<Preview>,
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
        // The same for a message about the folder being left: it is about a
        // listing that is on its way out.
        *ctx.shared.spoken.borrow_mut() = None;
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
    // A preview left open says what one row of the folder being left was, and a
    // text read in flight belongs to a file that may not be on screen a moment
    // later. Closing it is cheaper than tracking whether it still applies.
    if let Some(window) = ctx.window.upgrade() {
        ctx.preview.hide(&window);
    }
    // Results describe a query, not this folder, so they leave when the folder
    // does. `reset` rather than `clear`: the listing coming on screen is the
    // thing that replaces them, and reporting `Cleared` here would paint over it.
    ctx.find.reset();
    ctx.find.set_folder(location.as_path());
    // Empty first, so a folder that is slow to read shows an empty view rather
    // than the folder being left. For a virtual location this also clears the
    // override the cards left in the status bar.
    view.set_entries(Vec::new());
    // A synthesized collection has no reader, and a worker that cannot answer a
    // location reports that as a failure — which is how `computer://` used to
    // arrive with a banner saying it was not a local path. The two that are
    // listings get one built here; the cards are the third presentation.
    if let Some(dir) = location.virtual_dir() {
        show_virtual(ctx, dir, &view);
        select_place(ctx, &location);
        sync_buttons(ctx);
        sync_watches(ctx);
        return;
    }
    // Only while columns are on screen: building a chain reads every folder on
    // the path, and in grid mode that is work for panes nobody can see.
    if columns_on(ctx) {
        ctx.columns.show_chain(&location);
    }
    select_place(ctx, &location);
    sync_buttons(ctx);

    spawn_read(ctx, location, cancel, &view);
    sync_watches(ctx);
}

/// Highlight the sidebar row a location belongs to, or clear the highlight.
fn select_place(ctx: &Ctx, location: &Location) {
    if let Some(panel) = ctx.panel.upgrade() {
        let mounts = mount_rows(&ctx.shared);
        match place_for(&ctx.shared.bookmark_items(), &mounts, location) {
            Some(place) => panel.select(&place),
            None => panel.clear_selection(),
        }
    }
}

/// Put a synthesized collection on screen: the cards, or a listing assembled from
/// the history rather than read from a directory.
///
/// The stack is switched here rather than left to the preference for this location,
/// because a collection is one presentation and not a choice the user makes per
/// folder: the cards are the only way to show a set of capacities, and a list of
/// files is a list of files.
fn show_virtual(ctx: &Ctx, dir: VirtualDir, view: &View) {
    match dir {
        VirtualDir::Computer => {
            ctx.computer.set_cards(&card_data(&ctx.shared));
            ctx.modes.set_visible_child_name("computer");
            // The cards are not rows, so the count in the bar would say "Empty
            // folder" about a window showing six machines.
            let places = u64::try_from(ctx.computer.card_count()).unwrap_or(u64::MAX);
            view.set_status(Some(&format_items(places)));
        }
        VirtualDir::Recent => {
            let readers = Arc::clone(&ctx.shared.readers);
            let entries: Vec<Entry> = recent::load()
                .iter()
                .filter_map(|record| readers.metadata(&Location::file(record.path.clone())).ok())
                .collect();
            ctx.shared.state.borrow_mut().shown.clone_from(&entries);
            ctx.modes.set_visible_child_name("view");
            view.set_entries(entries);
        }
        VirtualDir::Starred => show_starred(ctx, view),
    }
}

/// The starred collection, assembled from the store rather than read from a
/// folder. Each star is asked for its current metadata, which is what makes a
/// share that is not mounted right now, or a folder deleted outside the
/// window, simply absent from a collection that otherwise looks fine.
///
/// The rows are kept rather than dropped when they cannot be read, and the
/// count of them goes in the bar: a collection that quietly shrinks is worse
/// than one that says how much of it it could not show.
fn show_starred(ctx: &Ctx, view: &View) {
    let readers = Arc::clone(&ctx.shared.readers);
    let starred = ctx.shared.starred.borrow();
    let mut entries = Vec::with_capacity(starred.len());
    let mut missing = 0_usize;
    for star in starred.stars() {
        match readers.metadata(&star.location) {
            Ok(entry) => entries.push(entry),
            Err(_) => missing += 1,
        }
    }
    drop(starred);
    if entries.is_empty() && missing == 0 {
        ctx.modes.set_visible_child_name("view");
        show_banner(ctx, "Nothing starred yet");
        return;
    }
    // The rows come from the filesystem, which knows nothing about stars, so
    // they are stamped like any other listing. Every row here ends up with a
    // badge, which is repetitive and honest: the flag is what the menu reads to
    // decide between "Star" and "Unstar", and a collection that left it out
    // would offer a row called "Star" that takes a star away.
    ctx.shared.starred.borrow().stamp(&mut entries);
    // The count is what a collection owes its reader.
    let total = u64::try_from(entries.len()).unwrap_or(u64::MAX);
    let status = if missing > 0 {
        let gone = u64::try_from(missing).unwrap_or(u64::MAX);
        format!("{} · {gone} not available", format_items(total))
    } else {
        format_items(total)
    };
    ctx.shared.state.borrow_mut().shown.clone_from(&entries);
    ctx.modes.set_visible_child_name("view");
    // The empty-collection banner has to go, or the collection would arrive with
    // a strip saying there was nothing in it.
    ctx.banner.set_visible(false);
    view.set_entries(entries);
    view.set_status(Some(&status));
}

/// The cards for the My Computer view, read from the window's own mount table and
/// filesystem capabilities.
///
/// The kernel's table is re-read first: a share that died while this window was
/// open is a card that should say so, and the alternative is a badge that says
/// "Connected" about a mount point that is empty.
fn card_data(shared: &Rc<Shared>) -> Vec<Card> {
    let mut state = shared.state.borrow_mut();
    state.runner.refresh();
    computer::cards(
        state.runner.table(),
        |id| state.runner.state(id),
        |path| shared.hints(path),
    )
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
fn apply(ctx: &Ctx, mut event: Event) {
    // Stamping happens here rather than in each reader so that every listing
    // carries the same flags: folder reads, archive members and the virtual
    // views all arrive through this one point. The columns are updated below
    // from the same event, so a parent column's badges come from here too.
    if let Event::Batch { entries, .. } | Event::Finished { entries, .. } = &mut event {
        ctx.shared.starred.borrow().stamp(entries);
    }
    // First for the columns, which match events against their own panes rather
    // than against the window's current folder: a parent column's listing is not
    // for the folder in the title bar, and the check below would drop it.
    ctx.columns.apply(&event);
    // The two job events are answered before the view is looked for. Neither is
    // about a listing, and a window whose grid had gone away still has to hear
    // that a copy it started has finished — otherwise the strip would report a
    // job that had already stopped, with no way left to clear it.
    if let Event::Progress { report } = event {
        ctx.jobs.apply(&report);
        return;
    }
    if let Event::JobFinished { id, outcome } = event {
        ctx.jobs.finish(id);
        operated(ctx, outcome);
        return;
    }
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
            // A clean read hides the strip, unless an operation asked for a
            // message to stay: the read answering that operation would otherwise
            // eat its own report. See [`Shared::spoken`].
            match ctx.shared.spoken.borrow().as_deref() {
                Some(message) => show_banner(ctx, message),
                None => ctx.banner.set_visible(false),
            }
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
            // The failure replaces any operation message: it is the newer fact
            // about this folder, and a strip holding both would have to choose.
            *ctx.shared.spoken.borrow_mut() = None;
            show_banner(ctx, &format!("{}: {message}", location.display()));
        }
        Event::Mounted { report } => mounted(ctx, report),
        Event::Unmounted { report } => unmounted(ctx, report),
        Event::Operation { outcome } => operated(ctx, outcome),
        // Already answered above, before the view was looked for. The arm exists
        // because the match has to say something about every event, and a wildcard
        // here would swallow a future variant instead of asking about it.
        Event::Progress { .. } | Event::JobFinished { .. } => {}
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
        // Also not about the folder on screen: a member written out belongs to the
        // chooser that asked for it, which decides whether it is still open. The
        // chooser itself is built here rather than on the worker, because a
        // `gio::AppInfo` is neither `Send` nor `Sync` and the list of programs has
        // to be read on the main thread.
        Event::Materialised { id, ref path } => openwith::arrived(id, path),
        // Also not about the folder on screen: a text head belongs to the popover
        // that asked for it, and the token says which label it was. A preview
        // dismissed while its file was being read is the ordinary case, which is
        // why the answer is looked up rather than assumed wanted.
        Event::Preview { token, ref text } => preview::arrived(token, text.as_deref()),
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
    // The record goes to the table before anything else, so that the Undo
    // button is sensitive by the time the message that offers it has been read.
    // An operation that changed nothing carries no record, which is what stops a
    // refused paste from lighting a button that would undo the operation before it.
    if let Some(record) = outcome.undo {
        ctx.jobs.record(record);
    }
    // A star on a name that no longer exists is not a star on some future file
    // that happens to take the name, so the rows an operation destroyed are
    // dropped here. Trashed rows are not: Undo can bring them back, and with
    // them the star.
    if !outcome.removed.is_empty() {
        let forgotten = {
            let mut starred = ctx.shared.starred.borrow_mut();
            let forgotten = starred.forget(&outcome.removed);
            if forgotten > 0 {
                let _ = starred.save(&Starred::path());
            }
            forgotten
        };
        if forgotten > 0 {
            refresh_badges(ctx);
        }
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
    // The message goes up last, after any re-read this operation caused has
    // been set in motion: `navigate` clears what the strip is holding, and the
    // read answering that navigation would otherwise hide the report again a
    // moment later. Kept in `spoken` so that the read repainting the folder
    // puts the message back rather than eating it. See [`Shared::spoken`].
    if !outcome.message.is_empty() {
        *ctx.shared.spoken.borrow_mut() = Some(outcome.message.clone());
        show_banner(ctx, &outcome.message);
    }
    // Free space moved, and the sidebar shows free space.
    if let Some(panel) = ctx.panel.upgrade() {
        panel.set_usage(usages(&ctx.shared));
    }
    // …and so do the cards, which say the same thing about the same filesystems.
    ctx.computer.set_cards(&card_data(&ctx.shared));
}

/// A share a card's Disconnect asked to be let go, on a worker.
///
/// The shape of this handler is the difference between a mount and an unmount: a
/// mount steps into the share, because a user who typed an address wants to be
/// standing in it. An unmount does not move anybody — the share that just left is
/// the one the window may be showing, and walking out of it would be the window
/// admitting it did not know. So the table is re-read, the sidebar and the cards
/// are repainted, and the answer is said out loud either way.
fn unmounted(ctx: &Ctx, report: Report) {
    {
        let mut state = ctx.shared.state.borrow_mut();
        state.runner.reload();
    }
    // The mount point is gone, and so is every fact cached about it: the listing of
    // the folder it held, and the capacity and permissions of the filesystem that
    // was behind it. Without this the sidebar keeps a bar for a share that is now
    // an empty directory.
    ctx.shared.readers.clear_fs_cache();
    if let Some(panel) = ctx.panel.upgrade() {
        panel.set_places(places(&ctx.shared));
        panel.set_usage(usages(&ctx.shared));
    }
    ctx.computer.set_cards(&card_data(&ctx.shared));
    // The window may be standing in the share that left, or in a folder that was
    // under it. Its listing is now a listing of an empty directory, and the only
    // honest answer is to say what happened and let the user look.
    show_banner(ctx, &report.message);
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
    // Index results arrive from the search runner rather than through the queue,
    // so they miss the stamping in `apply` and need it here. Without it a star
    // would show in a folder and disappear in a search for the same file.
    let mut entries = page.entries;
    ctx.shared.starred.borrow().stamp(&mut entries);
    let filtered = filter(&entries, &ctx.shared.state.borrow().query);
    ctx.shared.state.borrow_mut().results = Some(entries);
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
        .set_visible_child_name(presentation(&location, mode));
    if mode == ViewMode::Columns && !location.is_virtual() {
        ctx.columns.show_chain(&location);
    }
}

/// Which child of the mode stack a location should be shown in.
///
/// The cards win over the preference: a set of filesystems is one presentation, not
/// a choice between grid and list, and a preference that moved the window off the
/// cards would leave the window showing a folder that is not the one in the title
/// bar. Columns are asked for only where there is a chain of folders to build.
fn presentation(location: &Location, mode: ViewMode) -> &'static str {
    if location.virtual_dir() == Some(VirtualDir::Computer) {
        return "computer";
    }
    // The other two collections are listings, so they use the ordinary view, and
    // only a real folder has a chain of folders to put in columns.
    if location.is_virtual() || mode != ViewMode::Columns {
        return "view";
    }
    "columns"
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
    // The share that just appeared is a card, and the window may be looking at the
    // cards rather than at a folder — in which case navigating below would be the
    // only other way to learn the mount happened.
    ctx.computer.set_cards(&card_data(&ctx.shared));
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
        state
            .runner
            .table()
            .get(id)
            .map(|record| mount_place(&state.runner, record))
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

/// The sidebar rows: the standard places, the user's bookmarks, and whatever is
/// in the mount table. The order belongs to [`Place::with_bookmarks`]; the mounts
/// are added last because they are their own group and appear below both.
fn places(shared: &Rc<Shared>) -> Vec<Place> {
    let mut places = Place::with_bookmarks(&shared.bookmark_items());
    places.extend(mount_rows(shared));
    places
}

/// The rows for the shares in the mount table, in the table's order.
///
/// One function rather than a literal at each place a mount row is needed, because
/// the panel matches a usage bar and a selection to a row by comparing [`Place`]
/// values: two builders that disagree about one field produce two different places
/// for one share, and the bar or the highlight for that share then goes missing
/// with no error anywhere. The three callers — the rows, the bars, and the
/// highlight — all go through here.
fn mount_rows(shared: &Rc<Shared>) -> Vec<Place> {
    let state = shared.state.borrow();
    state
        .runner
        .table()
        .iter()
        .map(|record| mount_place(&state.runner, record))
        .collect()
}

/// One mount row, carrying the three answers its menu is built from: the kernel's
/// about the mount point, the store's about the password, and the table's about the
/// next session. All three come from what the runner already holds in memory plus
/// one small file, so rebuilding the rows on every listing costs what reading a
/// folder costs.
fn mount_place(runner: &Runner, record: &MountRecord) -> Place {
    Place::Mount {
        id: record.id.clone(),
        label: record.share.label(),
        state: runner.state(&record.id),
        can_reconnect: runner.can_reconnect(&record.share),
        autostart: record.autostart,
    }
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
            place: mount_place(&state.runner, record),
            total: hints.total,
            free: hints.free,
        });
    }
    usages
}

/// The sidebar row a location belongs to, so the panel can agree with the view.
///
/// The bookmarks are searched before anything else, and by location rather than by
/// name: a folder the user bookmarked has two candidate rows — none, and the
/// bookmark — and it is the bookmark that should light up, since that is the row
/// they put there to get back to it. The mount rows come next, for the same
/// reason: standing in a share's mount point is standing at that share, and a
/// sidebar that goes blank there looks like it has lost track of the window.
///
/// The mount rows are passed in rather than rebuilt here, so that the comparison
/// below is against the values the panel was handed — see [`mount_rows`].
fn place_for(bookmarks: &[Bookmark], mounts: &[Place], location: &Location) -> Option<Place> {
    if let Some(bookmark) = bookmarks
        .iter()
        .find(|bookmark| &bookmark.location == location)
    {
        return Some(Place::Bookmark(bookmark.clone()));
    }
    if let Some(mount) = mounts.iter().find(|place| place.location() == *location) {
        return Some(mount.clone());
    }
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
    // First, and above the commands that act on a row: it is the only row here
    // that acts on the last operation rather than on the selection, and a user
    // who has just watched something they did go wrong looks at the top of the
    // list, not the middle. Disabled until there is something to undo, so the row
    // is honest about a folder nothing has touched yet.
    files.append(Some("Undo"), Some("win.undo"));
    // First among the commands that act on a row, because it is the one a user
    // reaches for before anything else: the folder opened by double-click and the
    // default handler disagree, or there is no default handler, and this is where
    // the machine says what it actually has registered. No chord, for the reason
    // the context menu's Open row has none — `Return` already means "take this
    // row", and a second name for one command is a chord wasted.
    files.append(Some("Open With\u{2026}"), Some("win.open-with"));
    files.append(Some("New Folder"), Some("win.new-folder"));
    // One row for both directions of the toggle, which is what a menu built once
    // can offer: the label cannot follow the selection, because the model is
    // built before there is one. The context menu has the rows on hand and says
    // "Star" or "Unstar" properly — see [`popup::Command`].
    files.append(Some("Star"), Some("win.star"));
    // The same compromise one row down: the header menu cannot know which way the
    // toggle goes, so it names the direction that adds. The context menu and
    // <kbd>Ctrl</kbd>D do not have the problem — the folder is known by then.
    files.append(Some("Bookmark This Folder"), Some("win.bookmark"));
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
    // The sidebar answers for itself as well, but only the rows that have anything
    // to say: [`panel::menu_for`] returns nothing for a derived place, and
    // [`Popup::show_rows`] opens nothing at all when handed no rows, so a
    // right-click on `Home` costs one call and shows no popover.
    //
    // The row is not selected first, which the view and the panes both do. The
    // sidebar's selection is a rendering of the folder the window is standing in,
    // so lighting a row the window did not walk into would leave the sidebar and
    // the title bar disagreeing about where the user is.
    panel.on_context({
        let ctx = ctx.clone();
        move |place, widget, x, y| {
            let Some(window) = ctx.window.upgrade() else {
                return;
            };
            ctx.popup
                .show_rows(&window, &panel::menu_for(place), Some((widget, x, y)));
        }
    });
    // The cards answer for themselves rather than through a row: a card names a
    // folder or a share, and the only two things worth doing with either are
    // walking into it and letting go of it.
    ctx.computer.on_open({
        let ctx = ctx.clone();
        move |id| open_card(&ctx, id)
    });
    ctx.computer.on_unmount({
        let ctx = ctx.clone();
        move |id| unmount_card(&ctx, &id)
    });
}

/// Walk into the folder or the mount point a card names.
fn open_card(ctx: &Ctx, id: CardId) {
    let location = match id {
        CardId::Local(path) => Location::file(path),
        // The mount point comes from the table rather than from a rule about where
        // mount points are made: the record is what the mount was made against,
        // and a card whose share has since been re-mounted elsewhere should open
        // where the record says it is now.
        CardId::Mount(id) => {
            let state = ctx.shared.state.borrow();
            match state.runner.table().get(&id) {
                Some(record) => Location::file(record.mountpoint.clone()),
                // The share left between the repaint and the click. The card is
                // already gone from the table, so the next repaint drops it too.
                None => return,
            }
        }
    };
    navigate(ctx, location, true);
}

/// Let go of a share, on a worker.
///
/// [`Runner::unmount`] blocks on `umount`, on a FUSE daemon's shutdown and
/// sometimes on polkit, and a window that froze for two seconds over a share that
/// was busy looked broken even when it was only waiting. The worker owns its own
/// runner, the same as the connect dialog's, because the record it retires has to
/// be written against a table read after the dialog's answer rather than against
/// one read when the window opened.
fn unmount_card(ctx: &Ctx, id: &str) {
    let Some(handle) = ctx.handle.borrow().clone() else {
        return;
    };
    let id = id.to_string();
    let spawned = thread::Builder::new()
        .stack_size(256 * 1024)
        .spawn(move || {
            let mut runner = Runner::open(Layout::system());
            let report = runner.unmount(&id);
            handle.send(Event::Unmounted { report });
        });
    if spawned.is_err() {
        // Out of threads. Nothing was attempted, so the message says so rather
        // than naming a backend that never ran.
        if let Some(handle) = ctx.handle.borrow().clone() {
            handle.send(Event::Unmounted {
                report: Report {
                    message: "cannot start an unmount".to_string(),
                    id: None,
                    ok: false,
                },
            });
        }
    }
}

/// The window actions and their accelerators. Split in three because each part
/// is a coherent set — moving, displaying, and the extras — and one function
/// holding all of them is longer than it is deep.
fn wire_actions(app: &gtk::Application, window: &gtk::ApplicationWindow, ctx: &Ctx) {
    let mut actions = nav_actions(ctx);
    actions.extend(view_actions(app, ctx));
    actions.extend(op_actions(ctx));
    actions.extend(bookmark_actions(ctx));
    actions.extend(mount_actions(ctx));
    actions.extend(context_actions(ctx));
    for action in &actions {
        ActionMapExt::add_action(window, action.as_ref());
    }
    install_accels(app);
    sync_buttons(ctx);
}

/// The four commands a mount row's menu offers, each about the share the row names
/// rather than about the folder in the title bar.
///
/// None is ever disabled, for the reason [`bookmark_actions`] gives: the gate is
/// the menu, which [`panel::menu_for`] builds only for a mount row and only in the
/// rows that row's state allows. What the kernel says can change while a menu sits
/// open — another window, the sweep on its timer, the share's own daemon dying — so
/// each handler asks again and says what it finds.
///
/// Two of the four run on a worker because both block on a helper process. The other
/// two edit one small file and answer in the same turn, which is why they do not
/// share a shape even though they share a menu.
fn mount_actions(ctx: &Ctx) -> Vec<Rc<gio::SimpleAction>> {
    let disconnect = gio::SimpleAction::new("disconnect", Some(glib::VariantTy::STRING));
    disconnect.connect_activate({
        let ctx = ctx.clone();
        move |_action, parameter| {
            let Some(id) = share_named(parameter) else {
                return;
            };
            unmount_card(&ctx, &id);
        }
    });

    let reconnect = gio::SimpleAction::new("reconnect", Some(glib::VariantTy::STRING));
    reconnect.connect_activate({
        let ctx = ctx.clone();
        move |_action, parameter| {
            let Some(id) = share_named(parameter) else {
                return;
            };
            reconnect_row(&ctx, &id);
        }
    });

    let forget = gio::SimpleAction::new("forget-share", Some(glib::VariantTy::STRING));
    forget.connect_activate({
        let ctx = ctx.clone();
        move |_action, parameter| {
            let Some(id) = share_named(parameter) else {
                return;
            };
            forget_share(&ctx, &id);
        }
    });

    let autostart = gio::SimpleAction::new("autostart-share", Some(glib::VariantTy::STRING));
    autostart.connect_activate({
        let ctx = ctx.clone();
        move |_action, parameter| {
            let Some(id) = share_named(parameter) else {
                return;
            };
            set_share_autostart(&ctx, &id);
        }
    });

    vec![
        Rc::new(disconnect),
        Rc::new(reconnect),
        Rc::new(forget),
        Rc::new(autostart),
    ]
}

/// The record id a sidebar menu row named.
///
/// An id rather than a URI, where a bookmark's menu row carries a URI: a share has
/// no location of its own until it is mounted, and the mount point is derived from
/// the id anyway. What the two have in common is that neither is a position in the
/// list, so a menu left open across a rebuild still names the row it opened over.
fn share_named(parameter: Option<&glib::Variant>) -> Option<String> {
    Some(parameter?.str()?.to_string())
}

/// Mount a listed share again, on a worker.
///
/// The same shape as [`unmount_card`] and for the same reason: mounting waits on a
/// helper process and sometimes on polkit, and the window that asked stays
/// responsive while it waits. The worker owns its own runner, so the answer arrives
/// as [`Event::Mounted`] and is handled by [`mounted`] — which re-reads the table,
/// repaints, and steps into the share. Stepping in is what a user who pressed
/// Reconnect wants too: the point of the row is to be standing in the share
/// afterwards, and the share's own contents are the success message.
///
/// A share whose password was never saved gets the backend's sentence about needing
/// one, shown as a banner. Reconnect deliberately has no password argument — it is
/// the row that costs one click — and [`panel::menu_for`] leaves the row out of the
/// menu for exactly that share, so reaching this function with such a share means
/// the answer was stored or forgotten while the menu was open.
fn reconnect_row(ctx: &Ctx, id: &str) {
    let Some(handle) = ctx.handle.borrow().clone() else {
        return;
    };
    let id = id.to_string();
    let spawned = thread::Builder::new()
        .stack_size(256 * 1024)
        .spawn(move || {
            let mut runner = Runner::open(Layout::system());
            let report = runner.reconnect(&id);
            handle.send(Event::Mounted { report });
        });
    if spawned.is_err() {
        // Out of threads. Nothing was attempted, so the message says so rather
        // than naming a backend that never ran.
        if let Some(handle) = ctx.handle.borrow().clone() {
            handle.send(Event::Mounted {
                report: Report {
                    message: "cannot start a mount".to_string(),
                    id: None,
                    ok: false,
                },
            });
        }
    }
}

/// Stop listing a share, from the sidebar row that named it.
///
/// Inline rather than on a worker, unlike the other two rows in the same menu:
/// [`Runner::forget_share`] runs no command — it refuses a share the kernel still
/// reports as mounted and otherwise edits one small file — so there is nothing to
/// wait for, and a row that had to be answered from a worker would repaint the
/// sidebar a frame later for no reason.
///
/// The refusal is the answer worth designing for, and it is why this does not reuse
/// [`unmount_card`]: letting go of a live share and taking its row away are two
/// decisions, and a menu row that silently did both would be a Disconnect row under
/// another name.
fn forget_share(ctx: &Ctx, id: &str) {
    let report = {
        let mut state = ctx.shared.state.borrow_mut();
        state.runner.forget_share(id)
    };
    // The row and the card follow the table. The filesystem cache is left alone on
    // purpose: nothing was unmounted, so no fact about a filesystem changed — the
    // mount point is either still a live share or still the empty directory it was
    // before, and both are what is already cached about it.
    if let Some(panel) = ctx.panel.upgrade() {
        panel.set_places(places(&ctx.shared));
        panel.set_usage(usages(&ctx.shared));
    }
    ctx.computer.set_cards(&card_data(&ctx.shared));
    show_banner(ctx, &report.message);
}

/// Flip whether a share connects at login, from the sidebar row that named it.
///
/// The action toggles rather than sets, because the menu row it comes from reads as
/// the opposite of the current answer and carries no boolean of its own: one
/// parameter shape for all four mount rows keeps [`panel::MenuRow`] a label plus an
/// action plus an id, and the row's label is already the feedback about what state
/// the press found. A menu left open across a change in another window therefore
/// acts on the state as it is now, which is what the other three rows do too.
///
/// Inline like [`forget_share`], for the same reason: one small file is rewritten
/// and no helper runs, so a worker would only delay the repaint by a frame.
fn set_share_autostart(ctx: &Ctx, id: &str) {
    let wanted = {
        let state = ctx.shared.state.borrow();
        !state
            .runner
            .table()
            .get(id)
            .is_some_and(|record| record.autostart)
    };
    let changed = {
        let mut state = ctx.shared.state.borrow_mut();
        state.runner.set_autostart(id, wanted)
    };
    // Repaint either way. The rows are built from the table, so a press that found
    // the share gone — forgotten in another window — has to drop the row rather
    // than leave it offering a menu for a share that is not listed.
    if let Some(panel) = ctx.panel.upgrade() {
        panel.set_places(places(&ctx.shared));
        panel.set_usage(usages(&ctx.shared));
    }
    ctx.computer.set_cards(&card_data(&ctx.shared));
    let message = match (changed, wanted) {
        (true, true) => "this share will connect at login",
        (true, false) => "this share will no longer connect at login",
        // Either the share is not listed or it already said this. Both read as
        // "nothing changed", which is the honest answer and not a failure: the
        // state the user asked for is the state the table is in.
        (false, true) => "this share already connects at login",
        (false, false) => "this share does not connect at login",
    };
    show_banner(ctx, message);
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
            let here = ctx.shared.state.borrow().current.clone();
            // The cards stay under a mode change: the choice is between grid, list
            // and columns, and a location with no rows behind it has none of the
            // three to move to. The preference below still records the choice, so
            // the next folder the user opens uses it.
            let child = here.as_ref().map_or_else(
                || if columns { "columns" } else { "view" },
                |location| presentation(location, mode),
            );
            ctx.modes.set_visible_child_name(child);
            // Coming back to columns after a while, or arriving here for the
            // first time: the chain has to be built, and any pane that was
            // dropped while hidden has to come back.
            if columns && !was_columns && child == "columns" {
                let (current, shown) = {
                    let state = ctx.shared.state.borrow();
                    (state.current.clone(), state.shown.clone())
                };
                if let Some(location) = current {
                    ctx.columns.show_chain(&location);
                    // The chain leaves the deepest pane to a read the window
                    // would normally be making. Here it is not: the folder is
                    // already on screen in another form, and its listing is
                    // sitting in `shown`. Handing those rows over is what stops
                    // the column for the folder in the title bar appearing empty
                    // until the next navigation. While a read really is running
                    // the pane stays as the chain left it, because those rows
                    // would then be thrown away by the answer that is coming.
                    if !ctx.shared.reading.get() {
                        ctx.columns.fill_leaf(&location, &shown);
                    }
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

    let rename = one_row_action("rename", ctx, |window, handle, entry| {
        ops::ask_rename(&window, &handle, entry);
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
            conflict::paste(&window, &handle, &ctx.jobs, plan, transfer);
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
            extract::extract(&handle, &ctx.jobs, container);
        }
    });

    // Registered disabled, and enabled by the table's own change callback rather
    // than by a sync function: unlike the other actions here, the answer to "is
    // there a job to stop?" changes while the window sits still, because a worker
    // finishes whenever it finishes. See [`cancel_action`].
    let cancel = cancel_action(ctx);
    // The same treatment for the same reason: an operation becomes available to
    // undo when its worker reports, and stops being available when it is spent.
    // See [`undo_action`].
    let undo = undo_action(ctx);

    let star = star_action(ctx);
    let bookmark = bookmark_action(ctx);

    let facts = one_row_action("properties", ctx, |window, handle, entry| {
        props::show(&window, &handle, entry);
    });

    let chooser = one_row_action("open-with", ctx, |window, handle, entry| {
        openwith::show(&window, &handle, entry);
    });

    vec![
        Rc::new(new_folder),
        rename,
        star,
        bookmark,
        discard_action("trash", false, ctx),
        discard_action("delete", true, ctx),
        pickup_action("copy", ops::Transfer::Copy, ctx),
        pickup_action("cut", ops::Transfer::Move, ctx),
        Rc::new(paste),
        Rc::new(unpack),
        cancel,
        undo,
        chooser,
        facts,
    ]
}

/// `star`: add or remove a star on every row the operation would act on.
///
/// One action for both directions, and the rows decide which way each of them
/// goes: a selection of three holding two stars and one plain row stars the
/// plain one and leaves the others alone, which is the only reading that never
/// destroys a star the user put somewhere else. The label the menu shows comes
/// from [`popup::Scope::starred`], which says whether every row is starred.
///
/// Nothing is read from disk here. The store is in memory, the listings are
/// re-stamped rather than re-read, and the only cost is writing one small file.
fn star_action(ctx: &Ctx) -> Rc<gio::SimpleAction> {
    let star = gio::SimpleAction::new("star", None);
    star.connect_activate({
        let ctx = ctx.clone();
        move |_action, _parameter| {
            let picked = targets(&ctx);
            if picked.is_empty() {
                return;
            }
            let now = SystemTime::now();
            let changed = {
                let mut starred = ctx.shared.starred.borrow_mut();
                picked
                    .iter()
                    // `toggle` reports the state a row is in afterwards rather
                    // than whether it moved, so counting its answer would count
                    // the stars put on and miss every star taken off — which
                    // would leave an un-star unpersisted and its badge lit.
                    .filter(|entry| {
                        let before = starred.contains(&entry.location);
                        starred.toggle(entry.location.clone(), now) != before
                    })
                    .count()
            };
            if changed == 0 {
                return;
            }
            let _ = ctx.shared.starred.borrow().save(&Starred::path());
            refresh_badges(&ctx);
        }
    });
    Rc::new(star)
}

/// Whether every row a menu covers already carries a star. An empty selection
/// counts as not starred: over the background there is no row to unstar, and the
/// menu leaves the command out there anyway.
fn all_starred(picked: &[Entry]) -> bool {
    !picked.is_empty()
        && picked
            .iter()
            .all(|entry| entry.flags.contains(EntryFlags::STARRED))
}

/// Tell everything on screen that the star list changed, so a badge follows the
/// change without a directory being read a second time.
///
/// Three places hold a row: the listings the window keeps, the model behind the
/// grid and list views, and the cached listings inside each column pane. All
/// three are re-stamped from the store. A star is not something the filesystem
/// stores, so a re-read would not settle it — the flags have to be rewritten
/// where they already are.
fn refresh_badges(ctx: &Ctx) {
    let starred = ctx.shared.starred.borrow().clone();
    {
        let mut state = ctx.shared.state.borrow_mut();
        starred.stamp(&mut state.shown);
        if let Some(results) = state.results.as_mut() {
            starred.stamp(results);
        }
    }
    if let Some(view) = ctx.view.upgrade() {
        view.restamp(&starred);
    }
    ctx.columns.restamp(&starred);
    // The collection itself can be the thing on screen, and a star removed from
    // a row there takes the row away rather than re-labelling it.
    let on_collection = ctx
        .shared
        .state
        .borrow()
        .current
        .as_ref()
        .is_some_and(|here| here.virtual_dir() == Some(VirtualDir::Starred));
    if on_collection && let Some(view) = ctx.view.upgrade() {
        show_starred(ctx, &view);
    }
}

/// An action whose handler acts on exactly one selected row, and stays quiet
/// otherwise rather than guessing which of them the user meant.
///
/// Renaming a selection of three has no obvious answer, and neither does a dialog
/// about three files or a chooser asked which of three to open: a user who
/// selected three usually wanted the folder they are looking at rather than a
/// summary of a third of it. `act` runs on the one row the selection holds.
fn one_row_action(
    name: &str,
    ctx: &Ctx,
    act: impl Fn(gtk::ApplicationWindow, Handle, &Entry) + 'static,
) -> Rc<gio::SimpleAction> {
    let action = gio::SimpleAction::new(name, None);
    action.connect_activate({
        let ctx = ctx.clone();
        move |_action, _parameter| {
            let Some((window, handle)) = sink_for(&ctx) else {
                return;
            };
            let picked = targets(&ctx);
            let [entry] = picked.as_slice() else {
                return;
            };
            act(window, handle, entry);
        }
    });
    Rc::new(action)
}

/// `undo`: put the last operation this window did back.
///
/// Built disabled like `win.cancel`, and for the same reason — the moment an
/// operation becomes undoable is a worker reporting an outcome, not a moment the
/// window picked, so [`wire_jobs`] connects the action to the job table. The table
/// holds one record, which is what makes the button say no the second time.
fn undo_action(ctx: &Ctx) -> Rc<gio::SimpleAction> {
    let undo = gio::SimpleAction::new("undo", None);
    undo.set_enabled(false);
    undo.connect_activate({
        let ctx = ctx.clone();
        move |_action, _parameter| {
            let Some(handle) = ctx.handle.borrow().clone() else {
                return;
            };
            undo::undo(&handle, &ctx.jobs);
        }
    });
    Rc::new(undo)
}

/// `cancel`: stop everything this window has running.
///
/// Built disabled. The window does not enable it from a sync function the way the
/// other file commands are synced, because a job ends on a worker at a moment the
/// window is not involved in; [`wire_jobs`] connects the action to the job table
/// itself, which is the only thing that always knows.
fn cancel_action(ctx: &Ctx) -> Rc<gio::SimpleAction> {
    let cancel = gio::SimpleAction::new("cancel", None);
    cancel.set_enabled(false);
    cancel.connect_activate({
        let jobs = Rc::clone(&ctx.jobs);
        move |_action, _parameter| jobs.pull_all()
    });
    Rc::new(cancel)
}

/// Which row the "Open With" chooser would be opened for, mirroring the
/// sensitivity rule in [`sync_open_with`] so the two cannot disagree.
///
/// Exactly one row, and one that an external program could be pointed at: a folder
/// is entered rather than opened, and a virtual location has no bytes to hand over.
fn open_with_target(ctx: &Ctx) -> Option<Entry> {
    let picked = targets(ctx);
    let [entry] = picked.as_slice() else {
        return None;
    };
    openwith::openable(entry).then(|| entry.clone())
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
    sync_open_with(ctx);
    sync_properties(ctx);
    sync_preview(ctx);
    sync_star(ctx);
    sync_bookmark(ctx);
}

/// Enable or disable the bookmark. The condition is the folder in the title bar
/// rather than the selection: a bookmark is a name for a place you can return to,
/// and the place a window is standing in is the one thing a user cannot mistake
/// for another. So the command needs a real folder — `computer://` and the two
/// collections are not places a later window can be pointed at.
fn sync_bookmark(ctx: &Ctx) {
    let Some(window) = ctx.window.upgrade() else {
        return;
    };
    enable(&window, "bookmark", target_folder(ctx).is_some());
}

/// Whether the folder on screen is already in the sidebar, which is what decides
/// whether a menu says `Bookmark` or `Remove Bookmark`. Asked of the store rather
/// than of the sidebar's rows, because the rows are a rendering of it — and through
/// [`Bookmarks::find`], so that the answer follows the same comparison the store
/// itself makes rather than a second rule written beside it.
fn current_bookmarked(ctx: &Ctx) -> bool {
    target_folder(ctx).is_some_and(|folder| ctx.shared.bookmarks.borrow().find(&folder).is_some())
}

/// `bookmark`: put the folder in the title bar in the sidebar, or take it out.
///
/// One action for both directions, like `star`, and for the same reason: a menu
/// that offered both would ask the user which one takes effect. The label is
/// decided per opening by [`popup::Scope::bookmarked`], which can tell the two
/// apart because it is built after the folder is known.
///
/// The folder in the title bar and not the selected row, because a bookmark names
/// a place to return to and a row inside a folder is not that folder. Nothing is
/// read from disk: the store is in memory, the sidebar is rebuilt from it, and the
/// only cost is writing one small file.
fn bookmark_action(ctx: &Ctx) -> Rc<gio::SimpleAction> {
    let action = gio::SimpleAction::new("bookmark", None);
    action.connect_activate({
        let ctx = ctx.clone();
        move |_action, _parameter| {
            let Some(folder) = target_folder(&ctx) else {
                return;
            };
            let name = folder
                .file_name()
                .map_or_else(|| folder.display(), ToString::to_string);
            let message = {
                let mut bookmarks = ctx.shared.bookmarks.borrow_mut();
                if bookmarks.find(&folder).is_some() {
                    bookmarks.remove(&folder);
                    format!("Bookmark removed for {name}")
                } else {
                    // No label, so the row says the folder's own name. That is the
                    // right default rather than a placeholder: the name a person
                    // would type is usually the one already on the folder, and
                    // `Rename` on the sidebar's menu is there for the times it is
                    // not.
                    bookmarks.add(folder.clone(), None);
                    format!("Bookmarked {name}")
                }
            };
            let _ = ctx.shared.bookmarks.borrow().save(&Bookmarks::path());
            bookmarks_changed(&ctx, &message);
        }
    });
    Rc::new(action)
}

/// Save the bookmark store and repaint the sidebar from it. The two halves of
/// every bookmark command, split out because all four of them do exactly this and
/// a command that forgot one of the two leaves the file and the screen disagreeing.
fn bookmarks_updated(ctx: &Ctx, message: &str) {
    let _ = ctx.shared.bookmarks.borrow().save(&Bookmarks::path());
    bookmarks_changed(ctx, message);
}

/// The bookmark a sidebar menu row named.
///
/// The parameter is the URI the row was built with, re-parsed rather than carried
/// as a position: the menu can sit open across a rebuild, and a position says
/// nothing about which bookmark was meant once the list has moved. A URI that no
/// longer names a bookmark is not an error to report — the store lookup that
/// follows answers it — so this only answers whether there is a place to look up.
fn bookmark_named(parameter: Option<&glib::Variant>) -> Option<Location> {
    Location::parse(parameter?.str()?).ok()
}

/// The four commands a sidebar row offers, all of them about the bookmark the row
/// names rather than about the folder in the title bar.
///
/// None of the four is ever disabled, unlike [`bookmark_action`]: the gate is the
/// menu itself, which [`panel::menu_for`] only builds for a bookmark row. What the
/// store holds can change while the menu is open — another window, or a folder
/// deleted out from under the row — so each handler asks the store again and says
/// so when the answer has gone.
fn bookmark_actions(ctx: &Ctx) -> Vec<Rc<gio::SimpleAction>> {
    vec![
        rename_bookmark_action(ctx),
        shift_bookmark_action("bookmark-up", true, ctx),
        shift_bookmark_action("bookmark-down", false, ctx),
        remove_bookmark_action(ctx),
    ]
}

/// `rename-bookmark`: ask what the shortcut should be called, then write it.
///
/// The prompt is the reason this is a separate action from [`bookmark_action`]:
/// that one knows the folder and not the new name, and this one knows the name and
/// not the folder. The initial text is [`Bookmark::display`] rather than the stored
/// label, so a bookmark that has never been renamed is offered the name its row
/// actually shows rather than an empty box.
fn rename_bookmark_action(ctx: &Ctx) -> Rc<gio::SimpleAction> {
    let action = gio::SimpleAction::new("rename-bookmark", Some(glib::VariantTy::STRING));
    action.connect_activate({
        let ctx = ctx.clone();
        move |_action, parameter| {
            let (Some(window), Some(location)) = (ctx.window.upgrade(), bookmark_named(parameter))
            else {
                return;
            };
            let Some(bookmark) = ctx
                .shared
                .bookmark_items()
                .into_iter()
                .find(|bookmark| bookmark.location == location)
            else {
                return;
            };
            let current = bookmark.display();
            let worker = ctx.clone();
            ops::ask_bookmark_label(&window, &current, move |typed| {
                let message = if worker
                    .shared
                    .bookmarks
                    .borrow_mut()
                    .rename(&location, Some(typed))
                {
                    "Bookmark renamed"
                } else {
                    // The prompt outlived the bookmark — removed in another window,
                    // or by the same keystroke twice. Nothing was written.
                    "That bookmark is no longer in the sidebar"
                };
                bookmarks_updated(&worker, message);
            });
        }
    });
    Rc::new(action)
}

/// `bookmark-up` and `bookmark-down`: one row, one direction, so one function.
///
/// The store's own reorder is what decides whether anything moves: a bookmark
/// already at the end of the list asked to go further down stays where it is, which
/// is why the row is offered rather than hidden at the edges — a menu whose rows
/// appear and disappear as the list moves is harder to learn than one that is
/// sometimes inert.
fn shift_bookmark_action(name: &'static str, up: bool, ctx: &Ctx) -> Rc<gio::SimpleAction> {
    let action = gio::SimpleAction::new(name, Some(glib::VariantTy::STRING));
    action.connect_activate({
        let ctx = ctx.clone();
        move |_action, parameter| {
            let Some(location) = bookmark_named(parameter) else {
                return;
            };
            let message = {
                let mut bookmarks = ctx.shared.bookmarks.borrow_mut();
                let Some(at) = bookmarks.find(&location) else {
                    return;
                };
                let name = bookmarks.items[at].display();
                let to = if up {
                    at.saturating_sub(1)
                } else {
                    at.saturating_add(1)
                };
                bookmarks.move_item(at, to);
                let word = if up { "up" } else { "down" };
                format!("Moved {name} {word}")
            };
            bookmarks_updated(&ctx, &message);
        }
    });
    Rc::new(action)
}

/// `remove-bookmark`: take the row out of the sidebar.
///
/// Not the same action as <kbd>Ctrl</kbd>D, which acts on the folder in the title
/// bar. Reached from a row, this one has to act on the row, and standing in
/// `~/Projects` while right-clicking the shortcut for `/srv/data` is exactly the
/// case where the two differ.
fn remove_bookmark_action(ctx: &Ctx) -> Rc<gio::SimpleAction> {
    let action = gio::SimpleAction::new("remove-bookmark", Some(glib::VariantTy::STRING));
    action.connect_activate({
        let ctx = ctx.clone();
        move |_action, parameter| {
            let Some(location) = bookmark_named(parameter) else {
                return;
            };
            let name = {
                let bookmarks = ctx.shared.bookmarks.borrow();
                // Already gone — removed in another window while this menu was open.
                // Nothing to write, and no message claiming a row was taken out.
                let Some(at) = bookmarks.find(&location) else {
                    return;
                };
                bookmarks.items[at].display()
            };
            ctx.shared.bookmarks.borrow_mut().remove(&location);
            bookmarks_updated(&ctx, &format!("Bookmark removed for {name}"));
        }
    });
    Rc::new(action)
}

/// Tell the sidebar its bookmarks changed, and keep the row that matches the
/// folder on screen lit.
///
/// The whole list is rebuilt rather than one row inserted. A sidebar is a few
/// dozen rows and the rebuild is what [`Panel`] already does for mounts, so a diff
/// would be a second way to get the headings wrong. Rebuilding drops the
/// selection, which is why the current place is re-highlighted afterwards —
/// without it, bookmarking the folder you are standing in would leave the sidebar
/// agreeing with nothing.
fn bookmarks_changed(ctx: &Ctx, message: &str) {
    if let Some(panel) = ctx.panel.upgrade() {
        panel.set_places(places(&ctx.shared));
        panel.set_usage(usages(&ctx.shared));
    }
    let current = ctx.shared.state.borrow().current.clone();
    if let Some(location) = current {
        select_place(ctx, &location);
    }
    show_banner(ctx, message);
    sync_bookmark(ctx);
}

/// Enable or disable the star. Any row can be starred, and any number of them
/// together, so the condition is the simplest of the row commands: there has to
/// be a row. Starring the folder in the title bar is what the sidebar row and
/// <kbd>Ctrl</kbd>T are for instead.
fn sync_star(ctx: &Ctx) {
    let Some(window) = ctx.window.upgrade() else {
        return;
    };
    enable(&window, "star", !targets(ctx).is_empty());
}

/// Enable or disable the preview. One row is the whole condition: any row at all
/// can be previewed, because the popover decides what to say from what the row
/// is, and a folder with no text and no picture still gets its facts.
fn sync_preview(ctx: &Ctx) {
    let Some(window) = ctx.window.upgrade() else {
        return;
    };
    enable(&window, "preview", targets(ctx).len() == 1);
}

/// Enable or disable the chooser. Depends on what the one selected row *is* — a
/// folder has no program to take it — which is a third answer than extract's or
/// properties' and so its own function.
fn sync_open_with(ctx: &Ctx) {
    let Some(window) = ctx.window.upgrade() else {
        return;
    };
    enable(&window, "open-with", open_with_target(ctx).is_some());
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
            ops::discard(&handle, &ctx.jobs, &picked, permanent);
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
            // Two popovers over one row answer one question, so the preview steps
            // aside rather than competing for the same corner of the screen.
            ctx.preview.hide(&window);
            ctx.popup.show(&window, &menu_rows(&ctx, true), None);
        }
    });

    // The spacebar preview. Enabled for exactly one row, like Properties: the
    // question a preview answers is about one thing, and a popover that tried to
    // summarise six files would be a report rather than a glance.
    let preview = gio::SimpleAction::new("preview", None);
    preview.connect_activate({
        let ctx = ctx.clone();
        move |_action, _parameter| {
            let (Some(window), Some(handle)) = (ctx.window.upgrade(), ctx.handle.borrow().clone())
            else {
                return;
            };
            let picked = targets(&ctx);
            let [entry] = picked.as_slice() else {
                return;
            };
            // The preview anchors its autohide grab on the window's focus, so a
            // window left without one — just after a dialog or chooser closed and
            // took it — would show a popover that dismisses itself as it maps.
            // A real preview is asked for from a focused row; putting focus back on
            // whatever listing is on screen makes the popover behave the same here.
            if gtk::prelude::GtkWindowExt::focus(&window).is_none()
                && let Some(visible) = ctx.modes.visible_child()
            {
                visible.grab_focus();
            }
            ctx.preview.toggle(&window, &handle, &ctx.thumbs, entry);
        }
    });

    vec![Rc::new(open), Rc::new(menu), Rc::new(preview)]
}

/// The rows for a menu opened without a press, from the state the window is in.
fn menu_rows(ctx: &Ctx, in_current: bool) -> Vec<popup::Command> {
    let picked = targets(ctx);
    let scope = popup::Scope {
        in_current,
        extracting: extract_target(ctx).is_some(),
        openable: open_with_target(ctx).is_some(),
        pastable: can_paste(ctx),
        starred: all_starred(&picked),
        bookmarked: current_bookmarked(ctx),
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
        openable: open_with_target(ctx).is_some(),
        pastable: can_paste(ctx),
        starred: all_starred(&picked),
        bookmarked: current_bookmarked(ctx),
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
    // The star. `<Ctrl>T` rather than a letter with a tradition elsewhere: `T` is
    // the initial of the only word the command has, and the chord is free.
    app.set_accels_for_action("win.star", &["<Ctrl>T"]);
    // The bookmark. `<Ctrl>D` is what the browsers and the GNOME files use for it,
    // and unlike the star there is a tradition to follow rather than invent. In an
    // entry the key deletes forward, which the entry's own controller keeps — the
    // same arrangement that lets <kbd>Ctrl</kbd>V paste a file without pasting into
    // a name being renamed.
    app.set_accels_for_action("win.bookmark", &["<Ctrl>D"]);
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
    // The usual chord, and one deep. `<Ctrl>Z` reaches an entry too, where it
    // undoes typing — but an entry binds the key on its own controller and handles
    // the event before the window's accelerator sees it, the same way it keeps
    // `<Ctrl>V` from pasting a file into the middle of a name being renamed.
    app.set_accels_for_action("win.undo", &["<Ctrl>Z"]);
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
    // The preview. The rows answer `Return` and the arrows, and Space is what
    // every other file manager uses for this, so the chord costs nothing and
    // buys the feature its name.
    app.set_accels_for_action("win.preview", &["space"]);
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
    use ufm_mount::monitor::MountState;

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
        assert!(place_for(&[], &[], &Location::file(paths::home())).is_some());
        assert!(place_for(&[], &[], &Location::file("/definitely/not/a/place")).is_none());
    }

    #[test]
    fn a_bookmarked_folder_becomes_a_place_under_the_name_it_was_given() {
        let work = Location::file("/srv/work");
        let bookmarks = [Bookmark {
            location: work.clone(),
            label: Some("Work".to_string()),
            icon: None,
        }];
        let found = place_for(&bookmarks, &[], &work);
        assert_eq!(
            found.map(|place| place.label()),
            Some("Work".to_string()),
            "a bookmark lights its own row rather than leaving the sidebar blank"
        );
    }

    #[test]
    fn standing_in_a_share_lights_the_row_for_that_share() {
        // The row the panel was handed, built by the same helper that built the
        // list — a mount point compared against a row assembled differently would
        // not match, and the sidebar would go blank mid-share.
        let mount = Place::Mount {
            id: "smb-nas-filmy".to_string(),
            label: "filmy".to_string(),
            state: MountState::Mounted,
            can_reconnect: true,
            autostart: false,
        };
        let point = mount.location();
        let found = place_for(&[], &[mount], &point);
        assert_eq!(
            found.map(|place| place.label()),
            Some("filmy".to_string()),
            "a mount point is its share's row, not an unlisted folder"
        );
    }
}
