//! Miller columns: one pane per folder in the path, each showing the next.
//!
//! The widget renders a *chain*: for `/home/u/projects/src` it draws four panes,
//! `/`, `/home`, `/home/u` and `projects`, the last one selected on `src`. The
//! chain is derived from the location the window navigated to rather than kept
//! as state of its own, which is what keeps the two modes honest: the history,
//! the path label and the columns can never disagree, because there is one
//! source of truth and it is the window's current location.
//!
//! Two consequences of that shape run through this file.
//!
//! Panes are cached and reused. Rebuilding a chain keeps every pane that is
//! still in it, with its rows, so moving deeper or back up costs a read of the
//! one folder that was not already on screen. This matters more here than in the
//! other modes: [`ufm_vfs::LocalReader`] keeps no cache of its own, so a pane
//! that threw its listing away would walk the directory again on every arrow
//! key.
//!
//! The deepest pane is filled by the window rather than by this widget. The
//! window already reads the folder it navigated to for the grid and list views,
//! and a second read of the same directory would double the cost of every
//! navigation for the one folder the user is about to look at. So the last pane
//! is left marked as waiting, and its listing arrives through [`Columns::apply`]
//! like any other.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;
use std::thread;

use gio::prelude::*;
use glib::Propagation;
use gtk::prelude::*;

use ufm_core::Location;
use ufm_core::entry::Entry;
use ufm_core::sort::{SortKey, sort_entries};
use ufm_vfs::{Readers, read_all};

use super::job::{Event, Handle};
use super::row::Row;
use super::view::{GRID_ICON, grid_factory};

/// How many panes may be on screen. Deep paths lose their topmost components
/// rather than their leaf: the folder being read is the one being worked in,
/// and `/` and `/home` are the least interesting columns to spend a pane on.
const MAX_PANES: usize = 6;

/// Pane width in pixels. Wide enough for an icon and a two-line name, narrow
/// enough that a laptop shows four columns without scrolling.
const PANE_WIDTH: i32 = 240;

/// A callback into the window: what to do with a row the user opened. Named
/// because the field type it ends up in is hard to read otherwise.
type OpenHandler = Rc<dyn Fn(&Entry)>;

/// A callback into the window: what to do when the keyboard asks for the parent
/// folder, which is the window's business because the window owns the history.
type UpHandler = Rc<dyn Fn()>;

/// One column: a folder, the rows inside it, and the read that was asked for.
///
/// Always held behind an `Rc`. The widgets are shared anyway, and the two
/// `RefCell`s below have to be the ones the window writes to: a pane handed out
/// by value would cache its listing in a copy that nobody ever reads again.
struct Pane {
    location: Location,
    scroll: gtk::ScrolledWindow,
    list: gtk::ListView,
    store: gio::ListStore,
    selection: gtk::SingleSelection,
    /// The listing as read, before re-sorting. Kept so that changing the sort
    /// order costs no disk access.
    entries: RefCell<Vec<Entry>>,
    /// The folder whose listing this pane is still waiting for, or `None` once
    /// one has been shown. A second delivery for the same folder is dropped,
    /// which is what stops a stale read from overwriting a newer one.
    waiting: RefCell<Option<Location>>,
}

/// The widget's state, behind an `Rc` because every signal handler needs it and
/// a handler cannot borrow the widget it is attached to.
struct Inner {
    scroll: gtk::ScrolledWindow,
    track: gtk::Box,
    panes: RefCell<Vec<Rc<Pane>>>,
    readers: Arc<Readers>,
    key: RefCell<SortKey>,
    show_hidden: Cell<bool>,
    /// Filled in once the window's listing queue exists.
    handle: RefCell<Option<Handle>>,
    /// Which pane the keyboard is in, so a new column can be scrolled into view
    /// and so the arrow keys know where they were pressed from.
    active: Cell<usize>,
    on_open: RefCell<Option<OpenHandler>>,
    on_up: RefCell<Option<UpHandler>>,
}

/// The columns view. Cheap to hand out; the state is shared with the handlers.
pub struct Columns {
    inner: Rc<Inner>,
}

impl Columns {
    #[must_use]
    pub fn new(readers: Arc<Readers>, key: SortKey, show_hidden: bool) -> Self {
        let scroll = gtk::ScrolledWindow::new();
        scroll.set_hscrollbar_policy(gtk::PolicyType::Automatic);
        scroll.set_vscrollbar_policy(gtk::PolicyType::Never);
        scroll.set_overlay_scrolling(false);
        scroll.add_css_class("ufm-columns");

        let track = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        scroll.set_child(Some(&track));

        let inner = Rc::new(Inner {
            scroll,
            track,
            panes: RefCell::new(Vec::new()),
            readers,
            key: RefCell::new(key),
            show_hidden: Cell::new(show_hidden),
            handle: RefCell::new(None),
            active: Cell::new(0),
            on_open: RefCell::new(None),
            on_up: RefCell::new(None),
        });
        install_keys(&Rc::clone(&inner));
        Self { inner }
    }

    /// The widget to put in the window's content area.
    #[must_use]
    pub fn widget(&self) -> &gtk::ScrolledWindow {
        &self.inner.scroll
    }

    /// The queue listings are delivered through. Until this is set no pane can
    /// be read, which is the state during construction.
    pub fn set_handle(&self, handle: Handle) {
        *self.inner.handle.borrow_mut() = Some(handle);
    }

    /// React to the user opening a row, navigable or not. The window decides
    /// what that means, because the window owns the history.
    pub fn on_open<F: Fn(&Entry) + 'static>(&self, f: F) {
        *self.inner.on_open.borrow_mut() = Some(Rc::new(f));
    }

    /// React to the user asking for the parent folder with the keyboard.
    pub fn on_up<F: Fn() + 'static>(&self, f: F) {
        *self.inner.on_up.borrow_mut() = Some(Rc::new(f));
    }

    /// Show the chain that ends at `target`, reading every folder that is not
    /// already on screen. The listing for `target` itself is expected from the
    /// window, which is reading it for the other modes anyway.
    pub fn show_chain(&self, target: &Location) {
        let chain = chain(target);
        let mut left = std::mem::take(&mut *self.inner.panes.borrow_mut());
        let mut next: Vec<Rc<Pane>> = Vec::with_capacity(chain.len());
        for (depth, location) in chain.iter().enumerate() {
            let leaf = depth + 1 == chain.len();
            match position_of(&left, location) {
                Some(at) => next.push(left.remove(at)),
                None => next.push(build(&self.inner, location, !leaf)),
            }
        }
        // Whatever is left over is a pane no longer on the path. Its read may
        // still be running; the event simply finds no pane and is dropped.
        rebuild_track(&self.inner, &next);
        self.inner.active.set(next.len().saturating_sub(1));
        self.inner.panes.replace(next);
        self.reveal_active();
    }

    /// Re-read every pane but the deepest, including the ones already on
    /// screen. Bound to F5: a cached column that is quietly wrong is worse than
    /// a slow one. The leaf is left alone because the window re-reads the folder
    /// in its title bar anyway, and two reads of one directory would race.
    pub fn reload(&self) {
        let panes = self.inner.panes.borrow();
        for pane in panes.iter().take(panes.len().saturating_sub(1)) {
            *pane.waiting.borrow_mut() = Some(pane.location.clone());
            start(&self.inner, pane);
        }
    }

    /// Re-order the rows already on screen, from the cached listing. The cache
    /// is rewritten too: a pane that painted a new order and kept the old one
    /// would jump back the next time its rows were replaced, which is what
    /// narrowing a folder with the search box does.
    pub fn resort(&self, key: SortKey) {
        *self.inner.key.borrow_mut() = key;
        for pane in self.inner.panes.borrow().iter() {
            let mut entries = pane.entries.borrow_mut();
            sort_entries(&mut entries, key);
            paint(pane, &entries);
        }
    }

    /// Show or hide dotfiles. Every cached listing was read under the old rule,
    /// so the whole chain is dropped rather than re-read here: the window
    /// navigates straight after this, which rebuilds the chain and reads it.
    pub fn set_show_hidden(&self, show: bool) {
        if self.inner.show_hidden.replace(show) != show {
            self.forget();
        }
    }

    /// Drop every pane and the listing cached in it. The next
    /// [`Columns::show_chain`] walks the whole path again.
    pub fn forget(&self) {
        rebuild_track(&self.inner, &[]);
        self.inner.panes.borrow_mut().clear();
    }

    /// Put one delivered listing in the pane that asked for it. Events for
    /// folders that are not on screen are ignored rather than queued: a pane
    /// the user has scrolled past will ask again if it becomes relevant.
    pub fn apply(&self, event: &Event) {
        match event {
            // Columns paint a folder in one go. A partial listing would show a
            // column half full and then jump when the read finished.
            Event::Batch { .. } => {}
            Event::Finished {
                location, entries, ..
            } => {
                let Some(pane) = self.pane_for(location) else {
                    return;
                };
                if !pane
                    .waiting
                    .borrow()
                    .as_ref()
                    .is_some_and(|w| w == location)
                {
                    return;
                }
                pane.waiting.borrow_mut().take();
                // `clone_from` rather than an assignment: the previous listing is
                // usually a few hundred entries of the same shape, and reusing
                // its allocations is the difference between one allocation and
                // one per row on a folder the user opens twice.
                pane.entries.borrow_mut().clone_from(entries);
                paint(&pane, entries);
                // Back to the top of a column that was replaced. Guarded,
                // because `scroll_to` asserts on a position in an empty model
                // and an empty folder is a folder that exists.
                if !entries.is_empty() {
                    pane.list.scroll_to(0, gtk::ListScrollFlags::NONE, None);
                }
            }
            Event::Failed { location, message } => {
                if let Some(pane) = self.pane_for(location) {
                    pane.waiting.borrow_mut().take();
                    pane.entries.borrow_mut().clear();
                    paint(&pane, &[]);
                    pane.scroll.set_tooltip_text(Some(message));
                }
            }
        }
    }

    /// Replace one pane's rows without touching its cached listing, used when
    /// the window narrows the current folder with the search box.
    pub fn set_filter(&self, location: &Location, entries: &[Entry]) {
        if let Some(pane) = self.pane_for(location) {
            paint(&pane, entries);
        }
    }

    /// The folders currently shown, outermost first.
    #[must_use]
    pub fn pane_locations(&self) -> Vec<Location> {
        self.inner
            .panes
            .borrow()
            .iter()
            .map(|pane| pane.location.clone())
            .collect()
    }

    /// How many rows one pane is showing.
    #[must_use]
    pub fn pane_len(&self, index: usize) -> usize {
        self.inner
            .panes
            .borrow()
            .get(index)
            .map_or(0, |pane| usize::try_from(pane.store.n_items()).unwrap_or(0))
    }

    #[must_use]
    pub fn pane_widgets(&self) -> Vec<gtk::ScrolledWindow> {
        self.inner
            .panes
            .borrow()
            .iter()
            .map(|pane| pane.scroll.clone())
            .collect()
    }

    fn pane_for(&self, location: &Location) -> Option<Rc<Pane>> {
        let panes = self.inner.panes.borrow();
        position_of(&panes, location).map(|at| Rc::clone(&panes[at]))
    }

    /// Scroll the active pane fully into view. A new column appears off the
    /// right edge otherwise, and the user has to notice it exists at all.
    fn reveal_active(&self) {
        let panes = self.inner.panes.borrow();
        let Some(pane) = panes.get(self.inner.active.get()) else {
            return;
        };
        let Some(bounds) = self.inner.scroll.compute_bounds(&pane.scroll) else {
            return;
        };
        let adjustment = self.inner.scroll.hadjustment();
        let x = f64::from(bounds.x());
        let right = x + f64::from(bounds.width());
        if x < 0.0 {
            adjustment.set_value(adjustment.value() + x);
        } else if right > adjustment.page_size() {
            adjustment.set_value(adjustment.value() + right - adjustment.page_size());
        }
    }
}

/// The panes to show for `target`, outermost first, capped at [`MAX_PANES`].
fn chain(target: &Location) -> Vec<Location> {
    let mut out = vec![target.clone()];
    let mut cursor = target.clone();
    while let Some(parent) = cursor.parent() {
        if parent.is_virtual() || out.contains(&parent) {
            break;
        }
        out.push(parent.clone());
        cursor = parent;
    }
    out.reverse();
    let skip = out.len().saturating_sub(MAX_PANES);
    out.split_off(skip)
}

/// A new pane for one folder. `read` says whether the pane fills itself; the
/// leaf is left to the window, which is reading that folder already.
fn build(inner: &Rc<Inner>, location: &Location, read: bool) -> Rc<Pane> {
    let store = gio::ListStore::new::<Row>();
    let selection = gtk::SingleSelection::new(Some(store.clone()));
    selection.set_can_unselect(false);
    selection.set_autoselect(true);
    let list = gtk::ListView::new(
        Some(selection.clone()),
        Some(grid_factory(Rc::new(Cell::new(GRID_ICON)))),
    );
    list.set_show_separators(false);
    list.set_single_click_activate(false);

    let scroll = gtk::ScrolledWindow::new();
    scroll.set_child(Some(&list));
    scroll.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
    scroll.set_size_request(PANE_WIDTH, -1);
    scroll.set_vexpand(true);
    scroll.add_css_class("ufm-pane");
    scroll.set_tooltip_text(Some(&location.display()));

    let pane = Rc::new(Pane {
        location: location.clone(),
        scroll,
        list,
        store,
        selection,
        entries: RefCell::new(Vec::new()),
        waiting: RefCell::new(Some(location.clone())),
    });
    wire(&pane, inner);
    if read {
        start(inner, &pane);
    }
    pane
}

/// Ask for a listing, unless the queue is not there yet.
///
/// [`read_all`] rather than the streaming path: a column is one folder, it is
/// painted in one go anyway, and the chunked path exists to make a folder of
/// twenty thousand rows feel alive. A read that outlives its pane is not
/// cancelled — `read_all` makes its own flag — it is dropped in
/// [`Columns::apply`] for want of a pane to paint, which is cheaper than
/// threading a second cancellation flag through here as well.
fn start(inner: &Rc<Inner>, pane: &Pane) {
    let Some(handle) = inner.handle.borrow().clone() else {
        return;
    };
    let readers = Arc::clone(&inner.readers);
    let location = pane.location.clone();
    let key = *inner.key.borrow();
    let show_hidden = inner.show_hidden.get();
    let spawned = thread::Builder::new()
        .stack_size(256 * 1024)
        .spawn(move || {
            let reader = readers.for_location(&location);
            match read_all(reader, &location, key, show_hidden, None) {
                Ok(entries) => handle.send(Event::Finished {
                    location,
                    entries,
                    hints: None,
                }),
                Err(error) => handle.send(Event::Failed {
                    location,
                    message: error.to_string(),
                }),
            }
        });
    if spawned.is_err() {
        // Out of threads. The pane says so rather than staying empty quietly.
        pane.waiting.borrow_mut().take();
        pane.scroll.set_tooltip_text(Some("Cannot start a reader."));
    }
}

/// Replace a pane's rows. The store is spliced whole rather than patched, which
/// is one model change for GTK to react to instead of one per row.
fn paint(pane: &Pane, entries: &[Entry]) {
    let rows = Row::rows(entries.to_vec());
    pane.store.splice(0, pane.store.n_items(), &rows);
}

fn position_of(panes: &[Rc<Pane>], location: &Location) -> Option<usize> {
    panes.iter().position(|pane| pane.location == *location)
}

/// Put the panes back in the track in chain order. Detaching everything and
/// re-appending is a handful of widget moves, and `GtkBox` has no API for
/// moving a child to a position, so there is nothing cheaper that is correct.
fn rebuild_track(inner: &Rc<Inner>, next: &[Rc<Pane>]) {
    while let Some(child) = inner.track.first_child() {
        inner.track.remove(&child);
    }
    for pane in next {
        inner.track.append(&pane.scroll);
    }
}

/// Where a click or Enter goes: the row's entry, handed to the window. The
/// handler is cloned out first, because opening a folder rebuilds the chain and
/// must not run while the list of handlers is borrowed.
fn open(inner: &Rc<Inner>, store: &gio::ListStore, position: u32) {
    let Some(entry) = entry_at(store, position) else {
        return;
    };
    let handler = inner.on_open.borrow().clone();
    if let Some(handler) = handler {
        handler(&entry);
    }
}

fn fire_up(inner: &Rc<Inner>) {
    let handler = inner.on_up.borrow().clone();
    if let Some(handler) = handler {
        handler();
    }
}

/// Focusing a pane makes it the keyboard's, so a column appearing on the right
/// does not steal the arrow keys from a pane the user is reading.
fn wire(pane: &Pane, inner: &Rc<Inner>) {
    let store = pane.store.clone();
    let handlers = Rc::clone(inner);
    pane.list.connect_activate(move |_list, position| {
        open(&handlers, &store, position);
    });

    let focus = gtk::EventControllerFocus::new();
    let tracked = Rc::clone(inner);
    let location = pane.location.clone();
    focus.connect_enter(move |_focus| {
        tracked.active.set(
            tracked
                .panes
                .borrow()
                .iter()
                .position(|pane| pane.location == location)
                .unwrap_or(0),
        );
    });
    pane.list.add_controller(focus);
}

/// The keyboard. Captured before the focused list sees it, because the arrows
/// mean "move between columns" here and "move between rows" inside a pane, and
/// only the outer widget knows which of the two was pressed.
fn install_keys(inner: &Rc<Inner>) {
    let keys = gtk::EventControllerKey::new();
    keys.set_propagation_phase(gtk::PropagationPhase::Capture);
    let tracked = Rc::clone(inner);
    keys.connect_key_pressed(move |_controller, key, _code, _modifiers| {
        let active = tracked.panes.borrow().get(tracked.active.get()).cloned();
        let Some(pane) = active else {
            return Propagation::Proceed;
        };
        let selected = pane.selection.selected();
        match key {
            // Right means "go deeper" only for a folder; on a file it has to
            // stay free, or a file could never be passed over with the keyboard.
            gtk::gdk::Key::Right if navigable_at(&pane.store, selected) => {
                open(&tracked, &pane.store, selected);
                Propagation::Stop
            }
            gtk::gdk::Key::Return if entry_at(&pane.store, selected).is_some() => {
                open(&tracked, &pane.store, selected);
                Propagation::Stop
            }
            gtk::gdk::Key::Left | gtk::gdk::Key::BackSpace => {
                fire_up(&tracked);
                Propagation::Stop
            }
            gtk::gdk::Key::Down | gtk::gdk::Key::Up => {
                if pane.store.n_items() == 0 {
                    return Propagation::Proceed;
                }
                if selected == gtk::INVALID_LIST_POSITION {
                    pane.selection.set_selected(0);
                }
                // The list moves within itself; this only made sure it has the
                // focus and has a row to move from.
                let _ = pane.list.grab_focus();
                Propagation::Proceed
            }
            _ => Propagation::Proceed,
        }
    });
    inner.scroll.add_controller(keys);
}

fn entry_at(store: &gio::ListStore, position: u32) -> Option<Entry> {
    if position == gtk::INVALID_LIST_POSITION {
        return None;
    }
    store
        .item(position)
        .and_then(|object| object.downcast::<Row>().ok())
        .map(|row| row.entry())
}

fn navigable_at(store: &gio::ListStore, position: u32) -> bool {
    entry_at(store, position).is_some_and(|entry| entry.navigable())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::tk;
    use std::path::PathBuf;
    use ufm_core::entry::{EntryFlags, EntryKind};

    fn location(name: &str) -> Location {
        Location::file(PathBuf::from(name))
    }

    fn entry(name: &str, kind: EntryKind) -> Entry {
        Entry {
            location: location(&format!("/home/u/{name}")),
            name: name.to_string(),
            kind,
            flags: EntryFlags::empty(),
            size: 4,
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

    fn file(name: &str) -> Entry {
        entry(name, EntryKind::File)
    }

    fn columns() -> Columns {
        Columns::new(Arc::new(Readers::local_only()), SortKey::default(), false)
    }

    fn done(path: &str, entries: Vec<Entry>) -> Event {
        Event::Finished {
            location: location(path),
            entries,
            hints: None,
        }
    }

    fn pane_at(index: usize, columns: &Columns) -> Rc<Pane> {
        columns
            .inner
            .panes
            .borrow()
            .get(index)
            .cloned()
            .expect("the pane")
    }

    #[test]
    fn a_chain_has_one_pane_per_component() {
        tk::ui(|| {
            let columns = columns();
            columns.show_chain(&location("/home/u/projects"));
            assert_eq!(
                columns.pane_locations(),
                vec![
                    location("/"),
                    location("/home"),
                    location("/home/u"),
                    location("/home/u/projects"),
                ]
            );
        });
    }

    #[test]
    fn a_deep_path_keeps_the_leaf_and_drops_the_root() {
        tk::ui(|| {
            let columns = columns();
            columns.show_chain(&location("/a/b/c/d/e/f/g"));
            let panes = columns.pane_locations();
            assert_eq!(panes.len(), MAX_PANES);
            assert_eq!(panes.last(), Some(&location("/a/b/c/d/e/f/g")));
            assert!(!panes.contains(&location("/")));
        });
    }

    #[test]
    fn a_virtual_location_is_one_pane() {
        tk::ui(|| {
            let columns = columns();
            columns.show_chain(&Location::computer());
            assert_eq!(columns.pane_locations(), vec![Location::computer()]);
        });
    }

    #[test]
    fn going_deeper_reuses_the_panes_already_shown() {
        tk::ui(|| {
            let columns = columns();
            columns.show_chain(&location("/home/u"));
            let before = columns.pane_widgets();
            columns.show_chain(&location("/home/u/projects"));
            let after = columns.pane_widgets();
            assert_eq!(after.len(), before.len() + 1);
            // The same widgets, not merely the same folders: a pane that was
            // rebuilt would have thrown its listing away.
            assert_eq!(after[..before.len()], before);
        });
    }

    #[test]
    fn going_up_drops_the_pane_we_came_from() {
        tk::ui(|| {
            let columns = columns();
            columns.show_chain(&location("/home/u/projects"));
            columns.show_chain(&location("/home/u"));
            assert_eq!(
                columns.pane_locations(),
                vec![location("/"), location("/home"), location("/home/u")]
            );
        });
    }

    #[test]
    fn a_listing_lands_in_the_pane_that_asked_for_it() {
        tk::ui(|| {
            let columns = columns();
            columns.show_chain(&location("/home/u"));
            assert_eq!(columns.pane_len(2), 0);
            columns.apply(&done("/home/u", vec![file("a"), file("b")]));
            assert_eq!(columns.pane_len(2), 2);
            // A second delivery for the same folder is a stale read, and a
            // stale read must not replace what is on screen.
            columns.apply(&done("/home/u", Vec::new()));
            assert_eq!(columns.pane_len(2), 2);
        });
    }

    #[test]
    fn a_partial_listing_is_ignored() {
        tk::ui(|| {
            let columns = columns();
            columns.show_chain(&location("/home/u"));
            columns.apply(&Event::Batch {
                location: location("/home/u"),
                entries: vec![file("a")],
            });
            assert_eq!(columns.pane_len(2), 0);
        });
    }

    #[test]
    fn a_listing_for_a_folder_not_on_screen_changes_nothing() {
        tk::ui(|| {
            let columns = columns();
            columns.show_chain(&location("/home/u"));
            columns.apply(&done("/elsewhere", vec![file("a")]));
            columns.apply(&Event::Failed {
                location: location("/elsewhere"),
                message: "gone".to_string(),
            });
            for index in 0..columns.pane_locations().len() {
                assert_eq!(columns.pane_len(index), 0);
            }
        });
    }

    #[test]
    fn re_sorting_uses_the_cached_listing() {
        tk::ui(|| {
            let columns = columns();
            columns.show_chain(&location("/home/u"));
            columns.apply(&done("/home/u", vec![file("a"), file("b")]));
            columns.resort(SortKey {
                descending: true,
                ..SortKey::default()
            });
            // Reordered in place rather than emptied waiting for a re-read.
            assert_eq!(columns.pane_len(2), 2);
            let names: Vec<String> = pane_at(2, &columns)
                .entries
                .borrow()
                .iter()
                .map(|entry| entry.name.clone())
                .collect();
            assert_eq!(names, vec!["b", "a"]);
        });
    }

    #[test]
    fn the_folder_filter_only_touches_the_pane_it_names() {
        tk::ui(|| {
            let columns = columns();
            columns.show_chain(&location("/home/u"));
            for index in 0..columns.pane_locations().len() {
                *pane_at(index, &columns).entries.borrow_mut() = vec![file("a"), file("b")];
            }
            columns.set_filter(&location("/home/u"), &[file("a")]);
            assert_eq!(columns.pane_len(2), 1);
            assert_eq!(columns.pane_len(1), 0);
            // Filtering painted one row but left the listing it was applied to,
            // so clearing the filter puts both rows back without a re-read.
            assert_eq!(pane_at(2, &columns).entries.borrow().len(), 2);
        });
    }

    #[test]
    fn a_failure_names_itself_on_the_pane() {
        tk::ui(|| {
            let columns = columns();
            columns.show_chain(&location("/home/u"));
            columns.apply(&Event::Failed {
                location: location("/home/u"),
                message: "Permission denied".to_string(),
            });
            let widget = columns.pane_widgets().pop().expect("the leaf pane");
            assert_eq!(widget.tooltip_text().as_deref(), Some("Permission denied"));
        });
    }

    #[test]
    fn opening_a_row_reports_the_entry_it_names() {
        tk::ui(|| {
            let columns = columns();
            let seen = Rc::new(RefCell::new(Vec::new()));
            let tracker = Rc::clone(&seen);
            columns.on_open(move |entry| tracker.borrow_mut().push(entry.name.clone()));
            let store = gio::ListStore::new::<Row>();
            store.extend_from_slice(&Row::rows(vec![file("notes")]));
            open(&columns.inner, &store, 0);
            assert_eq!(*seen.borrow(), vec!["notes".to_string()]);
            // A position past the end is not an error, just nothing pressed.
            open(&columns.inner, &store, 9);
            assert_eq!(seen.borrow().len(), 1);
        });
    }

    #[test]
    fn only_a_folder_opens_to_the_right() {
        tk::ui(|| {
            let _columns = columns();
            let store = gio::ListStore::new::<Row>();
            store.extend_from_slice(&Row::rows(vec![
                file("notes"),
                entry("src", EntryKind::Directory),
            ]));
            assert!(!navigable_at(&store, 0));
            assert!(navigable_at(&store, 1));
            assert!(!navigable_at(&store, gtk::INVALID_LIST_POSITION));
        });
    }

    #[test]
    fn without_a_handle_no_pane_is_read_but_the_chain_still_shows() {
        tk::ui(|| {
            let columns = columns();
            columns.show_chain(&location("/home/u/projects"));
            assert_eq!(columns.pane_locations().len(), 4);
            assert!(columns.inner.handle.borrow().is_none());
        });
    }
}
