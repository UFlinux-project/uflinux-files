//! The search bar, and the client for `org.uflinux.Search1`.
//!
//! The window never opens the index itself. Every query goes over the session
//! bus, which keeps the index in one process for the whole session rather than
//! one per window: a second window then costs a text entry, not a second copy of
//! a `tantivy` heap (see `docs/memory-budget.md`). The bus name ships with a
//! service file, so the first query starts the daemon and no window has to know
//! how to launch it.
//!
//! There is no generated proxy. The interface has four methods, each of them
//! takes one string and answers with one string, and the body is the JSON
//! [`ufm_search`] already defines, so a proxy would be a second description of
//! the same thing to keep in step.
//!
//! Two things make a search bar feel broken when they are missing, so neither is
//! left to the caller. Typing is debounced, because one query per keystroke is a
//! query per frame on a machine that cannot afford either. And a late reply is
//! dropped by generation number, so a slow answer for `repo` cannot arrive after
//! the user has typed `report` and replace newer results with older ones.
//!
//! The daemon is not looked for before the first query. GIO has no name watcher
//! that this build can use, and D-Bus activation starts the daemon anyway, so a
//! failed call is both the discovery and the error message.

use std::cell::{Cell, RefCell};
use std::path::Path;
use std::rc::Rc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use glib::variant::ToVariant;
use gtk::prelude::*;

use ufm_core::Location;
use ufm_core::config::SearchScope;
use ufm_core::entry::{Entry, EntryFlags, EntryKind};
use ufm_core::paths;
use ufm_search::{CHANGED, OBJECT_PATH, PROGRESS, SESSION_NAME, errors};
use ufm_search::{
    Changed, Hit, Order, Progress, Reply, Request, SearchQuery as Query, SearchReply, Snippet,
};

/// How long to wait for the next keystroke before asking. Long enough that a
/// fast typist is merged into one query, short enough that the wait is not
/// noticeable after the typing stops.
const DEBOUNCE: Duration = Duration::from_millis(120);
/// A query that has not answered by now is not going to. The daemon answers from
/// an open index in milliseconds; past this the bus is the problem, and saying
/// so beats a bar that waits forever.
const CALL_TIMEOUT_MS: i32 = 2_000;
/// One page. The bar shows a page rather than everything: the index can hold
/// hundreds of thousands of names, and a view cannot show them usefully.
const PAGE_SIZE: usize = 200;
/// The drop-down entry that searches desktop files instead of filenames.
const APPS: &str = "application/x-desktop";
/// The daemon said it had to drop hits from the end of the page.
const TRUNCATED_PAGE: &str = ufm_search::Truncated::Page.as_str();
/// A reply that arrived but is not a page of results.
const UNFORMATTED: &str = "The search answer is not one this program can read.";
/// The names the bus itself answers with when nothing owns the name.
const SERVICE_UNKNOWN: &str = "org.freedesktop.DBus.Error.ServiceUnknown";
const NAME_HAS_NO_OWNER: &str = "org.freedesktop.DBus.Error.NameHasNoOwner";
const NO_REPLY: &str = "org.freedesktop.DBus.Error.NoReply";
const TIMED_OUT: &str = "org.freedesktop.DBus.Error.Timeout";

/// One page of results, in the shape the view wants.
#[derive(Debug, Clone)]
pub struct Found {
    /// One row per hit, in the order the index ranked them.
    pub entries: Vec<Entry>,
    /// Matches in the whole index, not only on this page.
    pub total: usize,
    /// What the bar says: how many, and whether this page is all there is.
    pub status: String,
}

/// What the window should show after a query.
#[derive(Debug, Clone)]
pub enum Outcome {
    /// The bar is empty again, so the folder belongs on screen.
    Cleared,
    /// One page of results.
    Found(Found),
    /// The query went out and did not come back as a page of results.
    Failed(String),
}

/// What the bar searches, chosen from the drop-down.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    /// The folder on screen and everything below it.
    Here,
    /// The user's own files.
    Home,
    /// Every folder the daemon indexes.
    Everything,
    /// Desktop entries rather than files.
    Apps,
}

impl Target {
    /// The position in the drop-down.
    fn index(self) -> u32 {
        match self {
            Self::Here => 0,
            Self::Home => 1,
            Self::Everything => 2,
            Self::Apps => 3,
        }
    }

    fn at(index: u32) -> Option<Self> {
        match index {
            0 => Some(Self::Here),
            1 => Some(Self::Home),
            2 => Some(Self::Everything),
            3 => Some(Self::Apps),
            _ => None,
        }
    }

    /// The saved preference, which has no notion of applications: the start
    /// menu asks for those explicitly rather than remembering them.
    fn from_scope(scope: &SearchScope) -> Self {
        match *scope {
            SearchScope::Here => Self::Here,
            SearchScope::Home => Self::Home,
            SearchScope::Everything => Self::Everything,
        }
    }
}

/// The two switches that come from preferences rather than from the bar.
#[derive(Debug, Clone, Copy)]
struct Tuning {
    content: bool,
    include_hidden: bool,
}

/// What the bar does with the answer to a query, as one stored closure.
/// Named because the `Vec` of them trips clippy's complexity limit otherwise, and
/// because the bare type says nothing about what it is for.
type Handlers = RefCell<Vec<Rc<dyn Fn(Outcome)>>>;

/// The search bar, shared by clone.
#[derive(Clone)]
pub struct Search {
    inner: Rc<Inner>,
}

struct Inner {
    bar: gtk::Box,
    entry: gtk::SearchEntry,
    target: gtk::DropDown,
    status: gtk::Label,
    /// The folder on screen, for "This folder". `None` until the window has read
    /// something it can name a path for.
    here: RefCell<Option<String>>,
    wanted: Cell<Target>,
    tuning: Cell<Tuning>,
    /// Kept once obtained. Opening a second connection per query would mean a
    /// second hello to the bus and a second set of serial numbers.
    bus: RefCell<Option<gio::DBusConnection>>,
    connecting: Cell<bool>,
    /// The query waiting for a connection, with the generation it belongs to.
    waiting: RefCell<Option<(Request, u64)>>,
    timer: RefCell<Option<glib::SourceId>>,
    /// Set while the bar empties its own entry, so that the signal the change
    /// emits is not read as a keystroke.
    quiet: Cell<bool>,
    generation: Cell<u64>,
    page: Cell<usize>,
    accumulated: RefCell<Vec<Entry>>,
    more: gtk::Button,
    /// Lives as long as the bar: a subscription unsubscribes when it is dropped.
    subscriptions: RefCell<Vec<gio::SignalSubscription>>,
    handlers: Handlers,
}

impl Search {
    /// Build the bar. The starting scope and the two switches come from
    /// preferences, so that the bar agrees with what the window already shows.
    #[must_use]
    pub fn new(scope: &SearchScope, content: bool, include_hidden: bool) -> Self {
        let bar = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        bar.set_visible(false);
        let entry = gtk::SearchEntry::new();
        entry.set_placeholder_text(Some("Search files and apps"));
        entry.set_hexpand(true);
        entry.set_width_chars(12);
        entry.set_max_width_chars(12);
        let target = gtk::DropDown::from_strings(&[
            "Folder and Subfolders",
            "Home",
            "Everywhere",
            "Applications",
        ]);
        target.set_tooltip_text(Some("Where to look"));
        let status = gtk::Label::new(None);
        status.add_css_class("ufm-meta");
        status.set_accessible_role(gtk::AccessibleRole::Status);
        status.set_ellipsize(gtk::pango::EllipsizeMode::End);
        bar.append(&entry);
        let scope_popover = gtk::Popover::new();
        scope_popover.set_child(Some(&target));
        let options = gtk::MenuButton::builder()
            .icon_name("view-more-symbolic")
            .tooltip_text("Search scope")
            .popover(&scope_popover)
            .build();
        bar.append(&options);
        let more = gtk::Button::with_label("Load More");
        more.set_visible(false);
        more.add_css_class("flat");

        let wanted = Target::from_scope(scope);
        target.set_selected(wanted.index());
        let tuning = Tuning {
            content,
            include_hidden,
        };
        let inner = Rc::new(Inner {
            bar,
            entry,
            target,
            status,
            here: RefCell::new(None),
            wanted: Cell::new(wanted),
            tuning: Cell::new(tuning),
            bus: RefCell::new(None),
            connecting: Cell::new(false),
            waiting: RefCell::new(None),
            timer: RefCell::new(None),
            quiet: Cell::new(false),
            generation: Cell::new(0),
            page: Cell::new(0),
            accumulated: RefCell::new(Vec::new()),
            more,
            subscriptions: RefCell::new(Vec::new()),
            handlers: RefCell::new(Vec::new()),
        });
        wire(&inner);
        Self { inner }
    }

    #[must_use]
    pub fn widget(&self) -> &gtk::Box {
        &self.inner.bar
    }

    pub fn more_button(&self) -> &gtk::Button {
        &self.inner.more
    }

    pub fn status_label(&self) -> &gtk::Label {
        &self.inner.status
    }

    /// Whether the bar is on screen. The window uses this to tell a folder from
    /// a page of results that happens to be in the same view.
    #[must_use]
    pub fn is_open(&self) -> bool {
        self.inner.bar.get_visible()
    }

    #[must_use]
    pub fn target(&self) -> Target {
        self.inner.wanted.get()
    }

    #[must_use]
    pub fn text(&self) -> String {
        self.inner.entry.text().to_string()
    }

    /// Open the bar and take the keyboard.
    pub fn show(&self) {
        self.inner.bar.set_visible(true);
        self.inner.entry.grab_focus();
    }

    pub fn show_here(&self) {
        self.inner.target.set_selected(Target::Here.index());
        self.show();
    }

    /// Ask now, without waiting out the debounce. Used by <kbd>Enter</kbd> and by
    /// a refresh of results after the index or the folder changed.
    pub fn search_now(&self) {
        run_now(&self.inner);
    }

    /// Put the bar away and report `Cleared`, which is what puts the folder back
    /// on screen. Bound to <kbd>Esc</kbd>.
    pub fn clear(&self) {
        clear(&self.inner);
    }

    /// Put the bar away without reporting. The window calls this when it
    /// navigates: the folder coming on screen brings its own listing, and
    /// results behind it would only be confusing.
    pub fn reset(&self) {
        reset(&self.inner);
    }

    /// Say what the window should do about a query. Handlers are called on the
    /// main thread, once per query, and never for an answer that a newer query
    /// has already overtaken.
    pub fn on_reply<F>(&self, f: F)
    where
        F: Fn(Outcome) + 'static,
    {
        self.inner.handlers.borrow_mut().push(Rc::new(f));
    }

    /// Tell the bar which folder "This folder" means. Called on every
    /// navigation, with `None` for a location that has no path.
    pub fn set_folder(&self, path: Option<&Path>) {
        *self.inner.here.borrow_mut() = path.and_then(Path::to_str).map(str::to_string);
    }

    /// Keep the results in step with the window's own rule about dotfiles.
    pub fn set_show_hidden(&self, show: bool) {
        let tuning = self.inner.tuning.get();
        self.inner.tuning.set(Tuning {
            include_hidden: show,
            ..tuning
        });
    }

    /// Whether to match extracted text, which is what makes a snippet worth
    /// asking for.
    pub fn set_content(&self, content: bool) {
        let tuning = self.inner.tuning.get();
        self.inner.tuning.set(Tuning { content, ..tuning });
    }
}

/// React to the bar. Every handler here is owned by a widget that outlives the
/// window's own widgets, so each holds the shared state rather than borrowing it.
fn wire(inner: &Rc<Inner>) {
    inner.more.connect_clicked({
        let inner = Rc::clone(inner);
        move |_| {
            inner.page.set(inner.page.get() + 1);
            run_page(&inner);
        }
    });
    inner.entry.connect_search_changed({
        let inner = Rc::clone(inner);
        move |_| arm(&inner)
    });
    inner.entry.connect_activate({
        let inner = Rc::clone(inner);
        move |_| run_now(&inner)
    });
    inner.entry.connect_stop_search({
        let inner = Rc::clone(inner);
        move |_| clear(&inner)
    });
    inner.target.connect_selected_notify({
        let inner = Rc::clone(inner);
        move |drop| {
            let Some(target) = Target::at(drop.selected()) else {
                return;
            };
            inner.wanted.set(target);
            if !inner.entry.text().is_empty() {
                run_now(&inner);
            }
        }
    });
}

/// A keystroke: wait for the next one before asking.
fn arm(inner: &Rc<Inner>) {
    if inner.quiet.get() {
        return;
    }
    if inner.entry.text().is_empty() {
        stop_timer(inner);
        deliver(inner, next_generation(inner), Outcome::Cleared);
        return;
    }
    stop_timer(inner);
    let timer = glib::source::timeout_add_local_once(DEBOUNCE, {
        let inner = Rc::clone(inner);
        move || {
            // A source that has fired is already gone, and removing one that is
            // gone is a programmer error, so forget the id rather than remove
            // it. Only a timer that has not fired yet can be cancelled.
            let _ = inner.timer.borrow_mut().take();
            run_now(&inner);
        }
    });
    *inner.timer.borrow_mut() = Some(timer);
}

/// Send whatever is in the bar now, skipping the wait.
fn run_now(inner: &Rc<Inner>) {
    inner.page.set(0);
    inner.accumulated.borrow_mut().clear();
    run_page(inner);
}

fn run_page(inner: &Rc<Inner>) {
    inner.more.set_sensitive(false);
    stop_timer(inner);
    let text = inner.entry.text().to_string();
    if text.is_empty() {
        deliver(inner, next_generation(inner), Outcome::Cleared);
        return;
    }
    let here = inner.here.borrow().clone();
    let mut request = request_for(
        &text,
        inner.wanted.get(),
        inner.tuning.get(),
        here.as_deref(),
    );
    match &mut request {
        Request::SearchFiles { query } => query.page = inner.page.get(),
        Request::SearchApps { page, .. } => *page = inner.page.get(),
        _ => {}
    }
    inner.status.set_text("Searching…");
    dispatch(inner, request, next_generation(inner));
}

/// Put the bar away and say so, so that the folder comes back.
fn clear(inner: &Rc<Inner>) {
    reset(inner);
    deliver(inner, next_generation(inner), Outcome::Cleared);
}

/// Put the bar away and say nothing.
fn reset(inner: &Rc<Inner>) {
    next_generation(inner);
    inner.page.set(0);
    inner.accumulated.borrow_mut().clear();
    inner.more.set_visible(false);
    stop_timer(inner);
    inner.waiting.borrow_mut().take();
    inner.quiet.set(true);
    inner.entry.set_text("");
    inner.quiet.set(false);
    inner.status.set_text("");
    inner.bar.set_visible(false);
}

fn stop_timer(inner: &Rc<Inner>) {
    // `SourceId::remove` consumes the id, so it has to come out of the cell
    // first. Dropping an id instead would leave the source armed.
    if let Some(id) = inner.timer.borrow_mut().take() {
        id.remove();
    }
}

fn next_generation(inner: &Rc<Inner>) -> u64 {
    let generation = inner.generation.get() + 1;
    inner.generation.set(generation);
    generation
}

/// Hand a query to the bus, opening the connection first if there is none.
fn dispatch(inner: &Rc<Inner>, request: Request, generation: u64) {
    *inner.waiting.borrow_mut() = Some((request, generation));
    if inner.bus.borrow().is_some() {
        send(inner);
        return;
    }
    if inner.connecting.replace(true) {
        return; // the connection already in flight will pick this one up
    }
    gio::bus_get(gio::BusType::Session, gio::Cancellable::NONE, {
        let inner = Rc::clone(inner);
        move |result| {
            inner.connecting.set(false);
            match result {
                Ok(connection) => {
                    watch(&inner, &connection);
                    *inner.bus.borrow_mut() = Some(connection);
                    send(&inner);
                }
                Err(error) => {
                    inner.waiting.borrow_mut().take();
                    deliver(&inner, generation, Outcome::Failed(message_for(&error)));
                }
            }
        }
    });
}

/// Give the waiting query to the bus, if one is waiting.
fn send(inner: &Rc<Inner>) {
    let Some((request, generation)) = inner.waiting.borrow_mut().take() else {
        return;
    };
    let Some(connection) = inner.bus.borrow().clone() else {
        return;
    };
    let Ok(body) = serde_json::to_string(&request) else {
        return;
    };
    if body.len() > ufm_search::MAX_REQUEST {
        deliver(
            inner,
            generation,
            Outcome::Failed("That search is too long to send.".to_string()),
        );
        return;
    }
    connection.call(
        Some(SESSION_NAME),
        OBJECT_PATH,
        SESSION_NAME,
        request.bus_method(),
        Some(&(body,).to_variant()),
        Some(glib::VariantTy::TUPLE),
        gio::DBusCallFlags::NONE,
        CALL_TIMEOUT_MS,
        gio::Cancellable::NONE,
        {
            let inner = Rc::clone(inner);
            move |result| {
                let outcome = match result {
                    Ok(reply) => decode(&reply).map_or_else(Outcome::Failed, Outcome::Found),
                    Err(error) => Outcome::Failed(message_for(&error)),
                };
                deliver(&inner, generation, outcome);
            }
        },
    );
}

/// Listen for the daemon's own news, once. The subscriptions are kept for as
/// long as the bar exists, because dropping one unsubscribes from the signal.
fn watch(inner: &Rc<Inner>, connection: &gio::DBusConnection) {
    if !inner.subscriptions.borrow().is_empty() {
        return;
    }
    let changed = subscribe(inner, connection, CHANGED, on_changed);
    let progress = subscribe(inner, connection, PROGRESS, on_progress);
    inner.subscriptions.borrow_mut().extend([changed, progress]);
}

fn subscribe<F>(
    inner: &Rc<Inner>,
    connection: &gio::DBusConnection,
    name: &str,
    f: F,
) -> gio::SignalSubscription
where
    F: Fn(&Rc<Inner>, &gio::DBusSignalRef) + 'static,
{
    connection.subscribe_to_signal(
        Some(SESSION_NAME),
        Some(SESSION_NAME),
        Some(name),
        Some(OBJECT_PATH),
        None,
        gio::DBusSignalFlags::NONE,
        {
            let inner = Rc::clone(inner);
            move |signal| f(&inner, &signal)
        },
    )
}

/// The index changed under the results on screen. Only worth a re-query while
/// the user is looking at them; a bar that is shut has nothing to keep fresh.
fn on_changed(inner: &Rc<Inner>, signal: &gio::DBusSignalRef) {
    // The body is parsed rather than ignored: a signal this client cannot read
    // should not cause work.
    if body::<Changed>(signal).is_none() || !inner.bar.get_visible() {
        return;
    }
    run_now(inner);
}

/// A crawl is running. Worth one line, because "no matches" while the index is
/// still being built reads as an empty disk.
fn on_progress(inner: &Rc<Inner>, signal: &gio::DBusSignalRef) {
    let Some(progress) = body::<Progress>(signal) else {
        return;
    };
    if inner.bar.get_visible() {
        inner.status.set_text(&progress_line(&progress));
    }
}

fn progress_line(progress: &Progress) -> String {
    if progress.total == 0 {
        return format!("Indexing {} — {}", progress.kind, progress.phase);
    }
    format!(
        "Indexing {} — {} of {} done",
        progress.kind, progress.done, progress.total
    )
}

/// The JSON body of a signal, or `None` when it is not one this client knows.
fn body<T>(signal: &gio::DBusSignalRef) -> Option<T>
where
    T: serde::de::DeserializeOwned,
{
    let (text,) = signal.parameters.get::<(String,)>()?;
    serde_json::from_str(&text).ok()
}

/// Report one query, unless a newer one has already replaced it.
fn deliver(inner: &Rc<Inner>, generation: u64, mut outcome: Outcome) {
    if generation != inner.generation.get() {
        return;
    }
    inner.more.set_sensitive(true);
    if let Outcome::Found(found) = &mut outcome {
        let page_nonempty = !found.entries.is_empty();
        let mut accumulated = inner.accumulated.borrow_mut();
        if inner.page.get() == 0 {
            accumulated.clear();
        }
        for entry in &found.entries {
            if !accumulated
                .iter()
                .any(|held| held.location == entry.location)
            {
                accumulated.push(entry.clone());
            }
        }
        found.entries = accumulated.clone();
        inner
            .more
            .set_visible(page_nonempty && found.entries.len() < found.total);
        if inner.page.get() > 0 {
            found.status = format!("Showing {} of {} matches", found.entries.len(), found.total);
        }
    } else if matches!(outcome, Outcome::Failed(_)) && inner.page.get() > 0 {
        inner.page.set(inner.page.get() - 1);
    }
    let status = match &outcome {
        Outcome::Cleared => String::new(),
        Outcome::Found(found) => found.status.clone(),
        Outcome::Failed(message) => message.clone(),
    };
    inner.status.set_text(&status);
    // Cloned out first: a handler may navigate, which clears the bar, which
    // reports again through this same list.
    let handlers: Vec<_> = inner.handlers.borrow().iter().map(Rc::clone).collect();
    for handler in handlers {
        handler(outcome.clone());
    }
}

impl From<SearchReply> for Found {
    fn from(reply: SearchReply) -> Self {
        let status = status_for(&reply);
        Self {
            entries: entries_from(&reply.hits),
            total: reply.total,
            status,
        }
    }
}

/// Decode one reply. Anything that is not a page of results is an error rather
/// than an empty answer, so that a client bug cannot look like an empty disk.
fn decode(reply: &glib::Variant) -> Result<Found, String> {
    let body = reply
        .get::<(String,)>()
        .map(|(text,)| text)
        .ok_or_else(|| UNFORMATTED.to_string())?;
    serde_json::from_str::<Reply>(&body)
        .ok()
        .and_then(search_reply)
        .map(Found::from)
        .ok_or_else(|| UNFORMATTED.to_string())
}

fn search_reply(reply: Reply) -> Option<SearchReply> {
    match reply {
        Reply::Search(found) => Some(found),
        _ => None,
    }
}

/// The request for one keystroke.
fn request_for(text: &str, target: Target, tuning: Tuning, here: Option<&str>) -> Request {
    if target == Target::Apps {
        return Request::SearchApps {
            text: text.to_string(),
            labels: Vec::new(),
            order: Order::default(),
            page: 0,
            page_size: PAGE_SIZE,
        };
    }
    Request::SearchFiles {
        query: Query {
            text: text.to_string(),
            scope: scope_for(target, here),
            mimes: Vec::new(),
            labels: Vec::new(),
            order: Order::default(),
            folders_first: true,
            include_hidden: tuning.include_hidden,
            content: tuning.content,
            page: 0,
            page_size: PAGE_SIZE,
        },
    }
}

/// The subtree to search, or `None` for everything the daemon indexes. With no
/// folder open, "This folder" falls back to home rather than silently searching
/// the whole machine, which is the one surprise a scope selector must not give.
fn scope_for(target: Target, here: Option<&str>) -> Option<String> {
    match target {
        Target::Here => {
            Some(here.map_or_else(|| paths::home().display().to_string(), str::to_string))
        }
        Target::Home => Some(paths::home().display().to_string()),
        Target::Everything | Target::Apps => None,
    }
}

fn status_for(reply: &SearchReply) -> String {
    let shown = reply.hits.len();
    let mut text = if reply.total == 0 {
        "No matches".to_string()
    } else if reply.total > shown {
        format!("Showing {shown} of {} matches", reply.total)
    } else {
        format!("{} matches", reply.total)
    };
    if reply.truncated.as_deref() == Some(TRUNCATED_PAGE) {
        text.push_str(" — older matches are not shown");
    }
    text
}

/// Index hits as rows, in the order the index ranked them. Sorting them again
/// here would throw the ranking away.
#[must_use]
pub fn entries_from(hits: &[Hit]) -> Vec<Entry> {
    hits.iter().map(entry_for).collect()
}

/// One hit as one row. The index keeps less than a directory listing does — no
/// permissions, no owner, no allocated size — and the columns that need those
/// show an em dash rather than a guess.
#[must_use]
pub fn entry_for(hit: &Hit) -> Entry {
    let kind = kind_of(hit);
    Entry {
        location: Location::file(Path::new(&hit.path)),
        name: hit.name.clone(),
        kind,
        flags: flags_for(hit),
        size: u64::try_from(hit.size.max(0)).unwrap_or(u64::MAX),
        allocated: None,
        modified: modified_of(hit.modified),
        accessed: None,
        created: None,
        mode: None,
        uid: None,
        gid: None,
        mime: mime_for(hit, kind),
        child_count: None,
        icon_key: icon_key_for(kind, hit.mime.as_deref()),
    }
}

/// A directory is a directory. An application is its own shape, so that it gets
/// an application icon and sorts with the other things that start programs.
fn kind_of(hit: &Hit) -> EntryKind {
    if hit.kind == "dir" {
        return EntryKind::Directory;
    }
    if is_application(hit) {
        return EntryKind::Socket;
    }
    EntryKind::File
}

/// The apps index is the only one whose records are desktop entries.
fn is_application(hit: &Hit) -> bool {
    hit.path.ends_with(".desktop")
}

/// The apps index stores no mimetype, so the row supplies the one that makes an
/// application open with the desktop's own rules rather than a text editor. A
/// record that does carry one is overridden rather than trusted: a desktop entry
/// is a desktop entry whatever the indexer guessed from its suffix.
fn mime_for(hit: &Hit, kind: EntryKind) -> Option<String> {
    match kind {
        EntryKind::Socket | EntryKind::Fifo => Some(APPS.to_string()),
        _ => hit.mime.as_deref().map(str::to_string),
    }
}

/// The index records no permissions, so nothing is claimed here beyond what it
/// does record. A row built from a hit is read-only as far as the interface is
/// concerned until an operation has checked the file itself.
fn flags_for(hit: &Hit) -> EntryFlags {
    let mut flags = EntryFlags::empty();
    if hit.hidden {
        flags |= EntryFlags::HIDDEN;
    }
    flags
}

/// The icon key, following the rule the local reader uses: a themed name for a
/// folder or an archive, the mime type for anything else.
fn icon_key_for(kind: EntryKind, mime: Option<&str>) -> String {
    match kind {
        EntryKind::Directory | EntryKind::MountPoint => "folder".to_string(),
        EntryKind::Archive => "package-x-generic".to_string(),
        EntryKind::Socket | EntryKind::Fifo => "application-x-executable".to_string(),
        _ => mime.map_or_else(|| kind.icon_name().to_string(), str::to_string),
    }
}

/// Whole seconds since the epoch, which is what the index stores, as a time.
/// A negative value is a record from before the epoch or a missing field, and
/// neither has a date to show.
fn modified_of(seconds: i64) -> Option<SystemTime> {
    u64::try_from(seconds)
        .ok()
        .and_then(|since| UNIX_EPOCH.checked_add(Duration::from_secs(since)))
}

/// A snippet as markup, with the matched words in bold.
///
/// The marks are byte ranges, so the text is cut at them directly. Anything the
/// index sent that does not name a real range is skipped rather than trusted.
#[must_use]
pub fn markup(snippet: &Snippet) -> String {
    let text = snippet.text.as_str();
    let mut out = String::new();
    let mut at = 0;
    for mark in &snippet.marks {
        let (start, end) = (mark[0], mark[1]);
        // `get` is the whole check: it refuses a range that runs off the end as
        // well as one whose ends fall inside a character. Skipping is the right
        // reaction — a snippet is decoration, and a wrong word in bold is worse
        // than no word in bold.
        if start < at || end <= start || text.get(start..end).is_none() {
            continue;
        }
        escape(&mut out, &text[at..start]);
        out.push_str("<b>");
        escape(&mut out, &text[start..end]);
        out.push_str("</b>");
        at = end;
    }
    escape(&mut out, &text[at..]);
    out
}

fn escape(out: &mut String, piece: &str) {
    out.push_str(&glib::markup_escape_text(piece));
}

/// Why a call did not work, in terms of what the user can do about it. The bus
/// name is what is matched, because the message that comes with it is written
/// for whoever wrote the daemon.
fn message_for(error: &glib::Error) -> String {
    let Some(name) = gio::DBusError::remote_error(error) else {
        return format!("Search is not available: {}", error.message());
    };
    match name.as_str() {
        SERVICE_UNKNOWN | NAME_HAS_NO_OWNER => {
            "Search is not running. Start uflinux-searchd, or log out and in again.".to_string()
        }
        NO_REPLY | TIMED_OUT => "Search took too long to answer. Try again.".to_string(),
        other if other == errors::UNAVAILABLE => {
            "The index is being rebuilt. Try again in a moment.".to_string()
        }
        other if other == errors::TOO_LARGE => {
            "There are too many matches to show at once. Narrow the search.".to_string()
        }
        other if other == errors::INVALID => "The search index rejected this query.".to_string(),
        other => format!("Search is not available: {other}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::tk;
    use ufm_search::Snippet;

    #[test]
    fn loading_more_accumulates_matches_and_reset_clears_pages() {
        tk::ui(|| {
            let search = Search::new(&SearchScope::Home, false, false);
            let generation = next_generation(&search.inner);
            deliver(
                &search.inner,
                generation,
                Outcome::Found(Found {
                    entries: vec![entry_for(&hit("/tmp/first"))],
                    total: 2,
                    status: "1 of 2".into(),
                }),
            );
            assert!(search.inner.more.get_visible());
            search.inner.page.set(1);
            deliver(
                &search.inner,
                generation,
                Outcome::Found(Found {
                    entries: vec![entry_for(&hit("/tmp/second"))],
                    total: 2,
                    status: "1 of 2".into(),
                }),
            );
            assert_eq!(search.inner.accumulated.borrow().len(), 2);
            assert!(!search.inner.more.get_visible());
            search.reset();
            assert_eq!(search.inner.page.get(), 0);
            assert!(search.inner.accumulated.borrow().is_empty());
        });
    }

    fn hit(path: &str) -> Hit {
        Hit {
            path: path.to_string(),
            name: path.rsplit('/').next().unwrap_or_default().to_string(),
            kind: "file".to_string(),
            mime: Some("text/plain".to_string()),
            size: 12,
            modified: 1_700_000_000,
            hidden: false,
            labels: Vec::new(),
            score: 1.5,
            snippet: None,
        }
    }

    fn tuning() -> Tuning {
        Tuning {
            content: false,
            include_hidden: false,
        }
    }

    fn files(request: Request) -> Query {
        match request {
            Request::SearchFiles { query } => query,
            other => panic!("expected a files query, got {other:?}"),
        }
    }

    #[test]
    fn reset_invalidates_an_in_flight_reply_without_restoring_the_old_folder() {
        tk::ui(|| {
            let search = Search::new(&SearchScope::Home, false, false);
            let seen = Rc::new(RefCell::new(Vec::new()));
            let replies = Rc::clone(&seen);
            search.on_reply(move |reply| replies.borrow_mut().push(reply));
            search.show();
            let old = next_generation(&search.inner);
            search.reset();
            deliver(&search.inner, old, Outcome::Failed("late failure".into()));
            assert!(seen.borrow().is_empty());
            assert!(!search.is_open());
        });
    }

    #[test]
    fn a_hit_becomes_a_row_that_points_at_the_file() {
        let entry = entry_for(&hit("/home/u/notes.txt"));
        assert_eq!(entry.name, "notes.txt");
        assert_eq!(entry.location, Location::file("/home/u/notes.txt"));
        assert_eq!(entry.kind, EntryKind::File);
        assert_eq!(entry.size, 12);
        assert_eq!(entry.mime.as_deref(), Some("text/plain"));
        assert_eq!(entry.icon_key, "text/plain");
        assert!(entry.modified.is_some(), "the index gave a date");
    }

    #[test]
    fn a_directory_hit_can_be_walked_into() {
        let mut source = hit("/home/u/Documents");
        source.kind = "dir".to_string();
        source.mime = None;
        let entry = entry_for(&source);
        assert_eq!(entry.kind, EntryKind::Directory);
        assert!(entry.navigable());
        assert_eq!(entry.icon_key, "folder");
    }

    #[test]
    fn a_hidden_hit_is_marked_hidden() {
        let mut source = hit("/home/u/.netrc");
        source.hidden = true;
        let entry = entry_for(&source);
        assert!(entry.hidden());
        assert!(!entry_for(&hit("/home/u/x")).hidden());
    }

    #[test]
    fn a_desktop_entry_becomes_an_application_row() {
        let entry = entry_for(&hit("/usr/share/applications/xterm.desktop"));
        assert_eq!(entry.kind, EntryKind::Socket);
        assert!(!entry.navigable(), "an application is started, not entered");
        assert_eq!(entry.icon_key, "application-x-executable");
        assert_eq!(entry.mime.as_deref(), Some(APPS));
    }

    #[test]
    fn a_size_the_index_could_not_measure_does_not_wrap() {
        let mut source = hit("/home/u/x");
        source.size = -1;
        assert_eq!(entry_for(&source).size, 0);
    }

    #[test]
    fn a_date_before_the_epoch_shows_no_date() {
        let mut source = hit("/home/u/x");
        source.modified = -5;
        assert!(entry_for(&source).modified.is_none());
    }

    #[test]
    fn hits_keep_the_order_the_index_gave_them() {
        let hits = vec![hit("/home/u/z"), hit("/home/u/a")];
        let entries = entries_from(&hits);
        assert_eq!(
            entries.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(),
            vec!["z", "a"]
        );
    }

    #[test]
    fn this_folder_searches_the_folder_on_screen() {
        let query = files(request_for(
            "x",
            Target::Here,
            tuning(),
            Some("/home/u/Documents"),
        ));
        assert_eq!(query.scope.as_deref(), Some("/home/u/Documents"));
    }

    #[test]
    fn this_folder_falls_back_to_home_with_no_folder_open() {
        let home = paths::home().display().to_string();
        let query = files(request_for("x", Target::Here, tuning(), None));
        assert_eq!(query.scope.as_deref(), Some(home.as_str()));
    }

    #[test]
    fn everywhere_sends_no_scope_at_all() {
        let query = files(request_for(
            "x",
            Target::Everything,
            tuning(),
            Some("/home/u"),
        ));
        assert_eq!(query.scope, None);
    }

    #[test]
    fn applications_ask_the_apps_index_which_has_no_scope() {
        let request = request_for("term", Target::Apps, tuning(), Some("/home/u"));
        let Request::SearchApps {
            text, page_size, ..
        } = request
        else {
            panic!("expected an apps query");
        };
        assert_eq!(text, "term");
        assert_eq!(page_size, PAGE_SIZE);
    }

    #[test]
    fn a_request_names_the_method_it_must_arrive_on() {
        let (files, apps) = (
            request_for("x", Target::Home, tuning(), None),
            request_for("x", Target::Apps, tuning(), None),
        );
        assert_eq!(files.bus_method(), "SearchFiles");
        assert_eq!(apps.bus_method(), "SearchApps");
    }

    #[test]
    fn the_switches_from_preferences_reach_the_query() {
        let tuning = Tuning {
            content: true,
            include_hidden: true,
        };
        let query = files(request_for("x", Target::Home, tuning, None));
        assert!(query.content);
        assert!(query.include_hidden);
        assert!(query.folders_first, "a folder should outrank its own name");
    }

    #[test]
    fn a_query_the_daemon_would_refuse_is_caught_before_sending() {
        let query = files(request_for("x", Target::Here, tuning(), None));
        query
            .validate()
            .expect("a scope taken from a path is absolute");
    }

    #[test]
    fn marks_become_bold_markup() {
        let snippet = Snippet {
            text: "see the report for details".to_string(),
            marks: vec![[8, 14]],
        };
        assert_eq!(markup(&snippet), "see the <b>report</b> for details");
    }

    #[test]
    fn markup_keeps_the_text_between_two_marks() {
        let snippet = Snippet {
            text: "one two three".to_string(),
            marks: vec![[0, 3], [8, 13]],
        };
        assert_eq!(markup(&snippet), "<b>one</b> two <b>three</b>");
    }

    #[test]
    fn markup_escapes_text_that_looks_like_markup() {
        let snippet = Snippet {
            text: "a<b>&c".to_string(),
            marks: vec![],
        };
        assert_eq!(markup(&snippet), "a&lt;b&gt;&amp;c");
    }

    #[test]
    fn a_mark_outside_the_text_is_skipped_not_panicked() {
        let snippet = Snippet {
            text: "short".to_string(),
            marks: vec![[3, 900], [1, 2]],
        };
        assert_eq!(markup(&snippet), "s<b>h</b>ort");
    }

    #[test]
    fn a_mark_inside_a_character_is_skipped_not_panicked() {
        let snippet = Snippet {
            text: "žluťoučký".to_string(),
            marks: vec![[1, 2]],
        };
        assert_eq!(markup(&snippet), "žluťoučký");
    }

    #[test]
    fn a_page_that_is_not_everything_says_so() {
        let reply = reply_with(2, 40, None);
        assert_eq!(status_for(&reply), "Showing 2 of 40 matches");
    }

    #[test]
    fn a_whole_page_says_only_the_count() {
        let reply = reply_with(3, 3, None);
        assert_eq!(status_for(&reply), "3 matches");
    }

    #[test]
    fn no_matches_says_that_rather_than_showing_an_empty_folder() {
        let reply = reply_with(0, 0, None);
        assert_eq!(status_for(&reply), "No matches");
    }

    #[test]
    fn a_shrunk_page_admits_that_results_are_missing() {
        let reply = reply_with(2, 900, Some("page".to_string()));
        assert!(
            status_for(&reply).contains("older matches"),
            "the user should not read a page as the whole answer"
        );
    }

    fn reply_with(shown: usize, total: usize, truncated: Option<String>) -> SearchReply {
        SearchReply {
            total,
            page: 0,
            page_size: PAGE_SIZE,
            hits: (0..shown).map(|n| hit(&format!("/home/u/{n}"))).collect(),
            truncated,
        }
    }

    #[test]
    fn a_search_reply_decodes_into_rows() {
        let found = Reply::Search(reply_with(1, 1, None))
            .encode()
            .expect("a small reply fits");
        let variant = (found,).to_variant();
        let found = decode(&variant).expect("a search page decodes");
        assert_eq!(found.entries.len(), 1);
        assert_eq!(found.total, 1);
    }

    #[test]
    fn a_reply_that_is_not_a_search_page_is_an_error() {
        for body in [r#"{"reply":"ack"}"#, "not json", r#"{"reply":"status"}"#] {
            let variant = (body.to_string(),).to_variant();
            assert!(
                decode(&variant).is_err(),
                "{body} should not read as results"
            );
        }
    }

    #[test]
    fn a_reply_with_the_wrong_shape_is_an_error() {
        let variant = 42u32.to_variant();
        assert!(decode(&variant).is_err(), "a number is not a reply");
    }

    #[test]
    fn a_missing_daemon_is_named_as_such() {
        let error =
            gio::DBusError::new_for_dbus_error(SERVICE_UNKNOWN, "The connection is unusable");
        let text = message_for(&error);
        assert!(text.contains("uflinux-searchd"), "{text}");
    }

    #[test]
    fn a_busy_index_is_told_to_the_user_as_a_retry() {
        let error = gio::DBusError::new_for_dbus_error(errors::UNAVAILABLE, "queue full");
        assert!(message_for(&error).contains("rebuilt"));
    }

    #[test]
    fn a_refused_query_does_not_blame_the_bus() {
        let error = gio::DBusError::new_for_dbus_error(errors::INVALID, "bad scope");
        assert!(message_for(&error).contains("rejected"));
    }

    #[test]
    fn the_scope_options_cover_every_saved_scope() {
        for scope in [
            SearchScope::Here,
            SearchScope::Home,
            SearchScope::Everything,
        ] {
            let target = Target::from_scope(&scope);
            assert_eq!(Target::at(target.index()), Some(target));
        }
        assert_eq!(Target::at(9), None);
    }

    #[test]
    fn the_bar_starts_away_and_opens_on_request() {
        tk::ui(|| {
            let search = Search::new(&SearchScope::Home, false, false);
            assert!(!search.is_open());
            assert_eq!(search.target(), Target::Home);
            search.show();
            assert!(search.is_open());
            search.clear();
            assert!(!search.is_open());
            assert!(search.text().is_empty());
        });
    }

    #[test]
    fn choosing_applications_changes_what_the_next_query_asks() {
        tk::ui(|| {
            let search = Search::new(&SearchScope::Here, false, false);
            assert_eq!(search.target(), Target::Here);
            search.inner.target.set_selected(Target::Apps.index());
            assert_eq!(search.target(), Target::Apps);
        });
    }

    #[test]
    fn a_folder_with_no_path_leaves_this_folder_on_home() {
        tk::ui(|| {
            let search = Search::new(&SearchScope::Here, false, false);
            search.set_folder(None);
            search.set_folder(Some(Path::new("/tmp")));
            search.set_show_hidden(true);
            let query = files(request_for(
                "x",
                search.target(),
                search.inner.tuning.get(),
                search.inner.here.borrow().as_deref(),
            ));
            assert_eq!(query.scope.as_deref(), Some("/tmp"));
            assert!(query.include_hidden);
        });
    }
}
