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
//! other modes: a chain is up to [`MAX_PANES`] folders deep, and a pane that threw
//! its listing away would walk its directory again on every arrow key. The
//! reader underneath caches listings too, so a pane that *has* been rebuilt is
//! repainted from memory rather than off the disk — see [`ufm_vfs::DirCache`].
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
use super::popup::{self, ContextHandler, Listeners};
use super::row::Row;
use super::thumb::Pipeline;
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

/// A callback into the window: the row the keyboard is on has changed, so a
/// command whose availability depends on what that row *is* has to be asked
/// again. The reason a window needs to hear about this at all is
/// [`super::view::View::on_selection`].
type SelectionHandler = Rc<dyn Fn()>;

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
    /// Every pane draws its icons at the same size, so the cell the factories
    /// read is shared rather than built per pane: a size that only one column
    /// knew about could not be changed by anything that changes the others.
    icon_size: Rc<Cell<i32>>,
    /// Previews for the rows of every pane. Shared with the view, which is the
    /// same rows in a different presentation.
    thumbs: Pipeline,
    /// Filled in once the window's listing queue exists.
    handle: RefCell<Option<Handle>>,
    /// Which pane the keyboard is in, so a new column can be scrolled into view
    /// and so the arrow keys know where they were pressed from.
    active: Cell<usize>,
    on_open: RefCell<Option<OpenHandler>>,
    on_up: RefCell<Option<UpHandler>>,
    on_selection: RefCell<Option<SelectionHandler>>,
    /// Right-clicks, reported to whoever shows the menu. Shared by every pane so
    /// that one subscription covers the whole widget, and so the columns and the
    /// grid answer a right-click with the same menu.
    contexts: Listeners,
}

/// The columns view. Cheap to hand out; the state is shared with the handlers.
pub struct Columns {
    inner: Rc<Inner>,
}

impl Columns {
    #[must_use]
    pub fn new(readers: Arc<Readers>, key: SortKey, show_hidden: bool, thumbs: Pipeline) -> Self {
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
            icon_size: Rc::new(Cell::new(GRID_ICON)),
            thumbs,
            handle: RefCell::new(None),
            active: Cell::new(0),
            on_open: RefCell::new(None),
            on_up: RefCell::new(None),
            on_selection: RefCell::new(None),
            contexts: Listeners::new(),
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

    /// React to the row under the column keyboard changing, or to the panes
    /// themselves being rebuilt.
    pub fn on_selection<F: Fn() + 'static>(&self, f: F) {
        *self.inner.on_selection.borrow_mut() = Some(Rc::new(f));
    }

    /// React to a right-click, on a row or on the empty part of a pane. The
    /// target says which row and which folder; see [`super::popup`] for how a row
    /// is identified, and for what [`popup::Target::in_current`] means here.
    pub fn on_context<F>(&self, f: F)
    where
        F: Fn(&gtk::Widget, popup::Target, f64, f64) + 'static,
    {
        self.inner.contexts.push(f);
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
        // A new chain means a new row under the keyboard even when its position
        // number did not change, so the window has to be told rather than left
        // to a signal that may not come.
        fire_selection(&self.inner);
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

    /// Re-read one pane, found by the folder it shows.
    ///
    /// For a change reported by the watcher in a parent column: the folders next
    /// to it on screen are not implicated, and re-reading a chain of five small
    /// folders because one of them moved is five directory walks where one would
    /// do. The deepest pane is fair game here, unlike in [`Columns::reload`],
    /// because the caller is the one that decided this folder changed and the
    /// window's own read is not in flight for it.
    pub fn reload_one(&self, location: &Location) {
        let Some(pane) = self.pane_for(location) else {
            return;
        };
        *pane.waiting.borrow_mut() = Some(pane.location.clone());
        start(&self.inner, &pane);
    }

    /// Mark the deepest pane as waiting for the folder in the title bar, so that
    /// the next listing the window reads paints it as well.
    ///
    /// For a change made outside this program in the folder on screen: the window
    /// re-reads that folder for its own rows, and without this the pane holding
    /// the same rows keeps painting the listing that just went stale.
    /// [`Columns::reload`] skips this pane for the opposite reason — on a reload
    /// the window is re-reading the folder anyway, and starting a second read of
    /// one directory would race the first. This asks for no read at all.
    pub fn expect_leaf(&self) {
        let panes = self.inner.panes.borrow();
        if let Some(pane) = panes.last() {
            *pane.waiting.borrow_mut() = Some(pane.location.clone());
        }
    }

    /// Paint the deepest pane from a listing the window already has, and
    /// remember it as that pane's own.
    ///
    /// For arriving at columns mode from grid or list. [`Columns::show_chain`]
    /// leaves the leaf pane to the window on the grounds that the window is
    /// reading that folder anyway — true while navigating, and false when the
    /// user merely changes how the folder they are already looking at is drawn.
    /// Without this the column for the folder in the title bar appears empty
    /// until something else happens to re-read it, which reads as a broken view
    /// rather than as a view that is waiting for a read nobody was going to make.
    ///
    /// The pane is left waiting, as [`Columns::expect_leaf`] leaves it: these
    /// rows are provisional in the sense that a read started before the mode
    /// changed still outranks them, and it is the fresh listing that should
    /// replace them rather than nothing. The listing goes into the pane's cache
    /// as well, so that re-sorting or narrowing the column afterwards does not
    /// lose the rows.
    pub fn fill_leaf(&self, location: &Location, entries: &[Entry]) {
        let Some(pane) = self.pane_for(location) else {
            return;
        };
        *pane.entries.borrow_mut() = entries.to_vec();
        paint(&pane, entries);
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
        // The selected position survives a re-sort, so the row under the keyboard
        // is usually a different file afterwards.
        fire_selection(&self.inner);
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

    /// Ask every pane to bind the rows it already holds again, without reading a
    /// folder a second time. The columns mode keeps its own listings, so a change
    /// to something a factory reads has to reach them here rather than through
    /// the view.
    pub fn rebind(&self) {
        for pane in self.inner.panes.borrow().iter() {
            let rows: Vec<Row> = pane.store.iter::<Row>().filter_map(Result::ok).collect();
            pane.store.splice(0, pane.store.n_items(), &rows);
        }
    }

    /// Put one delivered listing in the pane that asked for it. Events for
    /// folders that are not on screen are ignored rather than queued: a pane
    /// the user has scrolled past will ask again if it becomes relevant.
    pub fn apply(&self, event: &Event) {
        match event {
            // Columns paint a folder in one go, so a partial listing would show a
            // column half full and then jump when the read finished. A share that
            // finished connecting is the window's call — it owns the history and
            // the current location — so the columns repaint when it navigates. A
            // file operation is the same: the window knows which pane it touched
            // and either patches it or re-reads it.
            // A picture is the same kind of no-op: a pane asks for one through
            // the shared pipeline, which the window answers, and the panes have
            // nothing to match a token against. A folder's total is the same
            // again — it belongs to a dialog, not to a pane. So does a member
            // written out of an archive, which belongs to the chooser that asked.
            // So does a text head, which belongs to the preview popover and is
            // looked up there by its token. So does a progress report and a
            // job's ending, both of which belong to the strip rather than to a
            // pane — and a pane that re-read itself on every report would spend
            // a copy re-reading the folder it is copying into.
            Event::Batch { .. }
            | Event::Mounted { .. }
            | Event::Unmounted { .. }
            | Event::Operation { .. }
            | Event::Thumb { .. }
            | Event::Sized { .. }
            | Event::Materialised { .. }
            | Event::Preview { .. }
            | Event::Progress { .. }
            | Event::JobFinished { .. } => {}
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
                fire_selection(&self.inner);
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
                    fire_selection(&self.inner);
                }
            }
        }
    }

    /// Replace one pane's rows without touching its cached listing, used when
    /// the window narrows the current folder with the search box.
    pub fn set_filter(&self, location: &Location, entries: &[Entry]) {
        if let Some(pane) = self.pane_for(location) {
            paint(&pane, entries);
            fire_selection(&self.inner);
        }
    }

    /// The row the column keyboard is on, as one entry. The window asks for this
    /// when a file operation is invoked with the columns on screen: the grid
    /// selection is a different widget's state, and acting on rows the user
    /// cannot see would be worse than acting on none.
    #[must_use]
    pub fn selected(&self) -> Vec<Entry> {
        let panes = self.inner.panes.borrow();
        let Some(pane) = panes.get(self.inner.active.get()) else {
            return Vec::new();
        };
        let at = usize::try_from(pane.selection.selected()).unwrap_or(usize::MAX);
        // Read from the store rather than from the cached listing: the store is
        // what is painted, and while the search box narrows a folder the two
        // disagree about which row number means what.
        pane.store
            .item(u32::try_from(at).unwrap_or(u32::MAX))
            .and_then(|object| object.downcast::<Row>().ok())
            .map(|row| vec![row.entry()])
            .unwrap_or_default()
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

/// The right-click handler for one pane.
///
/// Two things happen before the press is passed on. The row is selected first: a
/// menu over a row that is not the row the keyboard is on would otherwise act on
/// a different file than the one under the pointer, and the window builds its
/// rows from the selection rather than from the position. Then the press is
/// reported with the answer to "is this the folder the window is in?", which for a
/// column is whether this pane is the deepest one — the pane holding the folder in
/// the title bar. That is asked at press time rather than at build time, because
/// the chain is rebuilt under a pane that stays on screen.
fn pane_press(
    inner: Rc<Inner>,
    location: Location,
    store: &gio::ListStore,
    selection: &gtk::SingleSelection,
) -> ContextHandler {
    let on_press = inner.contexts.handler();
    let store = store.clone();
    let selection = selection.clone();
    Rc::new(move |widget, mut target, x, y| {
        let in_current = inner
            .panes
            .borrow()
            .last()
            .is_some_and(|pane| pane.location == location);
        target = match target {
            popup::Target::Row { position, .. } => {
                // A recycled cell can report a position past the end of a
                // listing that was replaced since it was bound.
                if entry_at(&store, position).is_some() {
                    selection.set_selected(position);
                }
                popup::Target::Row {
                    position,
                    in_current,
                }
            }
            popup::Target::Background { .. } => popup::Target::Background { in_current },
        };
        on_press(widget, target, x, y);
    })
}

/// A new pane for one folder. `read` says whether the pane fills itself; the
/// leaf is left to the window, which is reading that folder already.
fn build(inner: &Rc<Inner>, location: &Location, read: bool) -> Rc<Pane> {
    let store = gio::ListStore::new::<Row>();
    let selection = gtk::SingleSelection::new(Some(store.clone()));
    selection.set_can_unselect(false);
    selection.set_autoselect(true);
    let on_press = pane_press(Rc::clone(inner), location.clone(), &store, &selection);
    let list = gtk::ListView::new(
        Some(selection.clone()),
        Some(grid_factory(
            Rc::clone(&inner.icon_size),
            inner.thumbs.clone(),
            &on_press,
        )),
    );
    list.set_show_separators(false);
    list.set_single_click_activate(false);
    list.add_controller(popup::background(&on_press, false));

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

/// Run the selection handler. Cloned out first, like the other two: a handler
/// that greys a button out runs in the middle of a repaint, and one that
/// replaced the listing would do it through a borrowed handler list.
fn fire_selection(inner: &Rc<Inner>) {
    let handler = inner.on_selection.borrow().clone();
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
        // Moving the keyboard into a different pane changes the row a command
        // would act on without changing any selection.
        fire_selection(&tracked);
    });
    pane.list.add_controller(focus);

    // The row the keyboard is on, including the first one a freshly painted pane
    // autoselects. `notify::selected` rather than a selection-model signal:
    // `GtkSingleSelection` has no `selection-changed`.
    let tracked = Rc::clone(inner);
    pane.selection
        .connect_selected_notify(move |_selection| fire_selection(&tracked));
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
        Columns::new(
            Arc::new(Readers::local_only()),
            SortKey::default(),
            false,
            Pipeline::new(),
        )
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
    fn the_deepest_column_can_be_filled_without_a_read() {
        tk::ui(|| {
            let columns = columns();
            columns.show_chain(&location("/home/u"));
            // Arriving at columns mode from another view: the window is not
            // reading the folder, so no event would ever arrive for the leaf.
            columns.fill_leaf(&location("/home/u"), &[file("a"), file("b"), file("c")]);
            assert_eq!(columns.pane_len(2), 3);
            // The rows are painted but the pane is still waiting, so a read
            // that was already running when the mode changed still gets to
            // replace them. That answer is fresher than the rows on screen.
            columns.apply(&done("/home/u", vec![file("a"), file("b")]));
            assert_eq!(columns.pane_len(2), 2);
        });
    }

    #[test]
    fn filling_a_column_that_is_not_on_screen_changes_nothing() {
        tk::ui(|| {
            let columns = columns();
            columns.show_chain(&location("/home/u"));
            columns.fill_leaf(&location("/somewhere/else"), &[file("a")]);
            assert_eq!(columns.pane_len(2), 0);
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

    /// Every pane of a settled chain: each one has been given a listing, so none
    /// of them is waiting. Returns the panes' folders, which is what `done` needs.
    fn settle(columns: &Columns) -> Vec<Location> {
        let locations = columns.pane_locations();
        for location in &locations {
            columns.apply(&done(
                &location.as_path().expect("a folder").display().to_string(),
                vec![file("a"), file("b")],
            ));
        }
        locations
    }

    fn waiting(columns: &Columns, index: usize) -> bool {
        pane_at(index, columns).waiting.borrow().is_some()
    }

    #[test]
    fn the_leaf_waits_for_a_listing_the_window_is_reading() {
        tk::ui(|| {
            let columns = columns();
            columns.show_chain(&location("/home/u"));
            settle(&columns);
            assert!(!waiting(&columns, 2));
            // The window re-reads the folder in its title bar for its own rows; the
            // leaf holds those same rows and has to be told to expect them.
            columns.expect_leaf();
            assert!(waiting(&columns, 2));
            // Marking is not reading: the pane keeps painting what it had, so the
            // column does not blink empty while the window's read is on its way.
            assert_eq!(columns.pane_len(2), 2);
            columns.apply(&done("/home/u", vec![file("a")]));
            assert_eq!(columns.pane_len(2), 1);
            assert!(!waiting(&columns, 2));
        });
    }

    #[test]
    fn one_pane_can_be_re_read_without_the_others() {
        tk::ui(|| {
            let columns = columns();
            columns.show_chain(&location("/home/u"));
            settle(&columns);
            // A change reported for one parent column must not implicate its
            // neighbours: only the pane showing that folder is marked.
            columns.reload_one(&location("/home"));
            assert!(waiting(&columns, 1));
            assert!(!waiting(&columns, 0));
            assert!(!waiting(&columns, 2));
            // A folder that is not a pane here is not this chain's business, and
            // must not leave a pane waiting for a listing that will never come.
            columns.reload_one(&location("/tmp"));
            assert!(!waiting(&columns, 0));
            assert!(!waiting(&columns, 2));
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
