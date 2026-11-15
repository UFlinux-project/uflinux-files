//! The view modes, over one model.
//!
//! Grid and list are two widgets stacked in a [`gtk::Stack`] and pointed at the
//! same [`gio::ListStore`] of [`Row`]s, so switching modes is a stack transition
//! and not a re-read: a folder is listed once per navigation however often the
//! user presses <kbd>Ctrl</kbd>+1. Columns mode reuses this widget for each pane
//! (see [`super::columns`]).
//!
//! The store is always in display order. [`ufm_core::sort::sort_entries`] runs
//! once, on the thread that read the directory, which is cheaper than a live
//! `GtkSortListModel` over twenty thousand rows and keeps the model a plain
//! array. The list view is deliberately given no sorter, because a second order
//! on top of ours would disagree with the grid and with the column panes, and a
//! model position would stop being a view position. The headers are ours instead
//! (see [`View::install_headers`]), which is also where the sort arrow comes
//! from.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gtk::prelude::*;
use gtk::{gio, pango};

use ufm_core::config::ViewMode;
use ufm_core::entry::Entry;
use ufm_core::sort::{SortField, SortKey, sort_entries};

use super::popup::{self, ContextHandler, Listeners};
use super::row::{Row, format_items};
use super::thumb::Pipeline;

/// Icon size in grid and columns mode. Small enough that a 512 px pane fits
/// three icons, which is what keeps a Miller-column view usable.
pub const GRID_ICON: i32 = 64;
/// Icon size in list mode, where a row is one line tall.
pub const LIST_ICON: i32 = 16;

/// Which column of the list view a factory renders.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Column {
    Name,
    Size,
    Kind,
    Modified,
}

impl Column {
    const ALL: [Self; 4] = [Self::Name, Self::Size, Self::Kind, Self::Modified];

    fn title(self) -> &'static str {
        match self {
            Self::Name => "Name",
            Self::Size => "Size",
            Self::Kind => "Type",
            Self::Modified => "Modified",
        }
    }

    /// The only column that carries the icon, and the one that expands.
    fn is_name(self) -> bool {
        self == Self::Name
    }

    fn field(self) -> SortField {
        match self {
            Self::Name => SortField::Name,
            Self::Size => SortField::Size,
            Self::Kind => SortField::Kind,
            Self::Modified => SortField::Modified,
        }
    }
}

/// What the status bar says: how much is shown, and how much of it is picked.
struct Status {
    labels: RefCell<Vec<gtk::Label>>,
    shown: Cell<usize>,
    selected: Cell<usize>,
    /// A line to say instead of the count, for the presentation beside this one
    /// that has no rows of its own. See [`View::set_status`].
    said: RefCell<Option<String>>,
}

impl Status {
    fn new() -> Self {
        Self {
            labels: RefCell::new(Vec::new()),
            shown: Cell::new(0),
            selected: Cell::new(0),
            said: RefCell::new(None),
        }
    }

    fn report(&self) {
        let text = self.said.borrow().clone().unwrap_or_else(|| {
            let shown = self.shown.get();
            let selected = self.selected.get();
            if selected > 0 && selected != shown {
                format!("{selected} of {shown} selected")
            } else if shown == 0 {
                "Empty folder".to_string()
            } else {
                format_items(u64::try_from(shown).unwrap_or(u64::MAX))
            }
        });
        for label in self.labels.borrow().iter() {
            label.set_text(&text);
        }
    }
}

type Activators = Rc<RefCell<Vec<Rc<dyn Fn(&Row)>>>>;
type Headers = Rc<RefCell<Vec<(Column, gtk::Button)>>>;
type Selections = Rc<RefCell<Vec<Rc<dyn Fn()>>>>;

/// The widget that shows one folder, in whichever mode is active.
pub struct View {
    stack: gtk::Stack,
    store: gio::ListStore,
    selection: gtk::MultiSelection,
    grid: gtk::GridView,
    list: gtk::ColumnView,
    icon_size: Rc<Cell<i32>>,
    mode: Cell<ViewMode>,
    key: RefCell<SortKey>,
    activators: Activators,
    status: Rc<Status>,
    headers: Headers,
    selections: Selections,
    contexts: Listeners,
    thumbs: Pipeline,
}

impl View {
    #[must_use]
    pub fn new(key: SortKey, thumbs: Pipeline) -> Self {
        let store = gio::ListStore::new::<Row>();
        let selection = gtk::MultiSelection::new(Some(store.clone()));
        let icon_size = Rc::new(Cell::new(GRID_ICON));
        let contexts = Listeners::new();
        // A right-click on a row that is not the selected one selects it first, so
        // that the menu is built from the row under the pointer. A row already in a
        // multi-selection is left alone: collapsing three selected rows to the one
        // the pointer happened to be over would quietly change what Trash does.
        let on_press: ContextHandler = {
            let report = contexts.handler();
            let store = store.clone();
            let selection = selection.clone();
            Rc::new(move |widget, target, x, y| {
                if let popup::Target::Row { position, .. } = target
                    && position < store.n_items()
                    && !selection.is_selected(position)
                {
                    selection.select_item(position, true);
                }
                report(widget, target, x, y);
            })
        };

        let grid = gtk::GridView::new(
            Some(selection.clone()),
            Some(grid_factory(
                Rc::clone(&icon_size),
                thumbs.clone(),
                &on_press,
            )),
        );
        grid.set_single_click_activate(false);
        // The width decides the column count; GTK's "no ceiling" is its largest
        // number, and 0 is rejected outright.
        grid.set_max_columns(u32::MAX);
        grid.add_css_class("ufm-grid");

        let list = gtk::ColumnView::new(Some(selection.clone()));
        list.set_show_row_separators(true);
        list.set_show_column_separators(false);
        list.set_reorderable(false);
        list.add_css_class("ufm-list");
        for column in Column::ALL {
            let widget = gtk::ColumnViewColumn::new(
                Some(column.title()),
                Some(list_factory(
                    column,
                    Rc::clone(&icon_size),
                    thumbs.clone(),
                    &on_press,
                )),
            );
            widget.set_expand(column.is_name());
            widget.set_resizable(true);
            if !column.is_name() {
                widget.set_fixed_width(110);
            }
            list.append_column(&widget);
        }

        let stack = gtk::Stack::new();
        stack.add_named(&grid, Some("grid"));
        stack.add_named(&list, Some("list"));
        stack.set_hexpand(true);
        stack.set_vexpand(true);

        let view = Self {
            stack,
            store,
            selection,
            grid,
            list,
            icon_size,
            mode: Cell::new(ViewMode::Grid),
            key: RefCell::new(key),
            activators: Rc::new(RefCell::new(Vec::new())),
            status: Rc::new(Status::new()),
            headers: Rc::new(RefCell::new(Vec::new())),
            selections: Rc::new(RefCell::new(Vec::new())),
            contexts,
            thumbs,
        };
        // Right-click below the last row is a click on the folder, and both
        // presentations need it: a grid has empty space beside its columns as
        // well as below them. Either one is showing the folder the window is in,
        // which is what makes the folder-level rows safe to offer.
        view.grid.add_controller(popup::background(&on_press, true));
        view.list.add_controller(popup::background(&on_press, true));
        view.wire_activate();
        view.wire_selection();
        view
    }

    /// The widget to put in the window's content area.
    #[must_use]
    pub fn widget(&self) -> &gtk::Stack {
        &self.stack
    }

    /// React to the user opening a folder or activating a file.
    pub fn on_activate<F: Fn(&Row) + 'static>(&self, f: F) {
        self.activators.borrow_mut().push(Rc::new(f));
    }

    /// React to the selection changing, or to the rows it points into being
    /// replaced. The window uses this for the one command whose availability
    /// depends on what the selected row *is* rather than merely on there being
    /// one, which no amount of clipboard bookkeeping can predict.
    pub fn on_selection<F: Fn() + 'static>(&self, f: F) {
        self.selections.borrow_mut().push(Rc::new(f));
    }

    /// React to a right-click, on a row or on the folder behind the rows. See
    /// [`super::popup`] for how a row is identified.
    pub fn on_context<F>(&self, f: F)
    where
        F: Fn(&gtk::Widget, popup::Target, f64, f64) + 'static,
    {
        self.contexts.push(f);
    }

    /// Replace the whole listing. Called once per navigation, and again when a
    /// watch event says the folder changed. The entries arrive sorted; sorting
    /// here as well would be a second pass over a list that can be large.
    pub fn set_entries(&self, entries: Vec<Entry>) {
        let rows = Row::rows(entries);
        self.status.shown.set(rows.len());
        self.status.selected.set(0);
        // Rows the view is itself showing, so the count describes them again. An
        // override belongs to a presentation with no rows, and lasts exactly as
        // long as that presentation is on screen.
        *self.status.said.borrow_mut() = None;
        self.store.splice(0, self.store.n_items(), &rows);
        self.status.report();
        // Replacing the rows can change what the selection means without any
        // signal saying so: a splice that leaves a selected position in place
        // points it at a different row, or at no row at all.
        fire_selection(&self.selections);
    }

    /// Append one batch of a streaming read, so a large folder paints its first
    /// screenful before the read finishes.
    pub fn push_batch(&self, entries: &[Entry]) {
        let rows = Row::rows(entries.to_vec());
        self.status.shown.set(self.len() + rows.len());
        *self.status.said.borrow_mut() = None;
        self.store.extend_from_slice(&rows);
        self.status.report();
    }

    /// Say `line` in the status bar instead of a row count, until the view is
    /// given rows of its own.
    ///
    /// The My Computer cards are not rows, and a view holding six of them while
    /// the bar reads "Empty folder" is a bar that contradicts the window. Passing
    /// `None` asks for the count back, which is what any folder read does by
    /// itself through [`View::set_entries`].
    pub fn set_status(&self, line: Option<&str>) {
        *self.status.said.borrow_mut() = line.map(str::to_string);
        self.status.report();
    }

    /// Re-order the rows already on screen without touching the disk.
    pub fn resort(&self, key: SortKey) {
        *self.key.borrow_mut() = key;
        let mut entries: Vec<Entry> = self
            .store
            .iter::<Row>()
            .filter_map(Result::ok)
            .map(|row| row.entry())
            .collect();
        sort_entries(&mut entries, key);
        let rows = Row::rows(entries);
        self.store.splice(0, self.store.n_items(), &rows);
        self.paint_headers();
    }

    #[must_use]
    pub fn sort_key(&self) -> SortKey {
        *self.key.borrow()
    }

    /// Show one of the three modes. Columns mode is a different widget, owned by
    /// the window and built from views like this one; it shows the list
    /// presentation here so a view used as a pane is consistent with itself.
    pub fn set_mode(&self, mode: ViewMode) {
        self.mode.set(mode);
        match mode {
            ViewMode::Grid => self.stack.set_visible_child_name("grid"),
            ViewMode::List | ViewMode::Columns => self.stack.set_visible_child_name("list"),
        }
        let icon = if mode == ViewMode::Grid {
            GRID_ICON
        } else {
            LIST_ICON
        };
        self.set_icon_size(icon);
    }

    #[must_use]
    pub fn mode(&self) -> ViewMode {
        self.mode.get()
    }

    pub fn set_icon_size(&self, size: i32) {
        if self.icon_size.replace(size.clamp(16, 256)) == size.clamp(16, 256) {
            return;
        }
        self.rebind();
    }

    /// Ask the factories to bind the rows already in the model again, without
    /// reading the folder a second time. Used when something a factory reads has
    /// changed — a thumbnail setting, an icon size — and the rows themselves have
    /// not.
    pub fn rebind(&self) {
        // Splicing the same rows back in place is the only way to ask for a
        // re-bind while keeping the selection, which a position-based update
        // would have to rebuild by hand.
        let rows: Vec<Row> = self.store.iter::<Row>().filter_map(Result::ok).collect();
        self.store.splice(0, self.store.n_items(), &rows);
    }

    #[must_use]
    pub fn selected(&self) -> Vec<Entry> {
        self.selected_rows().iter().map(Row::entry).collect()
    }

    #[must_use]
    pub fn selected_rows(&self) -> Vec<Row> {
        (0..self.store.n_items())
            .filter(|position| self.selection.is_selected(*position))
            .filter_map(|position| self.row_at(position))
            .collect()
    }

    #[must_use]
    pub fn selected_count(&self) -> usize {
        (0..self.store.n_items())
            .filter(|position| self.selection.is_selected(*position))
            .count()
    }

    #[must_use]
    pub fn row_at(&self, position: u32) -> Option<Row> {
        self.store
            .item(position)
            .and_then(|object| object.downcast::<Row>().ok())
    }

    pub fn select_first(&self) {
        if self.store.n_items() > 0 {
            self.selection.select_item(0, true);
        }
    }

    pub fn select_position(&self, position: u32) {
        self.selection.select_item(position, true);
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.store.n_items() == 0
    }

    #[must_use]
    pub fn len(&self) -> usize {
        usize::try_from(self.store.n_items()).unwrap_or(usize::MAX)
    }

    /// A label that shows "N items" or "M of N selected", for the status bar.
    /// The view owns the text so header and status bar cannot disagree.
    #[must_use]
    pub fn status_label(&self) -> gtk::Label {
        let label = gtk::Label::new(None);
        label.add_css_class("ufm-status");
        self.status.labels.borrow_mut().push(label.clone());
        self.status.report();
        label
    }

    /// Draw our own column headers. GTK's defaults would show a sort indicator
    /// for a sorter the view does not have, so the headers are buttons that
    /// report the column the user clicked and the arrow is drawn from the key
    /// the view actually sorted by.
    pub fn install_headers(&self, on_sort: impl Fn(SortKey) + 'static) {
        let factory = gtk::SignalListItemFactory::new();
        let headers = Rc::clone(&self.headers);
        let key = self.key.clone();
        // One `Rc` per header: the factory runs once per column, and a plain
        // closure would be moved into the first one and missed by the rest.
        let on_sort = Rc::new(on_sort);
        factory.connect_setup(move |_factory, object| {
            let Some(item) = object.downcast_ref::<gtk::ListItem>() else {
                return;
            };
            let Some(column) = item
                .item()
                .and_then(|o| o.downcast::<gtk::ColumnViewColumn>().ok())
            else {
                return;
            };
            let Some(column) = Column::ALL
                .into_iter()
                .find(|c| column.title().as_deref() == Some(c.title()))
            else {
                return;
            };
            let button = gtk::Button::with_label(column.title());
            button.add_css_class("ufm-header");
            button.set_can_focus(false);
            let headers = Rc::clone(&headers);
            let key = key.clone();
            let on_sort = Rc::clone(&on_sort);
            button.connect_clicked(move |_button| {
                let mut key = key.borrow_mut();
                key.descending = if key.field == column.field() {
                    !key.descending
                } else {
                    false
                };
                key.field = column.field();
                on_sort(*key);
            });
            headers.borrow_mut().push((column, button.clone()));
            item.set_child(Some(&button));
        });
        self.list.set_header_factory(Some(&factory));
        self.paint_headers();
    }

    fn paint_headers(&self) {
        let key = *self.key.borrow();
        for (column, button) in self.headers.borrow().iter() {
            let active = column.field() == key.field;
            button.set_label(&format!(
                "{}{}",
                column.title(),
                if active {
                    if key.descending {
                        " \u{25bc}"
                    } else {
                        " \u{25b2}"
                    }
                } else {
                    ""
                }
            ));
            button.add_css_class("ufm-header-active");
            if !active {
                button.remove_css_class("ufm-header-active");
            }
        }
    }

    fn wire_activate(&self) {
        let handlers = Rc::clone(&self.activators);
        let store = self.store.clone();
        self.grid.connect_activate(move |_view, position| {
            if let Some(row) = store
                .item(position)
                .and_then(|object| object.downcast::<Row>().ok())
            {
                fire(&handlers, &row);
            }
        });

        let handlers = Rc::clone(&self.activators);
        let store = self.store.clone();
        self.list.connect_activate(move |_view, position| {
            // A model position, not a view position. That holds only because
            // the view has no sorter of its own, which is the point of sorting
            // here instead.
            if let Some(row) = store
                .item(position)
                .and_then(|object| object.downcast::<Row>().ok())
            {
                fire(&handlers, &row);
            }
        });
    }

    fn wire_selection(&self) {
        let status = Rc::clone(&self.status);
        let selection = self.selection.clone();
        // The store, not the selection model: `GtkSelectionModel` has no
        // `n-items`, and the count it would report is the store's anyway.
        let store = self.store.clone();
        let selections = Rc::clone(&self.selections);
        self.selection
            .connect_selection_changed(move |_selection, _position, _n| {
                status.selected.set(
                    (0..store.n_items())
                        .filter(|position| selection.is_selected(*position))
                        .count(),
                );
                status.report();
                fire_selection(&selections);
            });
    }
}

/// Run the activate handlers against one row. The list is cloned out first: a
/// handler that navigates replaces the listing, and it must not be called while
/// the list of handlers is borrowed.
fn fire(handlers: &Activators, row: &Row) {
    let cloned: Vec<_> = handlers.borrow().iter().map(Rc::clone).collect();
    for handler in cloned {
        handler(row);
    }
}

/// Run the selection handlers. Same clone-before-calling rule as [`fire`]: a
/// handler that changes a button's sensitivity runs while the model is still
/// being repainted, and one that replaced the listing would replace it through a
/// borrowed handler list.
fn fire_selection(handlers: &Selections) {
    let cloned: Vec<_> = handlers.borrow().iter().map(Rc::clone).collect();
    for handler in cloned {
        handler();
    }
}

/// Grid and columns mode: icon above a wrapped name, centred.
///
/// Shared with the column panes so that a folder looks the same in the grid and
/// in a Miller column, and so there is one factory to keep correct.
pub(super) fn grid_factory(
    icon_size: Rc<Cell<i32>>,
    thumbs: Pipeline,
    on_press: &ContextHandler,
) -> gtk::SignalListItemFactory {
    let factory = gtk::SignalListItemFactory::new();
    let size = Rc::clone(&icon_size);
    let on_press = Rc::clone(on_press);
    factory.connect_setup(move |_factory, object| {
        let Some(item) = object.downcast_ref::<gtk::ListItem>() else {
            return;
        };
        let cell = gtk::Box::new(gtk::Orientation::Vertical, 4);
        cell.add_css_class("ufm-cell");
        let image = gtk::Image::new();
        image.set_pixel_size(size.get());
        image.set_halign(gtk::Align::Center);
        let label = name_label();
        label.set_justify(gtk::Justification::Center);
        label.set_xalign(0.5);
        cell.append(&image);
        cell.append(&label);
        item.set_child(Some(&cell));
        popup::attach(&cell, item, true, &on_press);
    });
    factory.connect_bind(move |_factory, object| {
        let Some(item) = object.downcast_ref::<gtk::ListItem>() else {
            return;
        };
        let Some(row) = item.item().and_then(|object| object.downcast::<Row>().ok()) else {
            return;
        };
        let (image, label) = parts(item.child().as_ref());
        if let Some(image) = image {
            image.set_pixel_size(icon_size.get());
            image.set_icon_name(Some(&row.icon_name()));
            thumbs.request(&image, &row, icon_size.get());
        }
        if let Some(label) = label {
            label.set_text(&row.name());
        }
    });
    factory
}

/// List mode: one column of the table.
fn list_factory(
    column: Column,
    icon_size: Rc<Cell<i32>>,
    thumbs: Pipeline,
    on_press: &ContextHandler,
) -> gtk::SignalListItemFactory {
    let factory = gtk::SignalListItemFactory::new();
    let size = Rc::clone(&icon_size);
    let on_press = Rc::clone(on_press);
    factory.connect_setup(move |_factory, object| {
        let Some(item) = object.downcast_ref::<gtk::ListItem>() else {
            return;
        };
        let label = name_label();
        label.remove_css_class("ufm-name");
        label.add_css_class(if column.is_name() {
            "ufm-name"
        } else {
            "ufm-meta"
        });
        label.set_xalign(if column == Column::Size { 1.0 } else { 0.0 });
        label.set_lines(1);
        // Every column carries the gesture, not just the name: a right-click on
        // a size or a date is still a right-click on that row.
        let cell: gtk::Widget = if column.is_name() {
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
            row.add_css_class("ufm-row");
            let image = gtk::Image::new();
            image.set_pixel_size(size.get());
            row.append(&image);
            row.append(&label);
            row.upcast()
        } else {
            label.upcast()
        };
        item.set_child(Some(&cell));
        popup::attach(&cell, item, true, &on_press);
    });
    factory.connect_bind(move |_factory, object| {
        let Some(item) = object.downcast_ref::<gtk::ListItem>() else {
            return;
        };
        let Some(row) = item.item().and_then(|object| object.downcast::<Row>().ok()) else {
            return;
        };
        let text = match column {
            Column::Name => row.name(),
            Column::Size => row.size_label(),
            Column::Kind => row.kind_label(),
            Column::Modified => row.modified_label(),
        };
        let (image, label) = parts(item.child().as_ref());
        if let Some(image) = image {
            image.set_pixel_size(icon_size.get());
            image.set_icon_name(Some(&row.icon_name()));
            thumbs.request(&image, &row, icon_size.get());
        }
        if let Some(label) = label {
            label.set_text(&text);
        }
    });
    factory
}

/// A two-line, ellipsised, wrapping name label.
fn name_label() -> gtk::Label {
    let label = gtk::Label::new(None);
    label.set_wrap(true);
    label.set_lines(2);
    label.set_ellipsize(pango::EllipsizeMode::End);
    label.set_max_width_chars(18);
    label.add_css_class("ufm-name");
    label
}

/// Find the icon and the name inside the widget a setup handler built, by
/// looking at its children. Cheaper than per-row object data, and it keeps the
/// factories free of `unsafe`, which the object-data API needs.
fn parts(child: Option<&gtk::Widget>) -> (Option<gtk::Image>, Option<gtk::Label>) {
    let mut image = None;
    let mut label = None;
    let mut candidates: Vec<gtk::Widget> = child.into_iter().cloned().collect();
    while let Some(widget) = candidates.pop() {
        if let Some(picture) = widget.downcast_ref::<gtk::Image>() {
            image = Some(picture.clone());
        } else if let Some(text) = widget.downcast_ref::<gtk::Label>() {
            label = Some(text.clone());
        } else {
            // The children model says its items are plain `GObject`s, so they
            // have to be downcast one by one rather than iterated as widgets.
            candidates.extend(
                widget
                    .observe_children()
                    .snapshot()
                    .into_iter()
                    .filter_map(|object| object.downcast::<gtk::Widget>().ok()),
            );
        }
    }
    (image, label)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::tk;
    use std::path::PathBuf;
    use ufm_core::entry::{EntryFlags, EntryKind};

    fn entry(name: &str, kind: EntryKind) -> Entry {
        Entry {
            location: ufm_core::Location::file(PathBuf::from("/tmp").join(name)),
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

    #[test]
    fn every_column_has_a_title_and_a_distinct_sort_field() {
        let mut fields: Vec<_> = Vec::new();
        for column in Column::ALL {
            assert!(!column.title().is_empty());
            assert_eq!(column.is_name(), column.field() == SortField::Name);
            fields.push(column.field());
        }
        for (index, field) in fields.iter().enumerate() {
            assert!(
                !fields[..index].contains(field),
                "column {} sorts like an earlier one",
                Column::ALL[index].title()
            );
        }
        assert_eq!(fields.len(), Column::ALL.len());
    }

    #[test]
    fn a_view_starts_empty() {
        tk::ui(|| {
            let view = View::new(SortKey::default(), Pipeline::new());
            assert!(view.is_empty());
            assert_eq!(view.len(), 0);
            assert_eq!(view.selected_count(), 0);
            assert!(!view.sort_key().descending);
        });
    }

    #[test]
    fn a_listing_is_shown_in_the_order_it_arrived() {
        tk::ui(|| {
            let view = View::new(SortKey::default(), Pipeline::new());
            view.set_entries(vec![
                entry("zebra", EntryKind::File),
                entry("Docs", EntryKind::Directory),
            ]);
            assert_eq!(view.len(), 2);
            assert_eq!(view.row_at(0).expect("row").name(), "zebra");
        });
    }

    #[test]
    fn resorting_reorders_the_rows_it_already_holds() {
        tk::ui(|| {
            let view = View::new(SortKey::default(), Pipeline::new());
            view.set_entries(vec![
                entry("a", EntryKind::File),
                entry("bb", EntryKind::File),
                entry("ccc", EntryKind::File),
            ]);
            view.resort(SortKey {
                field: SortField::Name,
                descending: true,
                folders_first: false,
            });
            assert_eq!(view.row_at(0).expect("row").name(), "ccc");
            assert!(view.sort_key().descending);
        });
    }

    #[test]
    fn a_batch_appends_without_replacing() {
        tk::ui(|| {
            let view = View::new(SortKey::default(), Pipeline::new());
            view.push_batch(&[entry("one", EntryKind::File)]);
            view.push_batch(&[entry("two", EntryKind::File)]);
            assert_eq!(view.len(), 2);
        });
    }

    #[test]
    fn selection_is_reported_as_entries() {
        tk::ui(|| {
            let view = View::new(SortKey::default(), Pipeline::new());
            view.set_entries(vec![
                entry("a", EntryKind::File),
                entry("b", EntryKind::File),
            ]);
            view.select_first();
            assert_eq!(view.selected_count(), 1);
            assert_eq!(view.selected()[0].name, "a");
        });
    }

    #[test]
    fn replacing_a_listing_clears_the_selection() {
        tk::ui(|| {
            let view = View::new(SortKey::default(), Pipeline::new());
            view.set_entries(vec![entry("a", EntryKind::File)]);
            view.select_first();
            assert_eq!(view.selected_count(), 1);
            view.set_entries(vec![entry("b", EntryKind::File)]);
            assert_eq!(view.selected_count(), 0);
        });
    }

    #[test]
    fn the_status_label_counts_and_reports_selection() {
        tk::ui(|| {
            let view = View::new(SortKey::default(), Pipeline::new());
            let label = view.status_label();
            view.set_entries(vec![
                entry("a", EntryKind::File),
                entry("b", EntryKind::File),
            ]);
            assert_eq!(label.text(), "2 items");
            view.select_first();
            assert_eq!(label.text(), "1 of 2 selected");
            view.set_entries(vec![]);
            assert_eq!(label.text(), "Empty folder");
        });
    }

    #[test]
    fn one_item_is_one_item_not_one_items() {
        tk::ui(|| {
            let view = View::new(SortKey::default(), Pipeline::new());
            let label = view.status_label();
            view.set_entries(vec![entry("a", EntryKind::File)]);
            assert_eq!(label.text(), "1 item");
        });
    }

    #[test]
    fn parts_finds_the_widgets_a_factory_built() {
        tk::ui(|| {
            let cell = gtk::Box::new(gtk::Orientation::Vertical, 0);
            let image = gtk::Image::new();
            let label = name_label();
            cell.append(&image);
            cell.append(&label);
            let (found_image, found_label) = parts(Some(&cell.upcast::<gtk::Widget>()));
            assert!(found_image.is_some_and(|i| i == image));
            assert!(found_label.is_some_and(|l| l == label));
        });
    }

    #[test]
    fn parts_tolerates_a_row_with_no_child() {
        tk::ui(|| {
            assert!(parts(None).0.is_none());
            assert!(parts(None).1.is_none());
        });
    }

    #[test]
    fn switching_modes_shows_the_matching_child_and_icon_size() {
        tk::ui(|| {
            let view = View::new(SortKey::default(), Pipeline::new());
            view.set_mode(ViewMode::List);
            assert_eq!(view.mode(), ViewMode::List);
            assert_eq!(view.icon_size.get(), LIST_ICON);
            view.set_mode(ViewMode::Grid);
            assert_eq!(view.icon_size.get(), GRID_ICON);
        });
    }
}
