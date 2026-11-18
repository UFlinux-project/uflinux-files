//! The right-click menu, and the hit-testing needed to know where it belongs.
//!
//! Half of this module is policy: which rows a menu gets, given what is
//! selected. That is a plain function over a selection ([`rows_for`]), so it is
//! tested without a display, and the two presentations — the icon/list view and
//! the column panes — are held to the same answer by the same tests.
//!
//! The other half is the awkward part. A context menu has to know which row the
//! pointer was over, and neither `GtkListView` nor `GtkColumnView` says so in a
//! way this binding can call: `gtk_list_view_compute_item_for_point()` has no
//! binding in gtk4-rs 0.10, and the wrappers for a row (`GtkListItem`,
//! `GtkColumnViewCell`) are not widgets, so [`gtk::Widget::pick`] cannot report
//! one and walking up from a picked widget cannot reach one either. So the row
//! is identified from *inside* the row instead. Every cell the factories build
//! carries a right-click gesture ([`attach`]) which reads its own item's position
//! when it fires — at press time rather than setup time, because cells are
//! recycled and a position remembered at setup goes stale. Menus over empty space
//! come from a second gesture on the list widget itself ([`background`]), which
//! fires only when the press did not land on a row; it decides that with `pick()`,
//! which is enough here because a press over a row picks a widget inside that
//! row, and that widget is a descendant of the list rather than the list.
//!
//! The menu itself is one [`Popup`] per window, reused: its model is swapped for
//! each opening, and because it is parented to the window its `win.` rows resolve
//! against the window's own actions, greying out exactly where the header menu
//! and the keyboard do.

use std::cell::RefCell;
use std::rc::Rc;

use gtk::prelude::*;
use gtk::{gio, glib};
use ufm_core::entry::Entry;

/// Where a context-menu press landed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    /// The pointer was over the row at this position in the list being clicked.
    ///
    /// `in_current` says whether that list is showing the folder the window is
    /// currently in. In the grid and the list view it always is; in a Miller
    /// column it is only the deepest pane, and the difference decides whether the
    /// folder-level rows may be offered at all.
    Row { position: u32, in_current: bool },
    /// The pointer was over the folder itself, below or beside the rows.
    Background { in_current: bool },
}

impl Target {
    /// The row position, when the press was on one.
    #[must_use]
    pub fn position(self) -> Option<u32> {
        match self {
            Self::Row { position, .. } => Some(position),
            Self::Background { .. } => None,
        }
    }

    /// Whether the press happened in the folder the window is in.
    #[must_use]
    pub fn in_current(self) -> bool {
        match self {
            Self::Row { in_current, .. } | Self::Background { in_current } => in_current,
        }
    }
}

/// What a presentation calls when a context-menu press happens: the widget that
/// was clicked, the row it landed on, and the coordinates of the press inside
/// that widget.
pub type ContextHandler = Rc<dyn Fn(&gtk::Widget, Target, f64, f64)>;

/// Every subscriber to one list's context presses. Each cell takes a cheap clone
/// of the same list, so adding a factory for another column costs nothing beyond
/// the closure it already needed.
#[derive(Clone, Default)]
pub struct Listeners(Rc<RefCell<Vec<ContextHandler>>>);

impl Listeners {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Subscribe to the next context presses. Handlers run in the order they were
    /// added; a handler added while a press is being dispatched waits for the
    /// next one, which is what makes it safe to subscribe from a handler.
    pub fn push<F>(&self, f: F)
    where
        F: Fn(&gtk::Widget, Target, f64, f64) + 'static,
    {
        self.0.borrow_mut().push(Rc::new(f));
    }

    /// The callback a cell gesture should carry: one allocation per cell, one
    /// shared list behind all of them.
    #[must_use]
    pub fn handler(&self) -> ContextHandler {
        let shared = Rc::clone(&self.0);
        Rc::new(move |widget, target, x, y| {
            // Cloned out first, so a handler may add or drop listeners without
            // tripping the borrow this iteration holds.
            let listeners = shared.borrow().clone();
            for listener in listeners {
                listener(widget, target, x, y);
            }
        })
    }
}

impl std::fmt::Debug for Listeners {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Listeners")
            .field("listeners", &self.0.borrow().len())
            .finish()
    }
}

/// Put the right-click gesture on one cell of a list. The row is read from the
/// item when the press happens, not when the cell was built.
///
/// `in_current` says whether this list is showing the folder the window is in —
/// see [`Target::Row`].
pub fn attach(
    cell: &impl IsA<gtk::Widget>,
    item: &gtk::ListItem,
    in_current: bool,
    on_press: &ContextHandler,
) {
    let gesture = gtk::GestureClick::new();
    gesture.set_button(3);
    let on_press = Rc::clone(on_press);
    let weak = item.downgrade();
    gesture.connect_pressed(move |controller, n_press, x, y| {
        if n_press != 1 {
            return;
        }
        let (Some(item), Some(widget)) = (weak.upgrade(), controller.widget()) else {
            return;
        };
        let target = Target::Row {
            position: item.position(),
            in_current,
        };
        on_press(&widget, target, x, y);
    });
    cell.add_controller(gesture);
}

/// The gesture that catches a right-click on the folder itself, over the empty
/// space below or beside the rows. Added to the list widget, where it sees every
/// press; it reports only the ones no row claimed. See [`Target::Background`] for
/// `in_current`.
#[must_use]
pub fn background(on_press: &ContextHandler, in_current: bool) -> gtk::GestureClick {
    let gesture = gtk::GestureClick::new();
    gesture.set_button(3);
    let on_press = Rc::clone(on_press);
    gesture.connect_pressed(move |controller, n_press, x, y| {
        if n_press != 1 {
            return;
        }
        let Some(widget) = controller.widget() else {
            return;
        };
        // A press over a row picks a widget inside that row, which is a
        // descendant of the list rather than the list itself. Anything else —
        // nothing at all, or the list — is the folder showing through.
        let claimed = widget
            .pick(x, y, gtk::PickFlags::DEFAULT)
            .is_some_and(|hit| hit != widget);
        if claimed {
            return;
        }
        on_press(&widget, Target::Background { in_current }, x, y);
    });
    gesture
}

/// A row of the context menu. Each one is a label and the window action it
/// activates, so the menu holds no opinion of its own about whether a command
/// applies or is even possible: the action answers that, and the menu item greys
/// out with it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    Open,
    OpenWith,
    NewFolder,
    Star,
    Unstar,
    Bookmark,
    Unbookmark,
    Rename,
    Trash,
    Delete,
    Copy,
    Cut,
    Paste,
    Extract,
    Properties,
}

impl Command {
    /// Every row, in the order the sections below lay them out.
    const ALL: [Self; 15] = [
        Self::Open,
        Self::OpenWith,
        Self::NewFolder,
        Self::Star,
        Self::Unstar,
        Self::Bookmark,
        Self::Unbookmark,
        Self::Rename,
        Self::Trash,
        Self::Delete,
        Self::Copy,
        Self::Cut,
        Self::Paste,
        Self::Extract,
        Self::Properties,
    ];

    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Open => "Open",
            Self::OpenWith => "Open With\u{2026}",
            Self::NewFolder => "New Folder",
            Self::Star => "Star",
            Self::Unstar => "Unstar",
            Self::Bookmark => "Bookmark This Folder",
            Self::Unbookmark => "Remove Bookmark",
            Self::Rename => "Rename",
            Self::Trash => "Move to Trash",
            Self::Delete => "Delete Permanently",
            Self::Copy => "Copy",
            Self::Cut => "Cut",
            Self::Paste => "Paste",
            Self::Extract => "Extract to Folder",
            Self::Properties => "Properties",
        }
    }

    /// The window action this row activates.
    #[must_use]
    pub fn action(self) -> &'static str {
        match self {
            Self::Open => "win.open",
            Self::OpenWith => "win.open-with",
            Self::NewFolder => "win.new-folder",
            Self::Star | Self::Unstar => "win.star",
            Self::Bookmark | Self::Unbookmark => "win.bookmark",
            Self::Rename => "win.rename",
            Self::Trash => "win.trash",
            Self::Delete => "win.delete",
            Self::Copy => "win.copy",
            Self::Cut => "win.cut",
            Self::Paste => "win.paste",
            Self::Extract => "win.extract",
            Self::Properties => "win.properties",
        }
    }

    fn section(self) -> Section {
        match self {
            Self::Open | Self::OpenWith => Section::Open,
            Self::NewFolder | Self::Paste => Section::Add,
            Self::Star
            | Self::Unstar
            | Self::Bookmark
            | Self::Unbookmark
            | Self::Rename
            | Self::Trash
            | Self::Delete => Section::Change,
            Self::Copy | Self::Cut => Section::Take,
            Self::Extract | Self::Properties => Section::About,
        }
    }
}

/// The groups a menu is drawn in. GTK separates sections with a line, which is
/// how the menu says "these are different kinds of thing" without a wall of text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Section {
    Open,
    Add,
    Change,
    Take,
    About,
}

const SECTIONS: [Section; 5] = [
    Section::Open,
    Section::Add,
    Section::Change,
    Section::Take,
    Section::About,
];

/// What a menu is being opened over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    /// The folder itself, with nothing selected.
    Background,
    /// Exactly one entry.
    One,
    /// Two or more.
    Many,
}

impl Shape {
    /// The shape a menu opened over `entries` has. Empty means the background,
    /// which is a different menu rather than a one-row menu with the row missing.
    #[must_use]
    pub fn of(entries: &[Entry]) -> Self {
        match entries.len() {
            0 => Self::Background,
            1 => Self::One,
            _ => Self::Many,
        }
    }
}

/// What a menu may offer on top of what the selection allows. Both presentations
/// build one of these from the press and hand it to [`rows_for`], so the policy
/// lives in one place.
#[derive(Debug, Clone, Copy)]
pub struct Scope {
    /// The press happened in the folder the window is currently in. The
    /// folder-level rows — New Folder, Paste — act on that folder, so they are
    /// only offered when this is true; anywhere else they would name one folder
    /// and act on another.
    pub in_current: bool,
    /// The selection is something we can unpack: an archive, or the archive
    /// folder currently open.
    pub extracting: bool,
    /// Exactly one row is selected and it is something an external program could
    /// be handed. A folder is entered rather than opened, and a virtual location
    /// has no bytes to give a program, so the chooser has nothing to list. Asked
    /// of the window rather than worked out here, because the window is what knows
    /// whether the selection is one row or three.
    pub openable: bool,
    /// The clipboard has something to put into this folder. The same test the
    /// Paste action uses.
    pub pastable: bool,
    /// Every row the menu covers is already starred, so the row should say
    /// "Unstar" rather than "Star". One row rather than two is the point: a menu
    /// offering both would ask the user to guess which one takes effect. Asked of
    /// the window for the same reason as [`Scope::openable`] — the rows carry the
    /// flag, and only the window has them.
    pub starred: bool,
    /// The folder the menu is standing in is already in the sidebar, so the row
    /// should say "Remove Bookmark". Asked of the window rather than of the rows
    /// because a bookmark names a folder, not a row: the rows a menu covers live
    /// *in* the thing being bookmarked, and only the window knows what that is.
    /// Only meaningful together with [`Scope::in_current`] — anywhere else the
    /// row would name one folder and bookmark another.
    pub bookmarked: bool,
}

/// The bookmark row for a menu over a folder that is bookmarked when `bookmarked`
/// says so. The sibling of [`star_row`], and the same shape for the same reason:
/// one command that goes both ways, and the label is what says which way.
#[must_use]
fn bookmark_row(bookmarked: bool) -> Command {
    if bookmarked {
        Command::Unbookmark
    } else {
        Command::Bookmark
    }
}

/// The star row for a menu over rows that are starred when `starred` says so.
/// Always exactly one row: the command is one toggle, and the label is what says
/// which way it goes.
#[must_use]
fn star_row(starred: bool) -> Command {
    if starred {
        Command::Unstar
    } else {
        Command::Star
    }
}

/// The rows of the menu for a given selection.
///
/// A row is left out rather than greyed when the command cannot work here at all
/// — see [`Scope`].
///
/// The folder-level rows sit in the row menus too, not only over the background:
/// the folder a row lives in is implied by the row, and in a folder whose rows
/// fill the view there is no empty space left to click.
#[must_use]
pub fn rows_for(entries: &[Entry], scope: Scope) -> Vec<Command> {
    match Shape::of(entries) {
        Shape::Background => background_rows(scope),
        Shape::One => one_row(&entries[0], scope),
        Shape::Many => many_rows(scope),
    }
}

fn background_rows(scope: Scope) -> Vec<Command> {
    let mut rows = Vec::new();
    if scope.in_current {
        rows.push(Command::NewFolder);
        if scope.pastable {
            rows.push(Command::Paste);
        }
        // Over the folder itself the bookmark row is the only one here that acts
        // on what the menu is already pointing at, so it belongs. The two rows
        // above it add something to the folder; this one says something about it.
        rows.push(bookmark_row(scope.bookmarked));
    }
    rows
}

fn one_row(entry: &Entry, scope: Scope) -> Vec<Command> {
    let mut rows = Vec::new();
    if entry.navigable() {
        rows.push(Command::Open);
    }
    // The other half of the same question, and offered on the opposite condition:
    // `Open` needs something to enter, this needs bytes to hand over. A container
    // offers both — entering it and taking one program to the container itself —
    // and a plain file only this one, which is why the row above is not enough.
    if scope.openable {
        rows.push(Command::OpenWith);
    }
    if scope.in_current {
        rows.push(Command::NewFolder);
        if scope.pastable {
            rows.push(Command::Paste);
        }
    }
    // Before the destructive rows rather than after them: starring a row is the
    // lightest thing this group offers, and the row that undoes it is the same
    // row with a different label.
    rows.push(star_row(scope.starred));
    // Straight after the star, because it is the other reversible row here — and
    // because it acts on the folder the menu is standing in rather than on the
    // row, which is why it appears only when those are the same folder. A user
    // who right-clicks a row to bookmark the folder holding it is asking for the
    // same thing as one who right-clicks the empty space beside it.
    if scope.in_current {
        rows.push(bookmark_row(scope.bookmarked));
    }
    rows.extend([
        Command::Rename,
        Command::Trash,
        Command::Delete,
        Command::Copy,
        Command::Cut,
    ]);
    if scope.extracting {
        rows.push(Command::Extract);
    }
    rows.push(Command::Properties);
    rows
}

fn many_rows(scope: Scope) -> Vec<Command> {
    let mut rows = Vec::new();
    if scope.in_current {
        rows.push(Command::NewFolder);
        if scope.pastable {
            rows.push(Command::Paste);
        }
    }
    // Several rows star together and unstar together. `Scope::starred` is only
    // true when every one of them is starred, so the row cannot promise to undo
    // something it did not do to all of them.
    rows.push(star_row(scope.starred));
    // The folder is one thing however many rows are selected, so the bookmark row
    // needs no such agreement — and it stays where the single-row menu puts it,
    // because a menu whose rows move depending on how many rows are selected is a
    // menu you cannot learn.
    if scope.in_current {
        rows.push(bookmark_row(scope.bookmarked));
    }
    rows.extend([Command::Trash, Command::Delete, Command::Copy, Command::Cut]);
    if scope.extracting {
        rows.push(Command::Extract);
    }
    rows
}

/// The menu model for a set of rows: the rows grouped into their sections, with
/// empty groups left out.
#[must_use]
pub fn model(rows: &[Command]) -> gio::Menu {
    let menu = gio::Menu::new();
    for section in SECTIONS {
        let part = gio::Menu::new();
        let mut count = 0;
        for row in rows {
            if row.section() == section {
                part.append(Some(row.label()), Some(row.action()));
                count += 1;
            }
        }
        if count > 0 {
            menu.append_section(None, &part);
        }
    }
    menu
}

/// One row of a menu whose commands act on a thing named at the moment the menu
/// was built rather than on whatever the selection turns out to be.
///
/// The [`Command`] enum cannot express that: it is a fixed set of labels, and the
/// whole point of the sidebar's menu is that the row says *which* bookmark to
/// rename. A target is also why the row is a struct rather than a pair — a menu
/// with no target is the common case and deserves to say so by name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MenuRow {
    /// What the row says.
    pub label: &'static str,
    /// The window action to activate, in GTK's `win.name` form.
    pub action: &'static str,
    /// The parameter to hand it, when it takes one.
    pub target: Option<String>,
}

/// The menu model for rows that carry their own target: one section, in the order
/// handed over. No grouping, because these menus are four rows about one thing and
/// a separator between `Move Up` and `Move Down` would say they are different
/// kinds of command.
#[must_use]
pub fn model_for(rows: &[MenuRow]) -> gio::Menu {
    let menu = gio::Menu::new();
    let part = gio::Menu::new();
    for row in rows {
        let item = gio::MenuItem::new(Some(row.label), Some(row.action));
        if let Some(target) = &row.target {
            item.set_action_and_target_value(
                Some(row.action),
                Some(&glib::Variant::from(target.clone())),
            );
        }
        part.append_item(&item);
    }
    menu.append_section(None, &part);
    menu
}

/// The labels of a menu model, grouped as drawn. For tests and for the
/// accessibility check: a flat list would pass even with the grouping lost.
/// Generic over the model so that a [`gio::Menu`] can be handed straight back.
#[must_use]
pub fn sections(model: &impl IsA<gio::MenuModel>) -> Vec<Vec<String>> {
    let model = model.upcast_ref();
    (0..model.n_items())
        .map(|index| match model.item_link(index, "section") {
            Some(section) => labels(&section),
            None => vec![label_of(model, index).unwrap_or_default()],
        })
        .collect()
}

/// Every row of a menu, flat, with the target each one carries. The sidebar's
/// menu is read back this way: a label alone would pass even if the URI that says
/// *which* bookmark to rename had been dropped on the way into the model.
#[must_use]
pub fn targets(model: &impl IsA<gio::MenuModel>) -> Vec<(String, Option<String>)> {
    let model = model.upcast_ref();
    (0..model.n_items())
        .flat_map(|index| match model.item_link(index, "section") {
            Some(section) => flat(&section),
            None => vec![(label_of(model, index).unwrap_or_default(), None)],
        })
        .collect()
}

fn flat(model: &impl IsA<gio::MenuModel>) -> Vec<(String, Option<String>)> {
    let model = model.upcast_ref();
    (0..model.n_items())
        .map(|index| {
            (
                label_of(model, index).unwrap_or_default(),
                model
                    .item_attribute_value(index, "target", Some(glib::VariantTy::STRING))
                    .and_then(|value| value.str().map(str::to_string)),
            )
        })
        .collect()
}

fn labels(model: &impl IsA<gio::MenuModel>) -> Vec<String> {
    let model = model.upcast_ref();
    (0..model.n_items())
        .filter_map(|index| label_of(model, index))
        .collect()
}

fn label_of(model: &gio::MenuModel, index: i32) -> Option<String> {
    model
        .item_attribute_value(index, "label", Some(glib::VariantTy::STRING))
        .and_then(|value| value.str().map(str::to_string))
}

/// The window's context menu. One per window, built once, reused: what changes
/// between openings is the model.
#[derive(Debug, Clone)]
pub struct Popup {
    menu: gtk::PopoverMenu,
}

impl Popup {
    /// Build the window's context menu. Parenting it to the window is what makes
    /// `win.` resolve against that window's actions, and it goes through
    /// [`gtk::Widget::set_parent`] because the binding has no setter for a
    /// popover's `parent` property.
    #[must_use]
    pub fn install(window: &gtk::ApplicationWindow) -> Rc<Self> {
        let menu =
            gtk::PopoverMenu::from_model_full(&gio::Menu::new(), gtk::PopoverMenuFlags::NESTED);
        menu.add_css_class("ufm-context-menu");
        menu.set_parent(window);
        menu.set_has_arrow(false);
        let weak = menu.downgrade();
        window.connect_close_request(move |_| {
            if let Some(menu) = weak.upgrade()
                && menu.parent().is_some()
            {
                menu.popdown();
                menu.unparent();
            }
            glib::Propagation::Proceed
        });
        Rc::new(Self { menu })
    }

    #[must_use]
    pub fn is_visible(&self) -> bool {
        self.menu.is_visible()
    }

    /// Close it. Opening a menu over another row closes the old one on its own;
    /// this is for the points where the menu would otherwise go stale — after
    /// navigation, or when a row activates.
    pub fn hide(&self) {
        self.menu.popdown();
    }

    /// The rows the menu is currently offering, grouped as drawn. Empty before
    /// the first opening.
    #[must_use]
    pub fn rows(&self) -> Vec<Vec<String>> {
        self.menu
            .menu_model()
            .map_or_else(Vec::new, |model| sections(&model))
    }

    /// The rows the menu is offering, with the target each one carries. For a menu
    /// built from [`MenuRow`], where the target is half the row: a label with no
    /// URI would rename some bookmark, and which one is the question the row
    /// exists to answer.
    #[must_use]
    pub fn targeted(&self) -> Vec<(String, Option<String>)> {
        self.menu
            .menu_model()
            .map_or_else(Vec::new, |model| targets(&model))
    }

    /// Open the menu with `rows`, pointing at `spot` — the widget the press
    /// landed on and the coordinates inside it — or at the focused widget when
    /// there is no pointer to follow.
    ///
    /// Swapping the model while a popover is open leaves the old geometry behind,
    /// so it is closed first even when it does not look open.
    #[cfg(test)]
    pub fn show(
        &self,
        window: &gtk::ApplicationWindow,
        rows: &[Command],
        spot: Option<(&gtk::Widget, f64, f64)>,
    ) {
        self.show_model(window, &model(rows), spot);
    }

    pub fn show_model(
        &self,
        window: &gtk::ApplicationWindow,
        model: &gio::Menu,
        spot: Option<(&gtk::Widget, f64, f64)>,
    ) {
        self.menu.popdown();
        if model.n_items() == 0 {
            return;
        }
        self.menu.set_menu_model(Some(model));
        let rect = pointing_to(window, spot);
        self.menu.set_pointing_to(rect.as_ref());
        if let (Some(rect), Some(child)) = (rect, self.menu.child()) {
            let (_, menu_width, _, _) = child.measure(gtk::Orientation::Horizontal, -1);
            let (_, menu_height, _, _) = child.measure(gtk::Orientation::Vertical, menu_width);
            // Account for the small inset around the menu's scrolled child.
            let menu_width = menu_width + 16;
            let menu_height = menu_height + 16;
            let height = window.height();
            let width = window.width();
            let above = rect.y();
            let below = height - rect.y();
            let left = rect.x();
            let right = width - rect.x();
            let position = if above.max(below) < menu_height && left.max(right) >= menu_width {
                if right >= left {
                    gtk::PositionType::Right
                } else {
                    gtk::PositionType::Left
                }
            } else if below >= above {
                gtk::PositionType::Bottom
            } else {
                gtk::PositionType::Top
            };
            self.menu.set_position(position);
        }
        self.menu.popup();
    }

    /// Open the menu with rows that carry their own target. The same popover, for
    /// the same reasons — one menu per window means one menu to close, and
    /// `win.` still resolves against this window's actions.
    ///
    /// The sidebar's menu is the only caller: its rows name a bookmark rather than
    /// a selection, so they cannot be [`Command`]s.
    pub fn show_rows(
        &self,
        window: &gtk::ApplicationWindow,
        rows: &[MenuRow],
        spot: Option<(&gtk::Widget, f64, f64)>,
    ) {
        self.show_model(window, &model_for(rows), spot);
    }
}

/// Where to point the menu. With a press, that is the press, translated into
/// window coordinates. Without one — the keyboard — the focused widget's own
/// bounds, which `compute_bounds()` already reports in window coordinates, so no
/// pointer is needed at all.
fn pointing_to(
    window: &gtk::ApplicationWindow,
    spot: Option<(&gtk::Widget, f64, f64)>,
) -> Option<gtk::gdk::Rectangle> {
    let (x, y) = if let Some((widget, x, y)) = spot {
        let at = gtk::graphene::Point::new(x as f32, y as f32);
        let point = widget.compute_point(window, &at)?;
        (f64::from(point.x()), f64::from(point.y()))
    } else {
        // Qualified because an `ApplicationWindow` answers `focus` to both
        // `GtkWindowExt` and `RootExt`; the window's own answer is the one
        // that reports focus inside this window's own focusable tree.
        let focus = GtkWindowExt::focus(window)?;
        let bounds = focus.compute_bounds(window)?;
        let corners = bounds.vertices();
        let from = corners[0].to_float();
        let to = corners[2].to_float();
        (
            f64::from(from[0] + (to[0] - from[0]) / 2.0),
            f64::from(from[1] + (to[1] - from[1]) / 2.0),
        )
    };
    Some(gtk::gdk::Rectangle::new(
        x.round() as i32,
        y.round() as i32,
        1,
        1,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::tk;
    use std::path::PathBuf;
    use ufm_core::Location;
    use ufm_core::entry::{EntryFlags, EntryKind};

    fn kinded(name: &str, kind: EntryKind) -> Entry {
        Entry {
            location: Location::file(PathBuf::from("/tmp/t/tree").join(name)),
            name: name.to_string(),
            kind,
            flags: EntryFlags::WRITABLE,
            size: 4,
            allocated: None,
            modified: None,
            accessed: None,
            created: None,
            mode: Some(0o644),
            uid: None,
            gid: None,
            mime: None,
            child_count: None,
            icon_key: kind.icon_name().to_string(),
        }
    }

    fn file(name: &str) -> Entry {
        kinded(name, EntryKind::File)
    }

    fn folder(name: &str) -> Entry {
        kinded(name, EntryKind::Directory)
    }

    fn archive(name: &str) -> Entry {
        kinded(name, EntryKind::Archive)
    }

    /// A press in the folder the window is showing, which is where a menu is
    /// usually opened. Nothing on the clipboard, and the row selected is a folder,
    /// which is entered rather than opened.
    const HERE: Scope = Scope {
        in_current: true,
        extracting: false,
        openable: false,
        pastable: false,
        starred: false,
        bookmarked: false,
    };

    /// A press in one of the older Miller columns.
    const THERE: Scope = Scope {
        in_current: false,
        extracting: false,
        openable: false,
        pastable: false,
        starred: false,
        bookmarked: false,
    };

    /// Fire a gesture's `pressed` by hand. The harness has no pointer, so a
    /// synthetic emit is the only way to test what a press does without one.
    fn press(gesture: &gtk::GestureClick, n_press: i32, x: f64, y: f64) {
        gesture.emit_by_name::<()>("pressed", &[&n_press, &x, &y]);
    }

    /// The one controller a widget carries, as a click gesture.
    fn only_gesture(widget: &impl IsA<gtk::Widget>) -> gtk::GestureClick {
        let gestures: Vec<_> = widget
            .observe_controllers()
            .snapshot()
            .into_iter()
            .filter_map(|object| object.downcast::<gtk::GestureClick>().ok())
            .filter(|gesture| gesture.button() == 3)
            .collect();
        assert_eq!(gestures.len(), 1, "expected one context-menu gesture");
        gestures[0].clone()
    }

    /// A window to hang a popover off. Not shown: nothing here asserts on screen,
    /// only on what the menu was told to hold.
    fn window() -> gtk::ApplicationWindow {
        let app = gtk::Application::builder()
            .application_id("org.uflinux.FileManager.popup.test")
            .build();
        gtk::ApplicationWindow::new(&app)
    }

    #[test]
    fn every_row_has_a_label_and_points_at_a_window_action() {
        for row in Command::ALL {
            assert!(!row.label().is_empty(), "{row:?} has no label");
            assert!(
                row.action().starts_with("win."),
                "{row:?} does not activate a window action"
            );
        }
    }

    #[test]
    fn nothing_selected_is_the_folder_menu() {
        assert_eq!(
            rows_for(&[], HERE),
            vec![Command::NewFolder, Command::Bookmark]
        );
    }

    #[test]
    fn one_row_offers_what_applies_to_one_row() {
        assert_eq!(
            rows_for(&[folder("one")], HERE),
            vec![
                Command::Open,
                Command::NewFolder,
                Command::Star,
                Command::Bookmark,
                Command::Rename,
                Command::Trash,
                Command::Delete,
                Command::Copy,
                Command::Cut,
                Command::Properties,
            ]
        );
    }

    #[test]
    fn a_bookmarked_folder_is_offered_the_row_that_takes_the_bookmark_off() {
        // The star's shape, and for the star's reasons: one row either way, and a
        // label rather than a "toggle" that hides which way it goes.
        let marked = Scope {
            bookmarked: true,
            ..HERE
        };
        for picked in [
            vec![folder("one")],
            vec![folder("one"), file("note.txt")],
            vec![],
        ] {
            let rows = rows_for(&picked, marked);
            assert!(
                rows.contains(&Command::Unbookmark),
                "a folder already in the sidebar should say so"
            );
            assert!(
                !rows.contains(&Command::Bookmark),
                "and should not offer to put it there twice"
            );
            assert_eq!(
                rows.iter()
                    .filter(|row| **row == Command::Unbookmark)
                    .count(),
                1,
                "the bookmark should be offered once"
            );
        }
        // In an older column the row would name one folder and bookmark another.
        let elsewhere = Scope {
            bookmarked: true,
            ..THERE
        };
        assert!(
            !rows_for(&[folder("one")], elsewhere).contains(&Command::Unbookmark),
            "a menu in another column cannot say which folder it would bookmark"
        );
        // Several rows selected bookmark the folder they all live in, which needs
        // no agreement between them the way unstarring several does.
        let many = rows_for(&[folder("one"), file("note.txt")], HERE);
        assert!(many.contains(&Command::Bookmark));
    }

    #[test]
    fn a_starred_row_is_offered_the_row_that_takes_the_star_back() {
        // One row either way, so the menu never holds both. The label is the only
        // difference, which is also why the row cannot be a single "Toggle star"
        // — a menu row that does not say what it will do is a coin toss.
        let starred = Scope {
            starred: true,
            ..HERE
        };
        let rows = rows_for(&[folder("one")], starred);
        assert!(rows.contains(&Command::Unstar));
        assert!(!rows.contains(&Command::Star));
        assert_eq!(
            rows.iter().filter(|row| **row == Command::Unstar).count(),
            1,
            "the star should be offered once"
        );
        // Several rows that are all starred unstar together, and a mix does not
        // get an Unstar that would leave some of them alone.
        let many = rows_for(&[folder("one"), file("note.txt")], starred);
        assert!(many.contains(&Command::Unstar));
        assert!(!rows_for(&[folder("one"), file("note.txt")], HERE).contains(&Command::Unstar));
        assert!(
            !rows_for(&[], HERE)
                .iter()
                .any(|row| matches!(row, Command::Star | Command::Unstar)),
            "over the background there is no row to star"
        );
    }

    #[test]
    fn a_plain_file_is_not_offered_up_as_something_to_open() {
        let rows = rows_for(&[file("note.txt")], HERE);
        assert!(
            !rows.contains(&Command::Open),
            "the default handler may not exist, and a row that cannot be entered \
             should not promise it will"
        );
    }

    #[test]
    fn a_file_that_a_program_could_take_is_offered_to_one() {
        // The other half of the row above: a file cannot be entered, and this is
        // where it stops being something the user can only hope has a handler.
        let chooser = Scope {
            openable: true,
            ..HERE
        };
        let rows = rows_for(&[file("note.txt")], chooser);
        assert!(
            rows.contains(&Command::OpenWith),
            "a file with bytes to hand over should say which programs will take them"
        );
        assert!(
            !rows.contains(&Command::Open),
            "and it should still not promise a folder to enter"
        );
        assert!(
            !rows_for(&[folder("one")], HERE).contains(&Command::OpenWith),
            "a folder is entered, not opened"
        );
    }

    #[test]
    fn an_archive_offers_extraction_and_an_ordinary_folder_does_not() {
        let unpacking = Scope {
            extracting: true,
            ..HERE
        };
        assert!(
            rows_for(&[archive("bundle.tar")], unpacking).contains(&Command::Extract),
            "an archive should offer extraction"
        );
        assert!(
            !rows_for(&[folder("one")], HERE).contains(&Command::Extract),
            "a plain folder has nothing to extract"
        );
    }

    #[test]
    fn several_rows_lose_the_commands_that_have_no_answer() {
        let rows = rows_for(&[folder("one"), file("note.txt")], HERE);
        assert!(!rows.contains(&Command::Open), "which row would it open?");
        assert!(
            !rows.contains(&Command::Rename),
            "which row would it rename?"
        );
        assert!(
            !rows.contains(&Command::Properties),
            "one dialog cannot describe two entries"
        );
        assert!(rows.contains(&Command::Trash), "trashing several is fine");
        assert!(rows.contains(&Command::Delete), "deleting several is fine");
        assert!(rows.contains(&Command::Copy), "copying several is fine");
        assert!(rows.contains(&Command::Cut), "cutting several is fine");
    }

    #[test]
    fn a_press_in_another_column_keeps_the_folder_rows_out() {
        // "New Folder" and "Paste" act on the folder the window is in, not the
        // one under the pointer, so in an older column they would say one folder
        // and do another. Everything that acts on the row itself stays.
        let rows = rows_for(&[folder("one")], THERE);
        assert!(
            !rows.contains(&Command::NewFolder),
            "a new folder would appear in the folder being looked at, not this one"
        );
        assert!(!rows.contains(&Command::Paste), "for the same reason");
        assert!(rows.contains(&Command::Rename));
        assert!(rows.contains(&Command::Copy));
        assert!(rows.contains(&Command::Properties));
        assert!(
            rows_for(&[], THERE).is_empty(),
            "over the background of another column there is nothing left to offer"
        );
    }

    #[test]
    fn a_press_reports_where_it_landed() {
        let row = Target::Row {
            position: 7,
            in_current: false,
        };
        assert_eq!(row.position(), Some(7));
        assert!(!row.in_current());
        assert_eq!(Target::Background { in_current: true }.position(), None);
        assert!(Target::Background { in_current: true }.in_current());
    }

    #[test]
    fn paste_is_offered_wherever_the_command_would_work() {
        let ready = Scope {
            pastable: true,
            ..HERE
        };
        let cases = [
            (
                "over the background",
                rows_for(&[], HERE),
                rows_for(&[], ready),
            ),
            (
                "over one row",
                rows_for(&[folder("one")], HERE),
                rows_for(&[folder("one")], ready),
            ),
            (
                "over several rows",
                rows_for(&[folder("one"), file("note.txt")], HERE),
                rows_for(&[folder("one"), file("note.txt")], ready),
            ),
        ];
        for (where_, empty, ready) in cases {
            assert!(
                !empty.contains(&Command::Paste),
                "{where_}: nothing on the clipboard, nothing to offer"
            );
            assert!(
                ready.contains(&Command::Paste),
                "{where_}: the folder is a perfectly good target"
            );
            assert_eq!(
                ready.iter().filter(|row| **row == Command::Paste).count(),
                1,
                "{where_}: Paste should be offered once"
            );
        }
    }

    #[test]
    fn a_menu_is_drawn_in_sections_of_related_rows() {
        let menu = model(&rows_for(
            &[folder("one")],
            Scope {
                in_current: true,
                extracting: true,
                openable: false,
                pastable: true,
                starred: false,
                bookmarked: false,
            },
        ));
        assert_eq!(
            sections(&menu),
            vec![
                vec!["Open".to_string()],
                vec!["New Folder".to_string(), "Paste".to_string()],
                vec![
                    "Star".to_string(),
                    "Bookmark This Folder".to_string(),
                    "Rename".to_string(),
                    "Move to Trash".to_string(),
                    "Delete Permanently".to_string()
                ],
                vec!["Copy".to_string(), "Cut".to_string()],
                vec!["Extract to Folder".to_string(), "Properties".to_string()],
            ]
        );
    }

    #[test]
    fn a_targeted_menu_keeps_the_thing_each_row_means() {
        // The sidebar's menu: four rows about one bookmark. The labels are the easy
        // half — a menu that lost every target would still print four correct rows
        // and then rename the wrong bookmark, so the readback has to look at the
        // parameter too.
        let rows = [
            MenuRow {
                label: "Rename\u{2026}",
                action: "win.rename-bookmark",
                target: Some("file:///srv/work".into()),
            },
            MenuRow {
                label: "Remove",
                action: "win.remove-bookmark",
                target: Some("file:///srv/work".into()),
            },
            MenuRow {
                label: "Say Nothing",
                action: "win.nothing",
                target: None,
            },
        ];
        let menu = model_for(&rows);
        assert_eq!(
            sections(&menu),
            vec![vec![
                "Rename\u{2026}".to_string(),
                "Remove".to_string(),
                "Say Nothing".to_string()
            ]],
            "one section: these are three answers about one thing, not three kinds of command"
        );
        assert_eq!(
            targets(&menu),
            vec![
                (
                    "Rename\u{2026}".to_string(),
                    Some("file:///srv/work".to_string())
                ),
                ("Remove".to_string(), Some("file:///srv/work".to_string())),
                ("Say Nothing".to_string(), None),
            ],
            "the URI has to survive the trip into the model"
        );
    }

    #[test]
    fn a_menu_over_the_background_is_divided_the_same_way_as_any_other() {
        // Two groups rather than one, which is what the bookmark row changed. The
        // rows are not related to each other: one pair puts something into the
        // folder, the other says something about the folder, and the sections are
        // what draws the line between those two kinds of answer. A menu that
        // dumped all three together would read as three ways to add a file.
        assert_eq!(
            sections(&model(&rows_for(
                &[],
                Scope {
                    in_current: true,
                    extracting: false,
                    openable: false,
                    pastable: true,
                    starred: false,
                    bookmarked: false
                }
            ))),
            vec![
                vec!["New Folder".to_string(), "Paste".to_string()],
                vec!["Bookmark This Folder".to_string()]
            ]
        );
    }

    #[test]
    fn a_section_with_nothing_in_it_is_left_out() {
        let menu = model(&[Command::Copy, Command::Cut]);
        assert_eq!(menu.n_items(), 1, "an empty group should not be drawn");
        assert_eq!(
            sections(&menu),
            vec![vec!["Copy".to_string(), "Cut".to_string()]]
        );
    }

    #[test]
    fn a_menu_shows_the_rows_it_was_given_and_nothing_otherwise() {
        tk::ui(|| {
            let window = window();
            let popup = Popup::install(&window);
            assert!(popup.rows().is_empty(), "a fresh menu holds nothing");
            popup.show(&window, &[], None);
            assert!(
                popup.rows().is_empty(),
                "no rows means no menu, not an empty box"
            );
            popup.show(&window, &rows_for(&[], HERE), None);
            assert_eq!(
                popup.rows(),
                vec![
                    vec!["New Folder".to_string()],
                    vec!["Bookmark This Folder".to_string()]
                ]
            );
            popup.hide();
            assert!(!popup.is_visible(), "hide should close it");
        });
    }

    #[test]
    fn a_cell_carries_the_gesture_that_will_open_the_menu() {
        tk::ui(|| {
            let cell = gtk::Box::new(gtk::Orientation::Horizontal, 0);
            let item = glib::Object::new::<gtk::ListItem>();
            item.set_child(Some(&cell));
            assert_eq!(
                cell.observe_controllers().n_items(),
                0,
                "a fresh cell has no controllers"
            );
            attach(&cell, &item, true, &Listeners::new().handler());
            let gesture = only_gesture(&cell);
            assert_eq!(
                gesture.button().cast_signed(),
                3,
                "the right button, so a left-click still selects and activates"
            );
        });
    }

    #[test]
    fn a_press_on_a_cell_reports_the_row_it_landed_on() {
        tk::ui(|| {
            let cell = gtk::Box::new(gtk::Orientation::Horizontal, 0);
            let item = glib::Object::new::<gtk::ListItem>();
            item.set_child(Some(&cell));
            let seen = Rc::new(RefCell::new(Vec::new()));
            let watch = Rc::clone(&seen);
            let listeners = Listeners::new();
            listeners.push(move |_widget, target, x, y| {
                watch.borrow_mut().push((target, x, y));
            });
            attach(&cell, &item, true, &listeners.handler());
            press(&only_gesture(&cell), 1, 12.0, 34.0);
            assert_eq!(
                *seen.borrow(),
                vec![(
                    Target::Row {
                        position: item.position(),
                        in_current: true
                    },
                    12.0,
                    34.0
                )],
                "the press should reach the listener with its coordinates"
            );
            press(&only_gesture(&cell), 2, 12.0, 34.0);
            assert_eq!(
                seen.borrow().len(),
                1,
                "a second click in a row is not a second menu"
            );
        });
    }

    #[test]
    fn a_press_on_an_empty_list_is_the_folder_menu() {
        tk::ui(|| {
            let list = gtk::ListView::new(
                None::<gtk::SingleSelection>,
                None::<gtk::SignalListItemFactory>,
            );
            let seen = Rc::new(RefCell::new(Vec::new()));
            let watch = Rc::clone(&seen);
            let listeners = Listeners::new();
            listeners.push(move |_widget, target, _x, _y| watch.borrow_mut().push(target));
            list.add_controller(background(&listeners.handler(), true));
            press(&only_gesture(&list), 1, 50.0, 50.0);
            assert_eq!(
                *seen.borrow(),
                vec![Target::Background { in_current: true }],
                "an unrealized list picks nothing under the press, which is the \
                 folder showing through"
            );
        });
    }

    #[test]
    fn a_press_reaches_every_handler_in_the_order_they_were_added() {
        tk::ui(|| {
            let cell = gtk::Box::new(gtk::Orientation::Horizontal, 0);
            let item = glib::Object::new::<gtk::ListItem>();
            item.set_child(Some(&cell));
            let seen = Rc::new(RefCell::new(Vec::new()));
            let listeners = Listeners::new();
            for index in 0..3 {
                let watch = Rc::clone(&seen);
                listeners.push(move |_widget, _target, _x, _y| watch.borrow_mut().push(index));
            }
            attach(&cell, &item, true, &listeners.handler());
            press(&only_gesture(&cell), 1, 1.0, 1.0);
            assert_eq!(*seen.borrow(), vec![0, 1, 2]);
        });
    }

    #[test]
    fn a_handler_added_during_a_press_waits_for_the_next_one() {
        tk::ui(|| {
            let cell = gtk::Box::new(gtk::Orientation::Horizontal, 0);
            let item = glib::Object::new::<gtk::ListItem>();
            item.set_child(Some(&cell));
            let seen = Rc::new(RefCell::new(Vec::new()));
            let listeners = Listeners::new();
            let late = listeners.clone();
            let watch = Rc::clone(&seen);
            listeners.push(move |_widget, _target, _x, _y| {
                watch.borrow_mut().push("first");
                let watch = Rc::clone(&watch);
                late.push(move |_widget, _target, _x, _y| watch.borrow_mut().push("late"));
            });
            attach(&cell, &item, true, &listeners.handler());
            press(&only_gesture(&cell), 1, 1.0, 1.0);
            assert_eq!(
                *seen.borrow(),
                vec!["first"],
                "subscribing mid-dispatch must not re-run the press"
            );
            press(&only_gesture(&cell), 1, 1.0, 1.0);
            assert_eq!(
                *seen.borrow(),
                vec!["first", "first", "late"],
                "and the new handler should see the next press"
            );
        });
    }

    #[test]
    fn installing_a_listener_is_not_a_press() {
        tk::ui(|| {
            let cell = gtk::Box::new(gtk::Orientation::Horizontal, 0);
            let item = glib::Object::new::<gtk::ListItem>();
            item.set_child(Some(&cell));
            let seen = Rc::new(RefCell::new(0u32));
            let watch = Rc::clone(&seen);
            let listeners = Listeners::new();
            listeners.push(move |_widget, _target, _x, _y| {
                *watch.borrow_mut() += 1;
            });
            attach(&cell, &item, true, &listeners.handler());
            assert_eq!(*seen.borrow(), 0, "installing listens hears nothing");
        });
    }
}
