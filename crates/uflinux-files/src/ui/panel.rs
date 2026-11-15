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

use ufm_core::config::PanelGroup;
use ufm_core::entry::human_size;
use ufm_core::paths;
use ufm_core::sort::SortKey;

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
    Trash,
    /// A bookmark or a folder the user pinned.
    Folder(PathBuf),
    /// A network share from the mount table. `label` is what to show, because
    /// the slug is not something a person wrote.
    Mount {
        id: String,
        label: String,
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
            Self::Trash => "Trash".to_string(),
            Self::Folder(path) => path.file_name().map_or_else(
                || path.display().to_string(),
                |n| n.to_string_lossy().into_owned(),
            ),
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
            Self::Trash => "user-trash",
            Self::Folder(_) => "folder",
            Self::Mount { .. } => "network-server",
        }
    }

    /// Which heading this row belongs under. Rows are grouped in the order the
    /// caller passes them, so this only decides where a heading is drawn.
    #[must_use]
    pub const fn group(&self) -> PanelGroup {
        match self {
            Self::Home
            | Self::Desktop
            | Self::Documents
            | Self::Downloads
            | Self::Music
            | Self::Pictures
            | Self::Videos
            | Self::Folder(_) => PanelGroup::Folders,
            Self::Starred | Self::Recent | Self::Trash | Self::Computer => PanelGroup::System,
            Self::Mount { .. } => PanelGroup::Network,
        }
    }

    /// The XDG folders plus the virtual ones, in the order they should appear.
    /// A folder that does not exist is left out rather than shown greyed, which
    /// is what a user with no `~/Templates` expects.
    #[must_use]
    pub fn standard() -> Vec<Self> {
        let home = paths::home();
        let exists = |name: &str, place: Self| home.join(name).is_dir().then_some(place);
        let mut places = vec![Self::Home];
        places.extend(
            [
                exists("Desktop", Self::Desktop),
                exists("Documents", Self::Documents),
                exists("Downloads", Self::Downloads),
                exists("Music", Self::Music),
                exists("Pictures", Self::Pictures),
                exists("Videos", Self::Videos),
            ]
            .into_iter()
            .flatten(),
        );
        places.push(Self::Computer);
        places.push(Self::Recent);
        places.push(Self::Starred);
        places.push(Self::Trash);
        places
    }

    /// Where clicking the row goes.
    #[must_use]
    pub fn location(&self) -> ufm_core::Location {
        match self {
            Self::Home => ufm_core::Location::file(paths::home()),
            Self::Desktop => ufm_core::Location::file(paths::desktop_dir()),
            Self::Computer => ufm_core::Location::computer(),
            // The `//` is not decoration: `Location::parse` only accepts a bare
            // absolute path or a `scheme://rest` URI, so a bare `recent:` is an
            // error rather than the Recent folder.
            Self::Recent => ufm_core::Location::parse("recent://").expect("recent is a location"),
            Self::Starred => {
                ufm_core::Location::parse("starred://").expect("starred is a location")
            }
            Self::Trash => ufm_core::Location::file(paths::trash_dir()),
            Self::Folder(path) => ufm_core::Location::file(path.clone()),
            Self::Mount { id, .. } => ufm_core::Location::file(paths::mount_root().join(id)),
            other => ufm_core::Location::file(paths::home().join(other.label())),
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

/// What the click handler needs. Kept behind an `Rc` because a signal handler
/// must own everything it touches: borrowing the panel itself would tie the
/// handler to a borrow that outlives the method that installed it.
#[derive(Default)]
struct Shared {
    places: RefCell<Vec<Place>>,
    on_open: RefCell<Option<Opened>>,
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
            if last != Some(group) {
                self.list.append(&heading(group));
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
        row
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
                label: "nas/filmy".into()
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
            Place::Mount {
                id: "x".into(),
                label: "x".into(),
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
