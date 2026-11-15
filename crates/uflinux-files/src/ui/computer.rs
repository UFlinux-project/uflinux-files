//! The My Computer view: the places that are machines and shares rather than folders.
//!
//! `computer://` is not a directory. What is behind it is a set of capacities —
//! the home folder, the system drive, a desktop folder that may or may not exist,
//! and whatever the user has mounted — and a directory listing has no way to say
//! "46.2 GiB free of 232 GiB". So this is a presentation beside grid, list and
//! columns rather than another reader: the third child of the window's mode stack,
//! fed by [`cards`], which assembles plain data from the mount table and the
//! filesystem capabilities the window already keeps.
//!
//! The data is read through closures rather than by reaching into the window for
//! two reasons that point the same way. The window's state sits behind a `RefCell`
//! that a card builder would have to borrow while painting, and the interesting
//! rules — which places appear, what a share whose kernel entry has gone missing
//! says about itself — are exactly the rules worth testing without a display.

use std::cell::Cell;
use std::cell::RefCell;
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use gtk::prelude::*;

use ufm_core::Location;
use ufm_core::entry::human_size;
use ufm_core::paths;
use ufm_mount::{MountRecord, MountState, Protocol, Share};
use ufm_vfs::FsHints;

/// Which card was activated, or which share a Disconnect button was pressed for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CardId {
    /// A folder on a local filesystem: Home, the system drive, the desktop folder.
    Local(PathBuf),
    /// A share in the mount table, by record id.
    Mount(String),
    Device {
        path: PathBuf,
        mountpoint: Option<PathBuf>,
    },
}

/// One card of the My Computer view.
///
/// Plain owned data, because the interesting part of this view is which cards
/// there are and what each says about itself, and neither depends on a widget
/// existing.
#[derive(Debug, Clone, PartialEq)]
pub struct Card {
    pub id: CardId,
    /// The card's name: "Home", "System", the label a share was given.
    pub label: String,
    /// The path under the name, so a card can be recognised by where it is.
    pub detail: String,
    pub icon: String,
    /// Bar fill. `None` draws no bar: a filesystem that reports no size gets a
    /// card rather than a bar stuck at zero.
    pub fraction: Option<f64>,
    /// "46.2 GiB free of 232 GiB", or the mount point when there is no size to show.
    pub usage: String,
    /// "Connected" / "Disconnected" / "Not connected" on a share, nothing elsewhere.
    pub state: Option<&'static str>,
    /// Whether activating the card goes somewhere. A desktop folder that does not
    /// exist is described but not walked into.
    pub can_open: bool,
    /// Whether the card carries a Disconnect button.
    pub can_disconnect: bool,
}

/// The cards, in the order they are drawn: home, the desktop folder when there is
/// one, the system drive, then the shares in the order the table lists them.
///
/// `records` is anything shaped like the mount table's contents, `state` answers
/// what the kernel says about one id, and `hints` reads one path. The window
/// passes its own runner and reader; a test passes a table it wrote out by hand.
pub fn cards<T, I, S, H>(records: I, mut state: S, mut hints: H) -> Vec<Card>
where
    T: Deref<Target = MountRecord>,
    I: IntoIterator<Item = T>,
    S: FnMut(&str) -> MountState,
    H: FnMut(&Path) -> FsHints,
{
    let mut cards = vec![local(
        paths::home(),
        "Home",
        "user-home-symbolic",
        &mut hints,
    )];
    let desktop = paths::desktop_dir();
    if desktop.is_dir() {
        cards.push(local(
            desktop,
            "Desktop",
            "user-desktop-symbolic",
            &mut hints,
        ));
    }
    cards.push(local(
        PathBuf::from("/"),
        "System",
        "drive-harddisk-symbolic",
        &mut hints,
    ));
    for record in records {
        let hints = hints(&record.mountpoint);
        let detail = record.mountpoint.display().to_string();
        let state = state(&record.id);
        let mut card = share_card(&record.share);
        card.id = CardId::Mount(record.id.clone());
        card.label = record.share.label();
        // The mount point stands in for a capacity line when the filesystem
        // reports no size at all, which is what a stale share looks like.
        card.usage = capacity(&hints).unwrap_or_else(|| detail.clone());
        card.detail = detail;
        card.fraction = hints.used_fraction();
        card.state = Some(state.label());
        // A share the kernel no longer has is still listed — the record is the
        // user's — but walking into its mount point would only produce an error,
        // so the card says so and leaves the Disconnect button to fix it.
        card.can_open = state.alive();
        cards.push(card);
    }
    cards
}

/// A card for a folder on a local filesystem.
fn local<H: FnMut(&Path) -> FsHints>(
    path: PathBuf,
    label: &str,
    icon: &str,
    hints: &mut H,
) -> Card {
    let detail = path.display().to_string();
    let hints = hints(&path);
    Card {
        usage: capacity(&hints).unwrap_or_else(|| detail.clone()),
        fraction: hints.used_fraction(),
        id: CardId::Local(path),
        label: label.to_string(),
        detail,
        icon: icon.to_string(),
        state: None,
        can_open: true,
        can_disconnect: false,
    }
}

/// The parts every share card shares: an icon for the protocol and a Disconnect
/// button, with everything else left for the caller to fill in.
fn share_card(share: &Share) -> Card {
    Card {
        id: CardId::Mount(String::new()),
        label: String::new(),
        detail: String::new(),
        icon: match share.protocol {
            Protocol::Smb => "network-workgroup-symbolic",
            Protocol::Nfs | Protocol::Sftp | Protocol::Ftp => "network-server-symbolic",
            Protocol::WebDav => "folder-remote-symbolic",
        }
        .to_string(),
        fraction: None,
        usage: String::new(),
        state: None,
        can_open: false,
        can_disconnect: true,
    }
}

/// The capacity line, or `None` when the filesystem reports no size at all.
fn capacity(hints: &FsHints) -> Option<String> {
    let total = hints.total?;
    let free = hints.free.unwrap_or(0).min(total);
    Some(format!(
        "{} free of {}",
        human_size(free),
        human_size(total)
    ))
}

/// What a card can ask the window to do. One `Rc` shared by the view and every
/// card in it, so repainting a flow does not rebuild the closures and a handler
/// cannot outlive the callbacks it was wired with.
struct Hooks {
    on_open: RefCell<Option<Open>>,
    on_unmount: RefCell<Option<Unmount>>,
    on_shortcut: RefCell<Option<Box<dyn Fn(Location)>>>,
    on_device: RefCell<Option<Box<dyn Fn(PathBuf, Option<PathBuf>)>>>,
    on_connect: RefCell<Option<Box<dyn Fn()>>>,
}

type Open = Box<dyn Fn(CardId)>;
type Unmount = Box<dyn Fn(String)>;

impl Hooks {
    fn open(&self, id: CardId) {
        if let Some(callback) = self.on_open.borrow().as_ref() {
            callback(id);
        }
    }

    fn unmount(&self, id: &str) {
        if let Some(callback) = self.on_unmount.borrow().as_ref() {
            callback(id.to_string());
        }
    }
}

/// The view: one card per place, in a flow that wraps to the window's width.
///
/// Built once and repainted by [`ComputerView::set_cards`], because the set
/// changes under a view that stays open — a share connects, a share is
/// disconnected, free space moves — and rebuilding the whole flow is cheaper than
/// tracking which card moved.
pub struct ComputerView {
    scroll: gtk::ScrolledWindow,
    network_only: bool,
    flow: gtk::FlowBox,
    local_flow: gtk::FlowBox,
    removable_flow: gtk::FlowBox,
    local_heading: gtk::Label,
    removable_heading: gtk::Label,
    count: Cell<usize>,
    hooks: Rc<Hooks>,
    base: RefCell<Vec<Card>>,
    devices: RefCell<Vec<Card>>,
}

/// The widget name every card carries, so a test can find a card without
/// depending on how a card happens to be laid out today.
const CARD: &str = "ufm-card";

/// The key a card's id is stored under.
const SLOT: &str = "ufm-card-id";

impl ComputerView {
    #[must_use]
    pub fn new() -> Self {
        Self::build_view(false)
    }

    pub fn network() -> Self {
        Self::build_view(true)
    }

    fn build_view(network_only: bool) -> Self {
        let flow = gtk::FlowBox::new();
        flow.set_homogeneous(true);
        flow.set_max_children_per_line(6);
        flow.set_min_children_per_line(1);
        flow.set_selection_mode(gtk::SelectionMode::None);
        flow.set_row_spacing(12);
        flow.set_column_spacing(12);
        flow.set_margin_top(12);
        flow.set_margin_bottom(12);
        flow.set_margin_start(12);
        flow.set_margin_end(12);

        let hooks = Rc::new(Hooks {
            on_open: RefCell::new(None),
            on_unmount: RefCell::new(None),
            on_shortcut: RefCell::new(None),
            on_device: RefCell::new(None),
            on_connect: RefCell::new(None),
        });
        // Connected once, for the life of the view, rather than on every repaint:
        // the handler reads the id out of the child it was handed, so it works for
        // cards that have not been built yet. Wiring it up in `set_cards` instead
        // would add a second handler on every repaint and fire `on_open` twice on
        // the second one.
        flow.connect_child_activated({
            let hooks = Rc::clone(&hooks);
            move |_flow, child| {
                if let Some(id) = card_id(child) {
                    hooks.open(id);
                }
            }
        });
        let local_flow = gtk::FlowBox::new();
        let removable_flow = gtk::FlowBox::new();
        for group in [&local_flow, &removable_flow] {
            group.set_selection_mode(gtk::SelectionMode::None);
            group.set_max_children_per_line(4);
            group.set_row_spacing(12);
            group.set_column_spacing(12);
            group.connect_child_activated({
                let hooks = Rc::clone(&hooks);
                move |_, child| {
                    if let Some(id) = card_id(child) {
                        hooks.open(id);
                    }
                }
            });
        }

        let page = gtk::Box::new(gtk::Orientation::Vertical, 18);
        page.set_margin_top(24);
        page.set_margin_start(24);
        page.set_margin_end(24);
        page.set_margin_bottom(24);
        let heading = gtk::Label::new(Some("Preferred Folders"));
        heading.add_css_class("heading");
        heading.set_xalign(0.0);
        page.append(&heading);
        let shortcuts = gtk::FlowBox::new();
        shortcuts.set_selection_mode(gtk::SelectionMode::None);
        shortcuts.set_max_children_per_line(9);
        shortcuts.set_column_spacing(8);
        shortcuts.set_row_spacing(8);
        for (name, icon, location) in [
            (
                "Home",
                "user-home-symbolic",
                Some(Location::file(paths::home())),
            ),
            (
                "Recent",
                "document-open-recent-symbolic",
                Location::parse("recent://").ok(),
            ),
            (
                "Starred",
                "starred-symbolic",
                Location::parse("starred://").ok(),
            ),
            (
                "Documents",
                "folder-documents-symbolic",
                Some(Location::file(paths::user_dir(
                    "XDG_DOCUMENTS_DIR",
                    "Documents",
                ))),
            ),
            (
                "Downloads",
                "folder-download-symbolic",
                Some(Location::file(paths::user_dir(
                    "XDG_DOWNLOAD_DIR",
                    "Downloads",
                ))),
            ),
            (
                "Music",
                "folder-music-symbolic",
                Some(Location::file(paths::user_dir("XDG_MUSIC_DIR", "Music"))),
            ),
            (
                "Videos",
                "folder-videos-symbolic",
                Some(Location::file(paths::user_dir("XDG_VIDEOS_DIR", "Videos"))),
            ),
            (
                "Pictures",
                "folder-pictures-symbolic",
                Some(Location::file(paths::user_dir(
                    "XDG_PICTURES_DIR",
                    "Pictures",
                ))),
            ),
        ] {
            let Some(location) = location else {
                continue;
            };
            if let Some(path) = location.as_path()
                && !path.is_dir()
            {
                continue;
            }
            let button = gtk::Button::new();
            button.add_css_class("ufm-shortcut");
            let box_ = gtk::Box::new(gtk::Orientation::Vertical, 6);
            let image = gtk::Image::from_icon_name(icon);
            image.set_pixel_size(48);
            box_.append(&image);
            box_.append(&gtk::Label::new(Some(name)));
            button.set_child(Some(&box_));
            let hooks = Rc::clone(&hooks);
            button.connect_clicked(move |_| {
                if let Some(callback) = hooks.on_shortcut.borrow().as_ref() {
                    callback(location.clone());
                }
            });
            shortcuts.append(&button);
        }
        let network = gtk::Button::new();
        network.add_css_class("ufm-shortcut");
        let network_body = gtk::Box::new(gtk::Orientation::Vertical, 6);
        let network_icon = gtk::Image::from_icon_name("network-workgroup-symbolic");
        network_icon.set_pixel_size(48);
        network_body.append(&network_icon);
        network_body.append(&gtk::Label::new(Some("Network")));
        network.set_child(Some(&network_body));
        network.connect_clicked({
            let hooks = Rc::clone(&hooks);
            move |_| {
                if let Some(callback) = hooks.on_shortcut.borrow().as_ref() {
                    callback(Location::parse("network://").expect("network location"));
                }
            }
        });
        shortcuts.append(&network);
        page.append(&shortcuts);
        heading.set_visible(!network_only);
        shortcuts.set_visible(!network_only);
        let connect = gtk::Button::with_label("Connect to Server…");
        connect.set_action_name(Some("win.connect"));
        connect.set_halign(gtk::Align::Start);
        connect.set_visible(network_only);
        page.append(&connect);
        let heading = gtk::Label::new(Some(if network_only { "Network" } else { "Storage" }));
        heading.add_css_class("heading");
        heading.set_xalign(0.0);
        page.append(&heading);
        page.append(&flow);
        let local_heading = gtk::Label::new(Some("On This Computer"));
        local_heading.add_css_class("heading");
        local_heading.set_xalign(0.0);
        page.append(&local_heading);
        page.append(&local_flow);
        let removable_heading = gtk::Label::new(Some("Removable"));
        removable_heading.add_css_class("heading");
        removable_heading.set_xalign(0.0);
        page.append(&removable_heading);
        page.append(&removable_flow);
        let scroll = gtk::ScrolledWindow::new();
        scroll.set_child(Some(&page));
        scroll.set_hexpand(true);
        scroll.set_vexpand(true);
        Self {
            scroll,
            network_only,
            flow,
            local_flow,
            removable_flow,
            local_heading,
            removable_heading,
            count: Cell::new(0),
            hooks,
            base: RefCell::new(Vec::new()),
            devices: RefCell::new(Vec::new()),
        }
    }

    #[must_use]
    pub fn widget(&self) -> &gtk::ScrolledWindow {
        &self.scroll
    }

    /// How many cards are on screen, for a status line that has to say something
    /// about a presentation with no rows of its own. Kept rather than counted out
    /// of the flow, because the flow exposes a child by index but not a total.
    #[must_use]
    pub fn card_count(&self) -> usize {
        self.count.get()
    }

    /// Replace the cards. The old ones go with the old closures: a card is a few
    /// labels and a bar, and the set changes only when something about the machine
    /// changed. The flow's own activation handler was connected in [`Self::new`]
    /// and stays, because it reads each card's id from the card itself.
    pub fn set_cards(&self, cards: &[Card]) {
        if self.base.borrow().as_slice() == cards {
            return;
        }
        *self.base.borrow_mut() = cards.to_vec();
        self.render();
    }

    pub fn set_devices(&self, cards: Vec<Card>) {
        if *self.devices.borrow() == cards {
            return;
        }
        *self.devices.borrow_mut() = cards;
        self.render();
    }

    fn render(&self) {
        self.flow.remove_all();
        self.local_flow.remove_all();
        self.removable_flow.remove_all();
        let base = self.base.borrow();
        let devices = self.devices.borrow();
        let shown = base.iter().filter(|card| {
            if self.network_only {
                matches!(card.id, CardId::Mount(_))
            } else {
                !matches!(card.id, CardId::Mount(_))
                    && !matches!(&card.id, CardId::Local(path) if path == &paths::home())
            }
        });
        let base_count = shown.clone().count();
        for card in shown {
            self.flow.append(&build(card, &self.hooks));
        }
        let mut locals = 0;
        let mut removable = 0;
        for card in devices.iter().filter(|_| !self.network_only) {
            if card.icon == "drive-removable-media-symbolic" {
                self.removable_flow.append(&build(card, &self.hooks));
                removable += 1;
            } else {
                self.local_flow.append(&build(card, &self.hooks));
                locals += 1;
            }
        }
        self.local_heading.set_visible(locals > 0);
        self.removable_heading.set_visible(removable > 0);
        self.count.set(base_count + locals + removable);
    }

    /// What happens when a card is activated. The window navigates.
    pub fn on_open(&self, f: impl Fn(CardId) + 'static) {
        *self.hooks.on_open.borrow_mut() = Some(Box::new(f));
    }

    /// What happens when a card's Disconnect is pressed. The window unmounts.
    pub fn on_unmount(&self, f: impl Fn(String) + 'static) {
        *self.hooks.on_unmount.borrow_mut() = Some(Box::new(f));
    }

    pub fn on_shortcut(&self, f: impl Fn(Location) + 'static) {
        *self.hooks.on_shortcut.borrow_mut() = Some(Box::new(f));
    }

    pub fn on_device(&self, f: impl Fn(PathBuf, Option<PathBuf>) + 'static) {
        *self.hooks.on_device.borrow_mut() = Some(Box::new(f));
    }

    pub fn on_connect(&self, f: impl Fn() + 'static) {
        *self.hooks.on_connect.borrow_mut() = Some(Box::new(f));
    }
}

impl Default for ComputerView {
    fn default() -> Self {
        Self::new()
    }
}

/// Which card a flow child stands for, or `None` for a card that leads nowhere.
fn card_id(child: &gtk::FlowBoxChild) -> Option<CardId> {
    let slot = unsafe { child.data::<RefCell<Option<CardId>>>(SLOT) }?;
    // SAFETY: written by `build` for this exact widget before it was added to the
    // flow, with this exact type, and read only here — on the main thread, from a
    // signal the flow emits while nothing else holds the borrow.
    unsafe { slot.as_ref() }.borrow().clone()
}

/// One card: icon, name, path, bar, and the share's state and button when it has them.
fn build(card: &Card, hooks: &Rc<Hooks>) -> gtk::FlowBoxChild {
    let body = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    body.add_css_class("ufm-computer");
    body.add_css_class("ufm-drive");
    let icon = gtk::Image::from_icon_name(&card.icon);
    icon.set_pixel_size(36);
    body.append(&icon);

    let lines = gtk::Box::new(gtk::Orientation::Vertical, 2);
    lines.set_hexpand(true);
    lines.append(&named(&card.label));
    lines.append(&meta(&card.detail, gtk::pango::EllipsizeMode::Middle));
    if let Some(fraction) = card.fraction {
        let bar = gtk::LevelBar::new();
        bar.add_css_class("ufm-usage");
        bar.set_value(fraction);
        lines.append(&bar);
    }
    lines.append(&meta(&card.usage, gtk::pango::EllipsizeMode::End));
    if let Some(state) = card.state {
        lines.append(&meta(state, gtk::pango::EllipsizeMode::None));
    }
    body.append(&lines);

    if let CardId::Mount(id) = &card.id
        && matches!(card.state, Some("Connected" | "Disconnected"))
    {
        let hooks = Rc::clone(hooks);
        let id = id.clone();
        let button = gtk::Button::with_label("Disconnect");
        button.set_widget_name("ufm-disconnect");
        button.connect_clicked(move |_| hooks.unmount(&id));
        body.append(&button);
    }
    if let CardId::Mount(id) = &card.id
        && card.state == Some("Not connected")
    {
        let actions = gtk::MenuButton::new();
        actions.set_icon_name("open-menu-symbolic");
        let menu = gio::Menu::new();
        menu.append(Some("Reconnect"), Some(&format!("win.reconnect('{}')", id)));
        menu.append(Some("Forget"), Some(&format!("win.forget-share('{}')", id)));
        actions.set_menu_model(Some(&menu));
        body.append(&actions);
    }
    if let CardId::Device { path, mountpoint } = &card.id
        && !matches!(card.state, Some("Locked" | "Unsupported"))
        && mountpoint
            .as_ref()
            .is_none_or(|p| p.starts_with(paths::mount_root().join("devices")))
    {
        let button = gtk::Button::with_label(if mountpoint.is_some() {
            "Eject"
        } else {
            "Mount"
        });
        let hooks = Rc::clone(hooks);
        let path = path.clone();
        let mountpoint = mountpoint.clone();
        button.connect_clicked(move |_| {
            if let Some(callback) = hooks.on_device.borrow().as_ref() {
                callback(path.clone(), mountpoint.clone());
            }
        });
        body.append(&button);
    }

    let child = gtk::FlowBoxChild::new();
    child.set_widget_name(CARD);
    child.set_child(Some(&body));
    child.set_tooltip_text(Some(&card.detail));
    child.set_sensitive(
        card.can_open
            || matches!(card.id, CardId::Mount(_))
            || (matches!(card.id, CardId::Device { .. })
                && !matches!(card.state, Some("Locked" | "Unsupported"))),
    );
    // Same reason as the sidebar rows: the flow child is the thing a screen
    // reader lands on and activates, and a card whose name lives only on the
    // label inside it is an anonymous list item. The label property is the
    // explicit name, so it wins over whatever a walk of the subtree would
    // have collected.
    child.update_property(&[gtk::accessible::Property::Label(&card.label)]);
    let slot = RefCell::new(card.can_open.then_some(card.id.clone()));
    unsafe { child.set_data::<RefCell<Option<CardId>>>(SLOT, slot) };
    child
}

/// The card's name, drawn as a row name is.
fn named(text: &str) -> gtk::Label {
    let label = gtk::Label::new(Some(text));
    label.set_xalign(0.0);
    label.add_css_class("ufm-name");
    label
}

/// A secondary line: the path, the capacity, the state.
fn meta(text: &str, ellipsize: gtk::pango::EllipsizeMode) -> gtk::Label {
    let label = gtk::Label::new(Some(text));
    label.set_xalign(0.0);
    label.set_ellipsize(ellipsize);
    label.add_css_class("ufm-meta");
    label
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tests that change the process environment have to hold this, because
    /// `set_var` is unsafe in edition 2024 precisely because another thread may
    /// be reading the environment at the same time.
    fn env() -> std::sync::MutexGuard<'static, ()> {
        super::super::test_env()
    }

    #[allow(unsafe_code)]
    fn set_env(key: &str, value: &str) {
        // SAFETY: the guard means no other test in this binary is reading the
        // environment while this happens.
        unsafe { std::env::set_var(key, value) };
    }

    #[allow(unsafe_code)]
    fn unset_env(key: &str) {
        // SAFETY: as above.
        unsafe { std::env::remove_var(key) };
    }

    fn record(uri: &str, root: &str) -> MountRecord {
        let share = Share::parse(uri).expect("a share URI").share;
        MountRecord {
            id: share.slug(),
            mountpoint: Path::new(root).join(share.slug()),
            share,
            autostart: false,
            mounted_at: None,
            last_used: None,
            last_error: None,
        }
    }

    /// A filesystem that reports a size: one MiB with three quarters of it used.
    fn full(_: &Path) -> FsHints {
        FsHints {
            total: Some(1_048_576),
            free: Some(262_144),
            ..FsHints::default()
        }
    }

    /// A filesystem that reports nothing at all, which is what a dead mount looks
    /// like from `statvfs`.
    fn empty(_: &Path) -> FsHints {
        FsHints::default()
    }

    fn labels(cards: &[Card]) -> Vec<&str> {
        cards.iter().map(|card| card.label.as_str()).collect()
    }

    #[test]
    fn the_three_local_places_are_always_there() {
        let _guard = env();
        unset_env("UFLINUX_FILES_DESKTOP_DIR");
        let cards = cards(Vec::<&MountRecord>::new(), |_| MountState::Mounted, empty);
        // Home first, the system drive next, and nothing else: a machine with no
        // shares still has a My Computer view worth showing.
        assert_eq!(labels(&cards), ["Home", "System"]);
        assert_eq!(cards[0].id, CardId::Local(paths::home()));
        assert_eq!(cards[1].id, CardId::Local(PathBuf::from("/")));
        assert!(cards.iter().all(|card| card.state.is_none()));
        assert!(cards.iter().all(|card| card.can_open));
        assert!(cards.iter().all(|card| !card.can_disconnect));
    }

    #[test]
    fn a_desktop_folder_becomes_a_card_and_an_absent_one_does_not() {
        let _guard = env();
        let dir = std::env::temp_dir().join("ufm-computer-desktop");
        let _ = std::fs::remove_dir_all(&dir);
        set_env("UFLINUX_FILES_DESKTOP_DIR", &dir.display().to_string());
        let without = cards(Vec::<&MountRecord>::new(), |_| MountState::Mounted, empty);
        std::fs::create_dir_all(&dir).unwrap();
        let with = cards(Vec::<&MountRecord>::new(), |_| MountState::Mounted, empty);
        std::fs::remove_dir_all(&dir).unwrap();
        unset_env("UFLINUX_FILES_DESKTOP_DIR");
        assert_eq!(labels(&without), ["Home", "System"]);
        // Between Home and System, which is where the sidebar keeps it too.
        assert_eq!(labels(&with), ["Home", "Desktop", "System"]);
        assert_eq!(with[1].icon, "user-desktop-symbolic");
    }

    #[test]
    fn a_connected_share_says_so_and_opens() {
        let _guard = env();
        unset_env("UFLINUX_FILES_DESKTOP_DIR");
        let record = record("smb://nas/filmy", "/run/user/1000/ufm");
        let cards = cards(vec![&record], |_| MountState::Mounted, full);
        assert_eq!(labels(&cards), ["Home", "System", "filmy"]);
        let card = &cards[2];
        // The id is the record's own, because that is what a walk-in and a
        // Disconnect both have to use to find this mount and not another. The
        // slug carries a hash of the full address, so only its readable part is
        // asserted here.
        let CardId::Mount(id) = &card.id else {
            panic!("a share is identified by its record, not its path")
        };
        assert!(
            id.starts_with("nas-filmy-"),
            "the slug names the share: {id}"
        );
        assert_eq!(card.detail, format!("/run/user/1000/ufm/{id}"));
        assert_eq!(card.state, Some("Connected"));
        assert!(card.can_open);
        assert!(card.can_disconnect);
        assert_eq!(card.icon, "network-workgroup-symbolic");
        assert_eq!(card.usage, "256 KiB free of 1.0 MiB");
        assert_eq!(card.fraction, Some(0.75));
    }

    #[test]
    fn a_share_the_kernel_lost_is_listed_but_not_walked_into() {
        let _guard = env();
        unset_env("UFLINUX_FILES_DESKTOP_DIR");
        let record = record("nfs://host/export", "/m");
        let cards = cards(vec![&record], |_| MountState::Stale, empty);
        let card = &cards[2];
        // The record is the user's, so it stays; the card says what is wrong and
        // refuses the walk rather than producing a transport error.
        assert_eq!(card.state, Some("Disconnected"));
        assert!(!card.can_open);
        assert!(card.can_disconnect, "the button is how it gets fixed");
        assert_eq!(card.fraction, None, "a dead mount reports no size");
        // With nothing to show, the mount point stands in for a capacity line.
        assert_eq!(card.usage, card.detail);
        assert!(card.usage.starts_with("/m/host-export-"), "{}", card.usage);
        assert_eq!(card.icon, "network-server-symbolic");
    }

    #[test]
    fn each_protocol_picks_its_own_icon() {
        let _guard = env();
        unset_env("UFLINUX_FILES_DESKTOP_DIR");
        let records = vec![
            record("sftp://host/home", "/m"),
            record("webdav://host/dav", "/m"),
        ];
        let cards = cards(&records, |_| MountState::Absent, empty);
        assert_eq!(cards[2].icon, "network-server-symbolic");
        assert_eq!(cards[3].icon, "folder-remote-symbolic");
        // Not connected is a different answer from disconnected: nothing has to be
        // detached, and walking in would fail the same way either way.
        assert_eq!(cards[2].state, Some("Not connected"));
        assert!(!cards[2].can_open);
    }

    #[test]
    fn a_share_label_beats_the_last_path_component() {
        let _guard = env();
        unset_env("UFLINUX_FILES_DESKTOP_DIR");
        let records = vec![record("smb://nas/filmy?label=Filmy%202024", "/m")];
        let cards = cards(&records, |_| MountState::Mounted, empty);
        assert_eq!(labels(&cards)[2], "Filmy 2024");
    }

    #[test]
    fn a_filesystem_with_no_size_gets_no_bar() {
        let _guard = env();
        unset_env("UFLINUX_FILES_DESKTOP_DIR");
        let cards = cards(Vec::<&MountRecord>::new(), |_| MountState::Mounted, empty);
        assert_eq!(cards[0].fraction, None);
        // The path is the only honest thing left to say under the name.
        assert_eq!(cards[0].usage, cards[0].detail);
    }

    #[test]
    fn cards_are_built_and_repainted() {
        super::super::tk::ui(|| {
            let view = ComputerView::new();
            assert_eq!(view.card_count(), 0);
            let card = Card {
                id: CardId::Local(PathBuf::from("/")),
                label: "System".into(),
                detail: "/".into(),
                icon: "drive-harddisk-symbolic".into(),
                fraction: Some(0.5),
                usage: "1 GiB free of 2 GiB".into(),
                state: None,
                can_open: true,
                can_disconnect: false,
            };
            view.set_cards(std::slice::from_ref(&card));
            assert_eq!(view.card_count(), 1);
            // Repainting replaces rather than accumulates: the set changes under a
            // view that stays open, and a card left behind would still be clickable.
            view.set_cards(&[card.clone(), card]);
            assert_eq!(view.card_count(), 2);
            view.set_cards(&[]);
            assert_eq!(view.card_count(), 0);
        });
    }

    #[test]
    fn activating_a_card_reaches_the_hook_exactly_once() {
        super::super::tk::ui(|| {
            use std::cell::RefCell;
            use std::rc::Rc;
            let view = ComputerView::new();
            let seen = Rc::new(RefCell::new(Vec::new()));
            view.on_open({
                let seen = Rc::clone(&seen);
                move |id| seen.borrow_mut().push(id)
            });
            // A stale share: described, but activating it would only produce an error.
            let dead = Card {
                id: CardId::Local(PathBuf::from("/")),
                label: "System".into(),
                detail: "/".into(),
                icon: "drive-harddisk-symbolic".into(),
                fraction: None,
                usage: "/".into(),
                state: None,
                can_open: false,
                can_disconnect: false,
            };
            let live = Card {
                id: CardId::Mount("nas-filmy".into()),
                label: "filmy".into(),
                detail: "/m/nas-filmy".into(),
                icon: "network-workgroup-symbolic".into(),
                fraction: None,
                usage: "/m/nas-filmy".into(),
                state: Some("Connected"),
                can_open: true,
                can_disconnect: true,
            };
            view.set_cards(&[dead.clone(), live.clone()]);
            // Repainting twice, which is what a mount and an unmount do to a window
            // that stays open. A handler connected per repaint would answer twice
            // here and the window would navigate twice.
            view.set_cards(&[dead, live]);
            let child = view.flow.child_at_index(1).expect("the second card");
            view.flow.emit_by_name::<()>("child-activated", &[&child]);
            assert_eq!(*seen.borrow(), [CardId::Mount("nas-filmy".into())]);
            // The insensitive card carries no id at all, so a stray activation on it
            // is ignored rather than opening something.
            let stale = view.flow.child_at_index(0).expect("the first card");
            view.flow.emit_by_name::<()>("child-activated", &[&stale]);
            assert_eq!(seen.borrow().len(), 1);
        });
    }

    #[test]
    fn disconnect_reaches_the_hook_with_the_records_id() {
        super::super::tk::ui(|| {
            use std::cell::RefCell;
            use std::rc::Rc;
            let view = ComputerView::new();
            let seen = Rc::new(RefCell::new(Vec::new()));
            view.on_unmount({
                let seen = Rc::clone(&seen);
                move |id| seen.borrow_mut().push(id)
            });
            view.set_cards(&[Card {
                id: CardId::Mount("nas-filmy".into()),
                label: "filmy".into(),
                detail: "/m/nas-filmy".into(),
                icon: "network-workgroup-symbolic".into(),
                fraction: None,
                usage: "/m/nas-filmy".into(),
                state: Some("Connected"),
                can_open: true,
                can_disconnect: true,
            }]);
            let child = view.flow.child_at_index(0).expect("the card");
            let button = find_button(child.child().as_ref()).expect("a share card has one");
            button.emit_by_name::<()>("clicked", &[]);
            assert_eq!(*seen.borrow(), ["nas-filmy".to_string()]);
        });
    }

    /// Find a card's Disconnect button, by widget name, anywhere under a widget.
    fn find_button(widget: Option<&gtk::Widget>) -> Option<gtk::Widget> {
        let widget = widget?;
        if widget.widget_name() == "ufm-disconnect" {
            return Some(widget.clone());
        }
        widget
            .observe_children()
            .iter::<gtk::Widget>()
            .filter_map(Result::ok)
            .find_map(|child| find_button(Some(child.as_ref())))
    }
}
