//! The sidebar, and the My-Computer panel that shares it.
//!
//! One [`gtk::ListBox`] holds both. The places are what a file manager has
//! always listed — home, the XDG folders, the trash — and the computer panel is
//! the same list extended with a row per filesystem that shows how full it is,
//! which is the part `nautilus-my-computer` got right and Nautilus itself never
//! did. Usage is passed in rather than probed here: the caller already has
//! [`ufm_vfs::FsHints`] from the last listing, and `statvfs` twice for one
//! folder is one `statvfs` too many.
//!
//! The rows are rebuilt wholesale when the places change. A sidebar is a few
//! dozen rows, so a diff would cost more in code than the rebuild costs in
//! milliseconds, and it keeps the widget free of a model of its own.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;

use gtk::prelude::*;

use ufm_core::Location;
use ufm_core::config::{Bookmark, PanelGroup};
use ufm_core::entry::human_size;
use ufm_core::paths;
use ufm_core::sort::SortKey;
use ufm_mount::monitor::MountState;

use crate::ui::popup::MenuRow;

/// A place the panel can offer to take the user.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Place {
    Home,
    Desktop,
    Documents,
    Downloads,
    Music,
    Pictures,
    Videos,
    /// Everything the user starred, gathered in one folder.
    Starred,
    /// Recently touched files, newest first.
    Recent,
    /// The computer panel: mounts, usage, and what is stale.
    Computer,
    Network,
    Trash,
    /// A folder the user pinned that is not one of the bookmarks below — kept
    /// for a caller that has a path and no record behind it.
    Folder(PathBuf),
    /// A bookmark, shown under its own name. The record rather than a path, so
    /// a bookmark the user renamed says the name they gave it and not the last
    /// component of where it points.
    Bookmark(Bookmark),
    /// A network share from the mount table. `label` is what to show, because
    /// the slug is not something a person wrote. `state` is the kernel's answer
    /// about the mount point and `can_reconnect` the store's answer about the
    /// password, both as of the moment the rows were built. They belong to the
    /// row's identity rather than riding beside it: the row's menu is read off
    /// them, so a share that went stale or lost its password has to become a
    /// different row, not the same row offering different rows in its menu.
    /// `autostart` is the third such answer, for the same reason: it decides
    /// whether the menu says "Connect at Login" or "Do Not Connect at Login", and
    /// a row that got it wrong would offer the user the row they just pressed.
    Mount {
        id: String,
        label: String,
        state: MountState,
        can_reconnect: bool,
        autostart: bool,
    },
}

impl Place {
    /// What the row says.
    #[must_use]
    pub fn label(&self) -> String {
        match self {
            Self::Home => "Home".to_string(),
            Self::Desktop => "Desktop".to_string(),
            Self::Documents => "Documents".to_string(),
            Self::Downloads => "Downloads".to_string(),
            Self::Music => "Music".to_string(),
            Self::Pictures => "Pictures".to_string(),
            Self::Videos => "Videos".to_string(),
            Self::Starred => "Starred".to_string(),
            Self::Recent => "Recent".to_string(),
            Self::Computer => "Computer".to_string(),
            Self::Network => "Network".to_string(),
            Self::Trash => "Trash".to_string(),
            Self::Folder(path) => path.file_name().map_or_else(
                || path.display().to_string(),
                |n| n.to_string_lossy().into_owned(),
            ),
            Self::Bookmark(bookmark) => bookmark.display(),
            Self::Mount { label, .. } => label.clone(),
        }
    }

    #[must_use]
    pub fn icon_name(&self) -> &'static str {
        match self {
            Self::Home => "user-home",
            Self::Desktop => "user-desktop",
            Self::Documents => "folder-documents",
            Self::Downloads => "folder-download",
            Self::Music => "folder-music",
            Self::Pictures => "folder-pictures",
            Self::Videos => "folder-videos",
            Self::Starred => "starred",
            Self::Recent => "document-open-recent",
            Self::Computer => "computer",
            Self::Network => "network-workgroup-symbolic",
            Self::Trash => "user-trash",
            // The themed folder for a pinned folder and for a bookmark, whatever a
            // bookmark points at. The record has an `icon` field for a name a user
            // chose, and a name that turns out to be missing renders as the
            // fallback image GTK draws for it — which in a sidebar of fifteen rows
            // is a blank square rather than a hint about what the row is.
            Self::Folder(_) | Self::Bookmark(_) => "folder",
            Self::Mount { .. } => "network-server",
        }
    }

    /// Which heading this row belongs under. Rows are grouped in the order the
    /// caller passes them, so this only decides where a heading is drawn.
    #[must_use]
    pub const fn group(&self) -> PanelGroup {
        match self {
            Self::Desktop
            | Self::Documents
            | Self::Downloads
            | Self::Music
            | Self::Pictures
            | Self::Videos
            | Self::Folder(_)
            | Self::Bookmark(_) => PanelGroup::Folders,
            Self::Home
            | Self::Starred
            | Self::Recent
            | Self::Trash
            | Self::Computer
            | Self::Network => PanelGroup::System,
            Self::Mount { .. } => PanelGroup::Network,
        }
    }

    /// The XDG folders plus the virtual ones, in the order they should appear.
    /// A folder that does not exist is left out rather than shown greyed, which
    /// is what a user with no `~/Templates` expects.
    #[must_use]
    pub fn standard() -> Vec<Self> {
        let exists = |key: &str, name: &str, place: Self| {
            paths::user_dir(key, name).is_dir().then_some(place)
        };
        let mut places = vec![
            Self::Computer,
            Self::Home,
            Self::Recent,
            Self::Starred,
            Self::Network,
            Self::Trash,
        ];
        places.extend(
            [
                paths::desktop_dir().is_dir().then_some(Self::Desktop),
                exists("XDG_DOCUMENTS_DIR", "Documents", Self::Documents),
                exists("XDG_DOWNLOAD_DIR", "Downloads", Self::Downloads),
                exists("XDG_MUSIC_DIR", "Music", Self::Music),
                exists("XDG_PICTURES_DIR", "Pictures", Self::Pictures),
                exists("XDG_VIDEOS_DIR", "Videos", Self::Videos),
            ]
            .into_iter()
            .flatten(),
        );
        places
    }

    /// The standard places with the user's bookmarks spliced into the Folders
    /// group, in the order the user put them.
    ///
    /// The splice point matters more than it looks. A heading is drawn wherever
    /// the group changes, so bookmarks appended after the standard list would
    /// open a second `Folders` heading below `Trash` — a sidebar that says the
    /// same word twice and files the user under it. Hence before the first row
    /// that is not a folder, which is where a person looking for one looks.
    #[must_use]
    pub fn with_bookmarks(bookmarks: &[Bookmark]) -> Vec<Self> {
        let mut places = Self::standard();
        places.extend(bookmarks.iter().cloned().map(Self::Bookmark));
        places
    }

    /// Where clicking the row goes.
    #[must_use]
    pub fn location(&self) -> Location {
        match self {
            Self::Home => Location::file(paths::home()),
            Self::Desktop => Location::file(paths::desktop_dir()),
            Self::Documents => Location::file(paths::user_dir("XDG_DOCUMENTS_DIR", "Documents")),
            Self::Downloads => Location::file(paths::user_dir("XDG_DOWNLOAD_DIR", "Downloads")),
            Self::Music => Location::file(paths::user_dir("XDG_MUSIC_DIR", "Music")),
            Self::Pictures => Location::file(paths::user_dir("XDG_PICTURES_DIR", "Pictures")),
            Self::Videos => Location::file(paths::user_dir("XDG_VIDEOS_DIR", "Videos")),
            Self::Computer => Location::computer(),
            Self::Network => Location::parse("network://").expect("network is a location"),
            // The `//` is not decoration: `Location::parse` only accepts a bare
            // absolute path or a `scheme://rest` URI, so a bare `recent:` is an
            // error rather than the Recent folder.
            Self::Recent => Location::parse("recent://").expect("recent is a location"),
            Self::Starred => Location::parse("starred://").expect("starred is a location"),
            Self::Trash => Location::file(paths::trash_dir()),
            Self::Folder(path) => Location::file(path.clone()),
            Self::Bookmark(bookmark) => bookmark.location.clone(),
            Self::Mount { id, .. } => Location::file(paths::mount_root().join(id)),
        }
    }
}

/// The heading drawn above a group of rows. `PanelGroup` has no `label` because
/// the preference file only needs its name; the words shown to a person live
/// here, beside the widget that shows them.
fn group_title(group: PanelGroup) -> &'static str {
    match group {
        PanelGroup::Folders => "Folders",
        PanelGroup::System => "My Computer",
        PanelGroup::Local => "On This Computer",
        PanelGroup::Removable => "Removable",
        PanelGroup::Disc => "Discs",
        PanelGroup::Network => "Network",
    }
}

/// How full one filesystem is, as shown under a row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Usage {
    /// The place this bar belongs to.
    pub place: Place,
    pub total: Option<u64>,
    pub free: Option<u64>,
}

impl Usage {
    /// Bytes used, as a fraction of the whole. `None` when the filesystem
    /// reports no size, which is the answer for a network share and for a FUSE
    /// mount whose daemon does not implement `statfs`.
    ///
    /// In `f64` throughout: the ratio of two 64-bit byte counts is what is
    /// wanted, and routing it through `u32` would report every filesystem over
    /// 4 GiB as exactly full.
    #[must_use]
    pub fn fraction(&self) -> Option<f64> {
        let total = self.total.filter(|t| *t > 0)?;
        let free = self.free.unwrap_or(0).min(total);
        Some((total - free) as f64 / total as f64)
    }

    /// The line under the bar: how much is free, and of how much.
    #[must_use]
    pub fn detail(&self) -> String {
        match (self.total, self.free) {
            (Some(total), Some(free)) => {
                format!("{} free of {}", human_size(free), human_size(total))
            }
            (Some(total), None) => human_size(total),
            (None, Some(free)) => format!("{free} bytes free"),
            (None, None) => String::new(),
        }
    }
}

type Opened = Rc<dyn Fn(&Place)>;

/// What the right-click handler needs. The place, the widget the press landed on
/// and the coordinates inside it — the same three the view's menu is built from,
/// so the window can point the two menus at the same spot.
type Context = Rc<dyn Fn(&Place, &gtk::Widget, f64, f64)>;

/// The menu for one sidebar row.
///
/// The rows are [`MenuRow`]s rather than [`Command`]s because each one names the
/// thing it acts on: a bookmark's menu targets a URI and a mount's targets a
/// record id, never an index, since an index is a fact about a rendering that the
/// next rebuild may have changed and a menu can stay open across a rebuild.
///
/// Two kinds of row have anything to say about themselves. A bookmark, because
/// the user named it and put it there and can un-name it. A mount, because the
/// share is a record the user made and because the answer to "what could be done
/// with this?" depends on the kernel's state — which is why [`Place::Mount`] car-
/// ries a [`MountState`] and why the arms below read it. The standard places are
/// derived from the filesystem, so renaming `Documents` would be a name with
/// nothing behind it, and they get no menu. A row with no rows is no menu at all
/// rather than a popover with nothing in it.
///
/// Each state is offered what it can actually do, and no more: a `Reconnect` on a
/// share that is already connected, or a `Disconnect` on one that was never
/// mounted, both fail with an honest but useless complaint, and a `Reconnect` for
/// a share whose password was never saved would fail before reaching the network.
/// The gate is the menu rather than the actions, which stay enabled always:
/// between opening a menu and pressing a row the state can change underneath, and
/// the answer to acting on a share that no longer wants that action is a banner
/// that says so, which is the same answer the card's buttons give.
#[must_use]
pub fn menu_for(place: &Place) -> Vec<MenuRow> {
    match place {
        Place::Bookmark(bookmark) => {
            let uri = bookmark.location.to_string();
            vec![
                MenuRow {
                    label: "Rename\u{2026}",
                    action: "win.rename-bookmark",
                    target: Some(uri.clone()),
                },
                // The two directions before the destructive row, because they are
                // the reversible ones and because a menu that files `Remove` in
                // the middle invites the mistake it is there to allow.
                MenuRow {
                    label: "Move Up",
                    action: "win.bookmark-up",
                    target: Some(uri.clone()),
                },
                MenuRow {
                    label: "Move Down",
                    action: "win.bookmark-down",
                    target: Some(uri.clone()),
                },
                MenuRow {
                    label: "Remove",
                    action: "win.remove-bookmark",
                    target: Some(uri),
                },
            ]
        }
        Place::Mount {
            id,
            can_reconnect,
            state,
            autostart,
            ..
        } => {
            let row = |label, action| MenuRow {
                label,
                action,
                target: Some(id.clone()),
            };
            let forget = || row("Forget", "win.forget-share");
            // The one row in this menu about the next session rather than this
            // one, which is why it appears in every state: a share that is dead,
            // or not mounted, or connected right now is a share the user may
            // equally want mounted tomorrow. It reads as a label that flips rather
            // than a ticked box because every other row here is a command, and a
            // single checked row among plain ones reads as a setting for the menu
            // rather than for the share.
            let login = if *autostart {
                row("Do Not Connect at Login", "win.autostart-share")
            } else {
                row("Connect at Login", "win.autostart-share")
            };
            match state {
                // Connected: only letting go. Reconnect would be a no-op and
                // Forget would strand a live mount, so neither is offered —
                // disconnect first, from here or from the card.
                MountState::Mounted => vec![row("Disconnect", "win.disconnect"), login],
                // The point exists and is dead, so the useful row is the one that
                // detaches it; `mount` over a dead endpoint cannot succeed, which
                // is why there is no Reconnect here even though the share is
                // anything but connected. Detaching a stale point is what
                // Disconnect does, and what the sweep does on a timer.
                MountState::Stale => {
                    vec![row("Disconnect", "win.disconnect"), login, forget()]
                }
                // Nothing mounted and nothing in the way: mount it again, or stop
                // listing it. No Disconnect row, because there is nothing to
                // detach. A share that would have to be asked for a password has
                // no Reconnect row either — reconnecting is deliberately the row
                // that costs one click, and a share needing an answer is a share
                // to open the Connect dialog for, where the answer can be given.
                // The login row is offered anyway, because `--connect-at-login`
                // refuses such a share and says why in its report: the row is
                // honest about a preference, where a Reconnect row would promise a
                // mount.
                MountState::Absent => {
                    let mut rows = Vec::new();
                    if *can_reconnect {
                        rows.push(row("Reconnect", "win.reconnect"));
                    }
                    rows.push(login);
                    rows.push(forget());
                    rows
                }
            }
        }
        _ => Vec::new(),
    }
}

/// What the click handler needs. Kept behind an `Rc` because a signal handler
/// must own everything it touches: borrowing the panel itself would tie the
/// handler to a borrow that outlives the method that installed it.
#[derive(Default)]
struct Shared {
    places: RefCell<Vec<Place>>,
    on_open: RefCell<Option<Opened>>,
    on_context: RefCell<Option<Context>>,
}

/// The sidebar.
pub struct Panel {
    widget: gtk::ScrolledWindow,
    list: gtk::ListBox,
    shared: Rc<Shared>,
    usage: RefCell<HashMap<Place, Usage>>,
    show_hidden: bool,
    sort: SortKey,
}

impl Panel {
    #[must_use]
    pub fn new() -> Self {
        let list = gtk::ListBox::new();
        list.add_css_class("ufm-sidebar");
        list.set_selection_mode(gtk::SelectionMode::Browse);
        list.set_show_separators(false);

        let widget = gtk::ScrolledWindow::new();
        widget.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
        widget.set_min_content_width(180);
        widget.set_max_content_width(320);
        widget.set_child(Some(&list));

        let panel = Self {
            widget,
            list,
            shared: Rc::new(Shared::default()),
            usage: RefCell::new(HashMap::new()),
            show_hidden: false,
            sort: SortKey::default(),
        };
        panel.wire();
        panel
    }

    #[must_use]
    pub fn widget(&self) -> &gtk::ScrolledWindow {
        &self.widget
    }

    /// React to the user choosing a place.
    pub fn on_open<F: Fn(&Place) + 'static>(&self, f: F) {
        *self.shared.on_open.borrow_mut() = Some(Rc::new(f));
    }

    /// React to a right-click on a place's row. The window owns the menu; the
    /// panel only says which row was pressed and where the pointer was.
    pub fn on_context<F: Fn(&Place, &gtk::Widget, f64, f64) + 'static>(&self, f: F) {
        *self.shared.on_context.borrow_mut() = Some(Rc::new(f));
    }

    /// Rebuild the list. The order is the caller's, so bookmarks stay where the
    /// user put them; a heading is inserted wherever the group changes.
    pub fn set_places(&self, places: Vec<Place>) {
        self.list.unselect_all();
        for child in child_widgets(&self.list) {
            self.list.remove(&child);
        }
        let mut last: Option<PanelGroup> = None;
        for place in &places {
            let group = place.group();
            if last.is_some() && last != Some(group) {
                let separator = gtk::Separator::new(gtk::Orientation::Horizontal);
                let row = gtk::ListBoxRow::new();
                row.set_selectable(false);
                row.set_activatable(false);
                row.set_child(Some(&separator));
                self.list.append(&row);
            }
            last = Some(group);
            self.list.append(&self.row(place));
        }
        *self.shared.places.borrow_mut() = places;
    }

    /// Attach usage bars to the rows they describe. A place with no row is
    /// ignored rather than added: the caller decides what the list contains.
    pub fn set_usage(&self, usage: Vec<Usage>) {
        let known: Vec<Place> = self.shared.places.borrow().iter().cloned().collect();
        let mut map = HashMap::new();
        for entry in usage {
            if known.contains(&entry.place) {
                map.insert(entry.place.clone(), entry);
            }
        }
        *self.usage.borrow_mut() = map;
    }

    /// Highlight the row that matches, so the sidebar agrees with the view.
    pub fn select(&self, place: &Place) {
        let places = self.shared.places.borrow();
        match places
            .iter()
            .position(|p| p == place)
            .and_then(|index| place_row(&self.list, index))
        {
            Some(row) => self.list.select_row(Some(&row)),
            None => self.list.unselect_all(),
        }
    }

    /// Drop the highlight, for a folder that is nowhere in the list. Keeping the
    /// old row lit would suggest the sidebar and the view disagree.
    pub fn clear_selection(&self) {
        self.list.unselect_all();
    }

    #[must_use]
    pub fn sort_key(&self) -> SortKey {
        self.sort
    }

    #[must_use]
    pub fn hides_hidden(&self) -> bool {
        !self.show_hidden
    }

    fn row(&self, place: &Place) -> gtk::ListBoxRow {
        let box_ = gtk::Box::new(gtk::Orientation::Vertical, 2);
        let line = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let image = gtk::Image::from_icon_name(place.icon_name());
        image.set_pixel_size(16);
        let label = gtk::Label::new(Some(&place.label()));
        label.set_ellipsize(pango_ellipsize());
        label.set_xalign(0.0);
        label.set_hexpand(true);
        line.append(&image);
        line.append(&label);
        box_.append(&line);

        if let Some(usage) = self.usage.borrow().get(place) {
            box_.append(&usage_bar(usage));
        }

        let row = gtk::ListBoxRow::new();
        // The class goes on the row, not the box inside it: the stylesheet
        // styles `row.ufm-place`, and the code that counts place rows looks at
        // rows. A class on the box would be invisible to both.
        row.add_css_class("ufm-place");
        row.set_child(Some(&box_));
        row.set_activatable(true);
        // The row is what a screen reader reaches and activates, so the
        // place's name has to live on the row itself. A name carried only by
        // the label inside leaves the list item anonymous: assistive tech
        // announces "list item" with nothing to say, and an AT client
        // activating the row activates the label, which does nothing.
        let name = place.label();
        row.update_property(&[gtk::accessible::Property::Label(&name)]);
        row.add_controller(self.context_gesture(place));
        row
    }

    /// The right-click gesture for one row.
    ///
    /// The place travels with the gesture rather than being looked up by index,
    /// because the index is a fact about the current rebuild.
    ///
    /// The row is deliberately *not* selected on press, unlike a row in the view.
    /// A view's selection is the user's to move; the sidebar's is a rendering of
    /// which folder the window is standing in, repainted on every navigation. Lit
    /// up a row the window did not walk into and the two halves of the window
    /// disagree about where the user is — which is worse than a menu pointing at a
    /// row that is not lit, especially as the menu is already pointing at it.
    fn context_gesture(&self, place: &Place) -> gtk::GestureClick {
        let gesture = gtk::GestureClick::new();
        gesture.set_button(3);
        let shared = Rc::clone(&self.shared);
        let place = place.clone();
        gesture.connect_pressed(move |controller, n_press, x, y| {
            if n_press != 1 {
                return;
            }
            let Some(widget) = controller.widget() else {
                return;
            };
            let handler = shared.on_context.borrow().as_ref().map(Rc::clone);
            if let Some(handler) = handler {
                handler(&place, &widget, x, y);
            }
        });
        gesture
    }

    fn wire(&self) {
        let shared = Rc::clone(&self.shared);
        self.list.connect_row_activated(move |list, row| {
            let Some(index) = row_index(list, row) else {
                return;
            };
            let handler = shared.on_open.borrow().as_ref().map(Rc::clone);
            let place = shared.places.borrow().get(index).cloned();
            if let (Some(handler), Some(place)) = (handler, place) {
                handler(&place);
            }
        });
    }
}

impl Default for Panel {
    fn default() -> Self {
        Self::new()
    }
}

/// A group heading: bold, not selectable, not activatable.
fn heading(group: PanelGroup) -> gtk::ListBoxRow {
    let label = gtk::Label::new(Some(group_title(group)));
    label.add_css_class("ufm-sidebar-group");
    label.set_xalign(0.0);
    let row = gtk::ListBoxRow::new();
    row.set_child(Some(&label));
    row.set_activatable(false);
    row.set_selectable(false);
    row
}

/// The usage bar and its caption.
fn usage_bar(usage: &Usage) -> gtk::Box {
    let box_ = gtk::Box::new(gtk::Orientation::Vertical, 2);
    box_.add_css_class("ufm-computer");
    if let Some(fraction) = usage.fraction() {
        let bar = gtk::LevelBar::new();
        bar.set_min_value(0.0);
        bar.set_max_value(1.0);
        bar.set_value(fraction);
        bar.set_hexpand(true);
        bar.add_css_class("ufm-usage");
        box_.append(&bar);
    }
    let detail = usage.detail();
    if !detail.is_empty() {
        let label = gtk::Label::new(Some(&detail));
        label.add_css_class("ufm-meta");
        label.set_xalign(0.0);
        box_.append(&label);
    }
    box_
}

/// The direct children of a container, as widgets.
///
/// `observe_children` hands back a model whose item type is `GObject` — GTK
/// does not type it per container — so `iter::<Widget>` panics on the type
/// check and the objects have to be downcast one by one instead.
fn child_widgets(list: &gtk::ListBox) -> Vec<gtk::Widget> {
    list.observe_children()
        .snapshot()
        .into_iter()
        .filter_map(|object| object.downcast::<gtk::Widget>().ok())
        .collect()
}

/// The rows of a `ListBox`, headings included.
fn child_rows(list: &gtk::ListBox) -> Vec<gtk::ListBoxRow> {
    child_widgets(list)
        .into_iter()
        .filter_map(|widget| widget.downcast::<gtk::ListBoxRow>().ok())
        .collect()
}

/// The index of a row among the place rows, counting only rows the panel added.
/// Headings are rows too, so they have to be skipped — hence counting children
/// rather than trusting `row.index()`, which counts headings as well.
fn row_index(list: &gtk::ListBox, row: &gtk::ListBoxRow) -> Option<usize> {
    let mut places_seen = 0_usize;
    for child in child_rows(list) {
        if child.has_css_class("ufm-place") {
            if child == *row {
                return Some(places_seen);
            }
            places_seen += 1;
        }
    }
    None
}

/// The inverse of [`row_index`]: the row for a place by its index among the
/// places. The same reason the two exist separately: there is no arithmetic that
/// turns one index into the other, because the number of headings depends on how
/// the places are grouped.
fn place_row(list: &gtk::ListBox, index: usize) -> Option<gtk::ListBoxRow> {
    let mut places_seen = 0_usize;
    for child in child_rows(list) {
        if child.has_css_class("ufm-place") {
            if places_seen == index {
                return Some(child);
            }
            places_seen += 1;
        }
    }
    None
}

/// Kept in one place so the ellipsisation is the same everywhere in the panel.
fn pango_ellipsize() -> gtk::pango::EllipsizeMode {
    gtk::pango::EllipsizeMode::End
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::tk;

    #[test]
    fn a_place_names_itself_without_a_path() {
        assert_eq!(Place::Home.label(), "Home");
        assert_eq!(Place::Trash.label(), "Trash");
        assert_eq!(
            Place::Mount {
                id: "smb-nas-filmy".into(),
                label: "nas/filmy".into(),
                state: MountState::Mounted,
                can_reconnect: true,
                autostart: false,
            }
            .label(),
            "nas/filmy"
        );
    }

    #[test]
    fn a_pinned_folder_is_named_after_its_last_component() {
        let place = Place::Folder(PathBuf::from("/data/foto"));
        assert_eq!(place.label(), "foto");
        assert_eq!(Place::Folder(PathBuf::from("/")).label(), "/");
    }

    #[test]
    fn a_bookmark_row_says_the_name_the_user_gave_it() {
        let here = Location::parse("/data/foto").expect("a path is a location");
        let named = Place::Bookmark(Bookmark {
            location: here.clone(),
            label: Some("Fotky".into()),
            icon: None,
        });
        assert_eq!(named.label(), "Fotky");
        let plain = Place::Bookmark(Bookmark {
            location: here,
            label: None,
            icon: None,
        });
        assert_eq!(
            plain.label(),
            "foto",
            "a bookmark with no name of its own takes the folder's"
        );
        assert_eq!(plain.location(), Location::parse("/data/foto").unwrap());
    }

    #[test]
    fn bookmarks_go_inside_the_folders_group_and_not_below_it() {
        let bookmark = Bookmark {
            location: Location::parse("/srv/work").expect("a path is a location"),
            label: Some("Work".into()),
            icon: None,
        };
        let marked = Place::Bookmark(bookmark.clone());
        let places = Place::with_bookmarks(std::slice::from_ref(&bookmark));
        let at = places
            .iter()
            .position(|place| *place == marked)
            .expect("the bookmark should be a row");
        let computer = places
            .iter()
            .position(|place| *place == Place::Computer)
            .expect("Computer should still be a row");
        assert!(at > computer, "bookmarks follow the primary places");
        // The reason for the splice rather than an append: one heading per group.
        // Walking the groups and counting the runs is what `set_places` does when
        // it decides where to draw a heading, so a second run of folders here
        // would be a second `Folders` heading in the sidebar.
        let groups: Vec<PanelGroup> = places.iter().map(Place::group).collect();
        let mut runs = 0_usize;
        let mut previous: Option<PanelGroup> = None;
        for group in &groups {
            if previous != Some(*group) && *group == PanelGroup::Folders {
                runs += 1;
            }
            previous = Some(*group);
        }
        assert_eq!(runs, 1, "the folder rows should be one contiguous run");
    }

    #[test]
    fn a_bookmark_row_offers_a_menu_about_itself() {
        let bookmark = Bookmark {
            location: Location::parse("/srv/work").expect("a path is a location"),
            label: Some("Work".into()),
            icon: None,
        };
        let rows = menu_for(&Place::Bookmark(bookmark));
        let labels: Vec<&str> = rows.iter().map(|row| row.label).collect();
        assert_eq!(
            labels,
            ["Rename\u{2026}", "Move Up", "Move Down", "Remove"],
            "the reversible rows before the destructive one, in the order a person reads them"
        );
        for row in &rows {
            assert!(
                row.action.starts_with("win."),
                "{} points outside this window's actions: {}",
                row.label,
                row.action
            );
            // Every row names the bookmark rather than a position in the list, so
            // that a menu left open across a rebuild still edits the row it opened
            // over.
            assert_eq!(
                row.target.as_deref(),
                Some("file:///srv/work"),
                "{} should say which bookmark it means",
                row.label
            );
        }
    }

    #[test]
    fn a_row_that_cannot_answer_for_itself_gets_no_menu() {
        // No menu at all rather than an empty popover: these rows are derived
        // from the filesystem, so there is nothing about them to change.
        for place in [
            Place::Home,
            Place::Trash,
            Place::Starred,
            Place::Folder(PathBuf::from("/srv/work")),
        ] {
            assert!(
                menu_for(&place).is_empty(),
                "{} should not offer a menu",
                place.label()
            );
        }
    }

    #[test]
    fn a_mount_row_offers_only_what_its_state_allows() {
        let mount = |state, can_reconnect| Place::Mount {
            id: "smb-nas-filmy".into(),
            label: "nas/filmy".into(),
            state,
            can_reconnect,
            autostart: false,
        };
        let labels = |state, can_reconnect| -> Vec<&'static str> {
            menu_for(&mount(state, can_reconnect))
                .iter()
                .map(|row| row.label)
                .collect()
        };
        // Connected: let go, and say whether tomorrow repeats it. Reconnect would
        // be a no-op and Forget would strand a mount that is in use.
        assert_eq!(
            labels(MountState::Mounted, true),
            ["Disconnect", "Connect at Login"]
        );
        // Dead: detach it, or give up on it. Reconnecting over a dead endpoint
        // cannot work, so the row that would promise that is not there.
        assert_eq!(
            labels(MountState::Stale, true),
            ["Disconnect", "Connect at Login", "Forget"],
            "a stale point has to be detached before it can be mounted again"
        );
        // Never mounted, password on disk: mount it again or stop listing it.
        assert_eq!(
            labels(MountState::Absent, true),
            ["Reconnect", "Connect at Login", "Forget"]
        );
        // Never mounted and the password was not saved. Reconnecting would fail
        // before reaching the network, so the row is left out rather than shown
        // disabled — the Connect dialog is where that answer gets given. The login
        // row stays, because it promises a preference and not a mount: the session
        // run refuses such a share and reports the refusal.
        assert_eq!(
            labels(MountState::Absent, false),
            ["Connect at Login", "Forget"],
            "a share that would have to be asked is not offered a one-click mount"
        );
        for row in menu_for(&mount(MountState::Absent, true)) {
            assert!(
                row.action.starts_with("win."),
                "{} points outside this window's actions: {}",
                row.label,
                row.action
            );
            // The record id rather than a position in the list, so a menu left
            // open across a rebuild still acts on the share it opened over.
            assert_eq!(
                row.target.as_deref(),
                Some("smb-nas-filmy"),
                "{} should say which share it means",
                row.label
            );
        }
    }

    #[test]
    fn the_login_row_says_the_opposite_of_what_the_share_already_does() {
        // The row is the only one in this menu whose label is a state rather than a
        // command, so the two labels are the whole feedback that the press worked.
        // A row that always said "Connect at Login" would leave a user pressing it
        // repeatedly and seeing the same menu.
        let mount = |autostart| Place::Mount {
            id: "smb-nas-filmy".into(),
            label: "nas/filmy".into(),
            state: MountState::Mounted,
            can_reconnect: true,
            autostart,
        };
        let login = |autostart| {
            menu_for(&mount(autostart))
                .into_iter()
                .find(|row| row.action == "win.autostart-share")
                .expect("a mount row always offers the login row")
        };
        assert_eq!(login(false).label, "Connect at Login");
        assert_eq!(login(true).label, "Do Not Connect at Login");
        // One row, whichever way it reads: two would mean the menu was asking
        // which of two opposite things to do.
        assert_eq!(
            menu_for(&mount(true))
                .iter()
                .filter(|row| row.action == "win.autostart-share")
                .count(),
            1
        );
        assert_eq!(
            menu_for(&mount(false))
                .iter()
                .filter(|row| row.action == "win.autostart-share")
                .count(),
            1
        );
    }

    #[test]
    fn a_share_marked_for_login_is_a_different_row_than_the_same_share_unmarked() {
        // The panel matches a usage bar, a selection, and a menu to a row by
        // comparing `Place` values, so a field that changes what the menu offers
        // has to change the row's identity too — otherwise ticking the box would
        // leave the old row on screen with the old label.
        let place = |autostart| Place::Mount {
            id: "smb-nas-filmy".into(),
            label: "nas/filmy".into(),
            state: MountState::Mounted,
            can_reconnect: true,
            autostart,
        };
        assert_ne!(place(true), place(false));
        // And the name does not change with it, or the row would jump.
        assert_eq!(place(true).label(), place(false).label());
    }

    #[test]
    fn every_place_has_an_icon_and_a_group() {
        let places = vec![
            Place::Home,
            Place::Desktop,
            Place::Documents,
            Place::Downloads,
            Place::Music,
            Place::Pictures,
            Place::Videos,
            Place::Starred,
            Place::Recent,
            Place::Computer,
            Place::Trash,
            Place::Folder(PathBuf::from("/x")),
            Place::Bookmark(Bookmark {
                location: Location::parse("/x").expect("a path is a location"),
                label: None,
                icon: None,
            }),
            Place::Mount {
                id: "x".into(),
                label: "x".into(),
                state: MountState::Absent,
                can_reconnect: false,
                autostart: false,
            },
        ];
        for place in places {
            assert!(!place.icon_name().is_empty());
            let _ = place.group();
        }
    }

    #[test]
    fn mounts_are_offered_as_network_places() {
        let place = Place::Mount {
            id: "smb-nas-filmy".into(),
            label: "nas/filmy".into(),
            state: MountState::Mounted,
            can_reconnect: true,
            autostart: false,
        };
        assert_eq!(place.group(), PanelGroup::Network);
        assert!(place.location().is_real());
    }

    #[test]
    fn a_usage_bar_shows_used_over_total() {
        let usage = Usage {
            place: Place::Home,
            total: Some(1000),
            free: Some(250),
        };
        assert_eq!(usage.fraction(), Some(0.75));
        assert!(usage.detail().contains("of"));
    }

    #[test]
    fn a_filesystem_that_reports_no_size_gets_no_bar() {
        let usage = Usage {
            place: Place::Computer,
            total: None,
            free: None,
        };
        assert_eq!(usage.fraction(), None);
        assert!(usage.detail().is_empty());
    }

    #[test]
    fn free_never_exceeds_total() {
        let usage = Usage {
            place: Place::Home,
            total: Some(100),
            free: Some(400),
        };
        assert_eq!(usage.fraction(), Some(0.0));
    }

    #[test]
    fn an_empty_filesystem_counts_as_zero_used_not_nan() {
        let usage = Usage {
            place: Place::Home,
            total: Some(0),
            free: Some(0),
        };
        assert_eq!(usage.fraction(), None);
    }

    #[test]
    fn the_panel_builds_its_rows() {
        tk::ui(|| {
            let panel = Panel::new();
            panel.set_places(vec![Place::Home, Place::Computer, Place::Trash]);
            let places = child_widgets(&panel.list)
                .iter()
                .filter(|child| child.has_css_class("ufm-place"))
                .count();
            assert_eq!(places, 3, "one row per place, headings aside");
        });
    }

    #[test]
    fn choosing_a_row_reports_the_place_at_that_position() {
        tk::ui(|| {
            let panel = Panel::new();
            panel.set_places(vec![Place::Home, Place::Trash]);
            let row = child_rows(&panel.list)
                .into_iter()
                .find(|row| row.has_css_class("ufm-place"))
                .expect("a place row");
            assert_eq!(row_index(&panel.list, &row), Some(0));
        });
    }
}
