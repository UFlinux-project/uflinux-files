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

use std::cell::RefCell;
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
use super::job::{Event, Handle, Queue};
use super::panel::{Panel, Place, Usage};
use super::row::Row;
use super::view::View;
use crate::runner::{Layout, Runner};

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

    let panel = Rc::new(Panel::new());
    let view = Rc::new(View::new(key));
    view.set_mode(mode);
    let columns = Rc::new(Columns::new(Arc::clone(&readers), key, prefs.show_hidden));
    let (banner, banner_label) = banner_bar();
    let path = path_label(&start);

    let shared = Rc::new(Shared {
        prefs: RefCell::new(prefs),
        state: RefCell::new(State::new(&start)),
        readers,
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
        shared: Rc::clone(&shared),
        handle: Rc::new(RefCell::new(None)),
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

    let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
    content.append(&ctx.banner);
    let split = gtk::Paned::new(gtk::Orientation::Horizontal);
    split.set_start_child(Some(panel.widget()));
    split.set_end_child(Some(&ctx.modes));
    split.set_shrink_start_child(false);
    split.set_position(220);
    content.append(&split);
    content.append(&status_bar(&view));
    window.set_child(Some(&content));
    window.set_titlebar(Some(&header(&ctx, &menu())));

    panel.set_places(places(&shared));
    panel.set_usage(usages(&shared));
    wire_handlers(&panel, &view, &columns, &ctx);

    wire_actions(app, &window, &ctx);
    window.connect_close_request({
        let shared = Rc::clone(&shared);
        let weak = window.downgrade();
        move |_window| {
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
        }
    });

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
            _queue: queue,
        });
    });
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
    shared: Rc<Shared>,
    /// Filled in once the queue exists, which is after the handlers are built.
    handle: Rc<RefCell<Option<Handle>>>,
}

/// State shared by every handler in one window.
struct Shared {
    prefs: RefCell<Preferences>,
    state: RefCell<State>,
    readers: Arc<Readers>,
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
    query: String,
    runner: Runner,
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
            query: String::new(),
            runner,
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
    fn push(&mut self, location: Location) {
        if self.items.last() == Some(&location) {
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
    /// Removing the fd source is what stops the wake-ups; the field owns it.
    _queue: Queue,
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
    let readers = Arc::clone(&ctx.shared.readers);
    let key = view.sort_key();
    let show_hidden = ctx.shared.prefs.borrow().show_hidden;
    let cancel = Cancel::new();
    let Some(handle) = ctx.handle.borrow().clone() else {
        return;
    };

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
        state.query.clear();
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
        view.set_entries(Vec::new());
        show_banner(ctx, &format!("{display}: cannot start a reader."));
    }
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
            view.set_entries(filtered);
            ctx.banner.set_visible(false);
            let _ = hints;
            if let Some(panel) = ctx.panel.upgrade() {
                panel.set_usage(usages(&ctx.shared));
            }
        }
        Event::Failed { location, message } => {
            if !ctx.shared.state.borrow().shows(&location) {
                return;
            }
            show_banner(ctx, &format!("{}: {message}", location.display()));
        }
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
                filter(&state.shown, &text)
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
            let shown = ctx.shared.state.borrow().shown.clone();
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
    let window = gio::Menu::new();
    window.append(Some("Search this folder"), Some("win.search"));
    window.append(Some("New window"), Some("win.new-window"));
    menu.append_section(None, &view);
    menu.append_section(None, &show);
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
}

/// The window actions and their accelerators. Split in three because each part
/// is a coherent set — moving, displaying, and the extras — and one function
/// holding all of them is longer than it is deep.
fn wire_actions(app: &gtk::Application, window: &gtk::ApplicationWindow, ctx: &Ctx) {
    let mut actions = nav_actions(ctx);
    actions.extend(view_actions(app, ctx));
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

/// What the window shows, and the two commands that are neither.
fn view_actions(app: &gtk::Application, ctx: &Ctx) -> Vec<Rc<gio::SimpleAction>> {
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
            let current = ctx.shared.state.borrow().current.clone();
            if let Some(location) = current {
                navigate(&ctx, location, false);
            }
        }
    });

    let search = gio::SimpleAction::new("search", None);
    search.connect_activate({
        let ctx = ctx.clone();
        move |_action, _parameter| {
            if let Some(entry) = ctx.search.borrow().as_ref() {
                entry.set_visible(true);
                entry.grab_focus();
            }
        }
    });

    let new_window = gio::SimpleAction::new("new-window", None);
    new_window.connect_activate({
        let app = app.clone();
        move |_action, _parameter| open(&app, &[])
    });

    vec![
        Rc::new(mode),
        Rc::new(hidden),
        Rc::new(search),
        Rc::new(new_window),
    ]
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
    app.set_accels_for_action("win.new-window", &["<Ctrl>N"]);
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
        let _ = shared.state.borrow_mut().runner.auto_unmount();
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
fn status_bar(view: &Rc<View>) -> gtk::Box {
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
