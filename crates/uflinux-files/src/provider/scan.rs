//! Reading the desktop directory into a [`Model`], and saving where the icons
//! ended up.
//!
//! This is the only place the provider looks at the filesystem. Everything else
//! — the D-Bus dispatch, the placement, the persistence — works on ids and
//! cells, which is what keeps the protocol honest: a renderer never sends a
//! path, and this module is where the one path the provider trusts is joined to
//! an id that [`Model::path_in`] has already vetted.
//!
//! One [`gio::File::enumerate_children`] pass does the whole directory. The
//! attributes asked for are the ones that change what an icon is or how it looks,
//! and nothing else: `standard::size` and the timestamps would be fetched for
//! every item and thrown away again, because the model has no field for them and
//! a desktop icon does not show them. What is asked for has to be read, and read
//! has to be asked for — `GLib` treats an attribute that was never requested as a
//! programming error rather than as an absent value, so a field read from a
//! [`gio::FileInfo`] is a field named below.
//!
//! The scan is synchronous, and that is a deliberate trade rather than an
//! oversight. A desktop directory is tens of entries, one `stat` each; the
//! provider is a process that exists to avoid doing expensive work on a
//! compositor's thread, and a second thread here would need the model behind a
//! mutex for the duration of the read, which is exactly the sharing the single
//! main-context thread was chosen to avoid. The one thing that would justify a
//! worker is a network home directory, and on that mount the watcher is the
//! thing that would have to move first.

use std::path::{Path, PathBuf};

use gio::prelude::*;
use ufm_desktop_ipc::{Applied, Item, Kind, Layout, MAX_ITEMS, Model};

/// The attributes one scan needs. `standard::icon` is what turns a `.jpg` into
/// `image-jpeg` without this crate carrying its own MIME-to-icon table, and
/// `standard::type` is what separates a folder from a file without a second
/// `stat`. `standard::is-hidden` and `standard::display-name` are what skipping
/// dot-files and labelling an icon are allowed to ask about, and the pair
/// `standard::is-symlink`/`standard::symlink-target` is the tooltip for a link —
/// all of them answered from the one `stat` the enumerator already does, so
/// naming them costs nothing and omitting one costs a critical and a null string.
const ATTRIBUTES: &str = "standard::name,standard::type,standard::icon,\
                          standard::is-hidden,standard::display-name,\
                          standard::is-symlink,standard::symlink-target";

/// Largest desktop directory that will be served.
///
/// [`MAX_ITEMS`] is the protocol's cap and the renderer's allocation budget, so
/// it is also the point where a scan has to stop. The number is reached by
/// counting, not by truncating a sorted list, so which icons are missing depends
/// on directory order rather than on name — which is fine, because a desktop
/// with two thousand icons on it is a directory the user is not using as a
/// desktop, and the alternative is reading all ten thousand entries to find out.
pub const MAX_ITEMS_SCANNED: usize = MAX_ITEMS;

/// What one scan found, before it is merged with the saved layout.
pub struct Scan {
    /// The directory this was read from, for the model's `desktop` field.
    pub desktop: PathBuf,
    /// The items, in directory order.
    pub items: Vec<Item>,
    /// Whether the directory could be read at all. A desktop that cannot be
    /// read is reported in `Status.note` rather than shown as empty, because an
    /// empty desktop and an unreadable one look the same to a renderer and want
    /// opposite messages.
    pub readable: bool,
    /// Whether icons may be moved: false on a read-only mount, and the safe
    /// reading when the answer could not be found out.
    pub writable: bool,
    /// Entries skipped because the model was full.
    pub dropped: usize,
}

impl Scan {
    /// Read `desktop`. A directory that is missing or unreadable yields an empty
    /// scan that says so, which is the difference between "you have no icons"
    /// and "I cannot see your icons".
    pub fn read(desktop: &Path) -> Self {
        let mut scan = Self {
            desktop: desktop.to_path_buf(),
            items: Vec::new(),
            readable: false,
            writable: false,
            dropped: 0,
        };
        let dir = gio::File::for_path(desktop);
        let Ok(list) = dir.enumerate_children(
            ATTRIBUTES,
            gio::FileQueryInfoFlags::NONE,
            Option::<&gio::Cancellable>::None,
        ) else {
            return scan;
        };
        scan.readable = true;
        scan.writable = writable(&dir);
        let mut found = Vec::new();
        while let Ok(Some(info)) = list.next_file(Option::<&gio::Cancellable>::None) {
            if info.is_hidden() {
                continue;
            }
            let Some(name) = info.name().to_str().map(str::to_string) else {
                continue;
            };
            let child = list.child(&info);
            found.push(describe(&child, &info, &name));
        }
        // Directory order from the kernel is arbitrary, and this order decides
        // which free cell a new icon gets. Sorting makes an icon appear in the
        // same place after the same change no matter which filesystem the
        // desktop lives on.
        found.sort_by_key(|item| item.id.to_lowercase());
        if found.len() > MAX_ITEMS_SCANNED {
            scan.dropped = found.len() - MAX_ITEMS_SCANNED;
            found.truncate(MAX_ITEMS_SCANNED);
        }
        scan.items = found;
        scan
    }

    /// The model this scan describes, with no cells yet.
    #[must_use]
    pub fn model(&self) -> Model {
        let mut model = Model::new(&self.desktop);
        model.writable = self.writable;
        for item in &self.items {
            // Ids come from path components, so a duplicate would mean the
            // directory listed the same name twice — which cannot happen, and
            // is not worth a branch beyond the one `push` already makes.
            let _ = model.push(item.clone());
        }
        model
    }

    /// Put the icons of this scan into `layout`, updating `layout` with wherever
    /// they ended up.
    ///
    /// The layout is the caller's, and is what the caller saves: a merge that
    /// placed a new icon has to be written down, or the icon moves again on
    /// restart. The generation travels through it, which is what stops the
    /// counter going backwards when the provider restarts.
    ///
    /// The grids are taken from the layout. A renderer that has reported screens
    /// calls [`ufm_desktop_ipc::Layout::set_geometry`] first, because a screen
    /// change is a request of its own; folding that in here would mean this one
    /// function had two reasons to change.
    ///
    /// The generation is carried across untouched. Whether a merge is worth a
    /// repaint depends on what the renderer was last told, which is a question only
    /// the caller can ask — see [`ufm_desktop_ipc::Applied::changed`] for the
    /// narrower claim this function could make about its own work.
    pub fn merge(&self, layout: &mut Layout) -> (Model, Applied) {
        let mut model = self.model();
        model.generation = layout.generation;
        // The grids have to be in the model before anything is placed, because
        // [`ufm_desktop_ipc::Layout::apply`] asks the model where a saved cell
        // still fits rather than consulting the layout it is being applied from.
        // A layout has already been normalised, so this cannot fail; the `_` is for
        // the one case that would mean a grid was dropped, which is a grid that was
        // never usable and would have been dropped anyway.
        let _ = model.set_geometry(layout.monitors.clone());
        let applied = layout.apply(&mut model);
        *layout = Layout::capture(&model);
        (model, applied)
    }

    /// What this merge did, in words that are safe to put in a log line and
    /// never contain a file name.
    #[must_use]
    pub fn explain(&self, applied: &Applied) -> Option<String> {
        let mut parts = Vec::new();
        if applied.restored > 0 {
            parts.push(format!("{} restored", applied.restored));
        }
        if applied.placed > 0 {
            parts.push(format!("{} placed", applied.placed));
        }
        if applied.forgotten > 0 {
            parts.push(format!("{} forgotten", applied.forgotten));
        }
        if !applied.homeless.is_empty() {
            parts.push(format!("{} with no room", applied.homeless.len()));
        }
        if self.dropped > 0 {
            parts.push(format!("{} over the cap", self.dropped));
        }
        if !self.readable {
            parts.push("directory unreadable".to_string());
        }
        (!parts.is_empty()).then(|| parts.join(", "))
    }
}

/// Whether icons on this directory may be moved.
///
/// `g_file_query_filesystem_info` is the only way to ask that does not guess,
/// and a refusal to answer counts as read-only: the failure mode of being wrong
/// that way is a drag that does nothing, and the failure mode of the other way
/// is a move that fails halfway across a network mount.
fn writable(dir: &gio::File) -> bool {
    dir.query_filesystem_info("filesystem::readonly", Option::<&gio::Cancellable>::None)
        .is_ok_and(|info| !info.boolean("filesystem::readonly"))
}

/// One entry, as the model describes it.
fn describe(child: &gio::File, info: &gio::FileInfo, name: &str) -> Item {
    let launcher = is_launcher(name);
    let parsed = launcher
        .then(|| gio::DesktopAppInfo::from_filename(child.path().unwrap_or_default()))
        .flatten();
    let kind = match parsed.as_ref() {
        Some(_) => Kind::Launcher,
        None if info.file_type() == gio::FileType::Directory => Kind::Directory,
        None => Kind::File,
    };
    let mut item = Item::new(name, label(info, name, parsed.as_ref()), kind);
    item.icon = match parsed.as_ref() {
        Some(app) => app
            .icon()
            .and_then(|icon| icon_names(&icon))
            .unwrap_or_default(),
        None => info
            .icon()
            .as_ref()
            .and_then(icon_names)
            .unwrap_or_default(),
    };
    item.tooltip = tooltip(info, parsed.as_ref());
    item
}

/// The label: a launcher's `Name`, which is usually not the file name, and the
/// display name for everything else.
///
/// A launcher is labelled from its own desktop entry rather than from the
/// filesystem, because the whole point of the entry is that `firefox.desktop`
/// is called Firefox. The display name is what a backend with its own name
/// metadata reports, and the file name is what is left when it reports nothing.
fn label(info: &gio::FileInfo, name: &str, launcher: Option<&gio::DesktopAppInfo>) -> String {
    if let Some(app) = launcher
        && let Some(text) = app.string("Name")
        && !text.is_empty()
    {
        return text.to_string();
    }
    let text = info.display_name();
    if text.is_empty() {
        name.to_string()
    } else {
        text.to_string()
    }
}

/// Secondary tooltip text, and `None` for the common case of a plain file,
/// which is what lets the field be skipped on the wire.
fn tooltip(info: &gio::FileInfo, launcher: Option<&gio::DesktopAppInfo>) -> Option<String> {
    if let Some(app) = launcher {
        let comment = app.string("Comment")?;
        return (!comment.is_empty()).then(|| comment.to_string());
    }
    // `standard::symlink-target` is only ever set on an entry that is a link, and
    // GLib answers an attribute it never set with a critical and a null pointer
    // rather than with "no". `is_symlink` is the question that can be asked of any
    // entry, so the expensive one is asked only of something that can answer it.
    if !info.is_symlink() {
        return None;
    }
    let target = info.symlink_target()?;
    (!target.as_os_str().is_empty()).then(|| target.to_string_lossy().into_owned())
}

/// Whether this name should be read as a launcher rather than as text.
///
/// A `.desktop` file that fails to parse stays a plain file: the honest answer
/// is the icon for what it actually is, and reporting it as [`Kind::Launcher`]
/// would make the renderer offer to run something that cannot be run.
#[must_use]
pub fn is_launcher(name: &str) -> bool {
    name.ends_with(".desktop")
}

/// The first usable icon name for a [`gio::Icon`].
///
/// A themed icon is a list, in preference order, and the first entry is what a
/// renderer with a theme will find; a file icon is an absolute path, which the
/// protocol allows for exactly this case — an image the file manager can render
/// and no icon theme has ever heard of. Anything else, like an emblemed icon,
/// collapses to its base icon, and a renderer that cannot resolve the result
/// falls back to [`Kind::fallback_icon`], which the model already carries.
#[must_use]
pub fn icon_names(icon: &gio::Icon) -> Option<String> {
    if let Some(themed) = icon.downcast_ref::<gio::ThemedIcon>() {
        return themed.names().first().map(ToString::to_string);
    }
    if let Some(file) = icon.downcast_ref::<gio::FileIcon>() {
        let path = file.file().path()?;
        return Some(path.to_string_lossy().into_owned());
    }
    if let Some(emblemed) = icon.downcast_ref::<gio::EmblemedIcon>() {
        return icon_names(&emblemed.icon());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use ufm_core::DesktopSlot;
    use ufm_desktop_ipc::{Cell, Geometry};

    /// A directory that is really there, because a scan is a read and a read of a
    /// fixture that does not exist tests the wrong branch.
    fn desktop(names: &[&str]) -> tempfile::TempDir {
        let root = tempfile::tempdir().expect("a temporary desktop");
        let dir = root.path().join("Desktop");
        std::fs::create_dir_all(&dir).expect("a desktop directory");
        for name in names {
            std::fs::write(dir.join(name), b"contents").expect("a desktop file");
        }
        root
    }

    fn ids(scan: &Scan) -> Vec<String> {
        scan.items.iter().map(|item| item.id.clone()).collect()
    }

    #[test]
    fn a_directory_that_is_not_there_is_reported_as_unreadable() {
        let root = tempfile::tempdir().expect("a temporary directory");
        let missing = root.path().join("Desktop");
        let scan = Scan::read(&missing);
        assert!(!scan.readable, "a missing folder is not an empty folder");
        assert!(
            !scan.writable,
            "nothing can be moved in a folder that is not there"
        );
        assert!(scan.items.is_empty());
        let (model, applied) = scan.merge(&mut Layout::new());
        assert_eq!(model.desktop, missing.display().to_string());
        assert!(
            !model.writable,
            "the model has to say the same thing the scan said"
        );
        assert!(!applied.changed(), "an unreadable directory moves nothing");
    }

    #[test]
    fn a_directory_that_is_there_is_writable() {
        let root = desktop(&["a"]);
        let scan = Scan::read(&root.path().join("Desktop"));
        assert!(scan.readable);
        assert!(
            scan.writable,
            "a temporary directory under /tmp is not read-only"
        );
        assert_eq!(scan.items.len(), 1);
    }

    #[test]
    fn dot_files_are_not_icons() {
        let root = desktop(&["a", ".hidden", ".config"]);
        let scan = Scan::read(&root.path().join("Desktop"));
        assert_eq!(ids(&scan), vec!["a".to_string()]);
    }

    #[test]
    fn names_are_ordered_the_same_way_every_time() {
        // The order decides which free cell a new icon lands in, so it has to come
        // from the names and not from the directory.
        let root = tempfile::tempdir().expect("a temporary desktop");
        let dir = root.path().join("Desktop");
        std::fs::create_dir_all(&dir).expect("a desktop directory");
        for name in ["zeta", "Alpha", "mango", "beta"] {
            std::fs::write(dir.join(name), b"x").expect("a desktop file");
        }
        let scan = Scan::read(&dir);
        assert_eq!(
            ids(&scan),
            vec![
                "Alpha".to_string(),
                "beta".to_string(),
                "mango".to_string(),
                "zeta".to_string()
            ],
            "sorted, and case-insensitively, which is what a person scanning a list sees"
        );
    }

    #[test]
    fn a_folder_is_an_icon_of_its_own_kind() {
        let root = tempfile::tempdir().expect("a temporary desktop");
        let dir = root.path().join("Desktop");
        std::fs::create_dir_all(dir.join("Složka")).expect("a desktop folder");
        std::fs::write(dir.join("note.txt"), b"x").expect("a desktop file");
        let scan = Scan::read(&dir);
        let folder = scan
            .items
            .iter()
            .find(|item| item.id == "Složka")
            .expect("the folder was scanned");
        assert_eq!(folder.kind, Kind::Directory);
        let file = scan
            .items
            .iter()
            .find(|item| item.id == "note.txt")
            .expect("the file was scanned");
        assert_eq!(file.kind, Kind::File);
        assert!(
            !file.icon.is_empty(),
            "a .txt file gets its icon from the content type, not from a table here"
        );
    }

    #[test]
    fn a_broken_launcher_stays_a_file() {
        // A `.desktop` that does not parse cannot be run, and claiming otherwise
        // would have a renderer offer to launch something that fails.
        let root = desktop(&["broken.desktop"]);
        let scan = Scan::read(&root.path().join("Desktop"));
        let item = scan.items.first().expect("the entry was scanned");
        assert_eq!(item.kind, Kind::File);
        assert!(is_launcher("broken.desktop"));
        assert!(!is_launcher("broken"));
    }

    #[test]
    fn a_launcher_for_a_vanished_program_stays_a_file() {
        // Well-formed, and the common shape of an icon left behind by an uninstalled
        // application: GLib checks that the `Exec` program resolves, so the entry
        // parses as a desktop entry and is still not something that can be launched.
        let root = desktop(&["gone.desktop"]);
        let dir = root.path().join("Desktop");
        std::fs::write(
            dir.join("gone.desktop"),
            "[Desktop Entry]\nType=Application\nName=Something Gone\n\
             Exec=/usr/bin/a-program-that-is-not-installed\n",
        )
        .expect("a stale launcher");
        let scan = Scan::read(&dir);
        let item = scan.items.first().expect("the entry was scanned");
        assert_eq!(item.kind, Kind::File);
        assert_eq!(
            item.name, "gone.desktop",
            "an entry that is not launchable is labelled as the file it is, which is the \
             same answer a broken entry gets"
        );
    }

    #[test]
    fn a_launcher_is_labelled_from_its_own_entry() {
        let root = tempfile::tempdir().expect("a temporary desktop");
        let dir = root.path().join("Desktop");
        std::fs::create_dir_all(&dir).expect("a desktop directory");
        // `Exec` names a program that exists: a desktop entry whose program cannot be
        // found is not a launchable application to GLib, and therefore not one here.
        std::fs::write(
            dir.join("browser.desktop"),
            "[Desktop Entry]\nType=Application\nName=Firefox\nExec=/bin/sh\n\
             Comment=Browse the Web\n",
        )
        .expect("a launcher");
        let scan = Scan::read(&dir);
        let item = scan.items.first().expect("the launcher was scanned");
        assert_eq!(item.kind, Kind::Launcher);
        assert_eq!(item.name, "Firefox", "the entry's name, not the file's");
        assert_eq!(
            item.tooltip.as_deref(),
            Some("Browse the Web"),
            "the comment is what a tooltip wants"
        );
    }

    #[test]
    fn a_merge_keeps_a_saved_cell_and_places_the_rest() {
        let root = desktop(&["a", "b"]);
        let dir = root.path().join("Desktop");
        let mut layout = Layout::new();
        let grids = vec![Geometry::new(0, 4, 4, 96, 96)];
        layout.set_geometry(grids.clone());
        layout.set("b", Cell::new(0, DesktopSlot { col: 2, row: 3 }));
        let scan = Scan::read(&dir);
        let (model, applied) = scan.merge(&mut layout);
        assert_eq!(
            model.cell_of("b"),
            Some(Cell::new(0, DesktopSlot { col: 2, row: 3 })),
            "where the user put an icon is the user's decision"
        );
        assert_eq!(
            model.cell_of("a"),
            Some(Cell::new(0, DesktopSlot { col: 0, row: 0 })),
            "a new icon takes the first cell that is free"
        );
        assert_eq!(applied.restored, 1);
        assert_eq!(applied.placed, 1);
        assert!(applied.homeless.is_empty());
        assert_eq!(model.monitors, grids, "the grids travel with the merge");
    }

    #[test]
    fn a_saved_cell_for_a_deleted_file_is_forgotten() {
        let root = desktop(&["still-here"]);
        let dir = root.path().join("Desktop");
        let mut layout = Layout::new();
        layout.set_geometry(vec![Geometry::new(0, 4, 4, 96, 96)]);
        layout.set("gone", Cell::new(0, DesktopSlot { col: 1, row: 1 }));
        let scan = Scan::read(&dir);
        let (model, applied) = scan.merge(&mut layout);
        assert_eq!(
            model.cell_of("gone"),
            None,
            "no cell for a file that is not there"
        );
        assert_eq!(applied.forgotten, 1);
        assert!(
            scan.explain(&applied)
                .is_some_and(|r| r.contains("forgotten")),
            "a stale cell is worth a line in the log, and is the only clue a user gets"
        );
        assert_eq!(
            layout.cell_of("gone"),
            None,
            "the layout comes back cleaned, so the next save does not resurrect it"
        );
    }

    #[test]
    fn an_explanation_names_counts_and_never_a_path() {
        let root = desktop(&["a"]);
        let scan = Scan::read(&root.path().join("Desktop"));
        let mut layout = Layout::new();
        layout.set_geometry(vec![Geometry::new(0, 4, 4, 96, 96)]);
        let (model, applied) = scan.merge(&mut layout);
        let text = scan.explain(&applied).expect("something was placed");
        assert!(text.contains("placed"), "{text}");
        assert!(!text.contains("a\n"), "a reason is one line");
        assert!(
            !text.contains(&scan.desktop.display().to_string()),
            "the note goes on a bus, and a path on a bus is a leak"
        );
        let _ = model;
    }

    #[test]
    fn nothing_to_say_is_no_note() {
        let root = desktop(&[]);
        let scan = Scan::read(&root.path().join("Desktop"));
        let applied = Applied::default();
        assert!(
            scan.explain(&applied).is_none(),
            "an empty desktop that was read has nothing to report"
        );
    }

    #[test]
    fn a_themed_icon_becomes_its_first_name() {
        let icon = gio::ThemedIcon::from_names(&["image-jpeg", "image"]);
        assert_eq!(
            icon_names(&icon.into()).as_deref(),
            Some("image-jpeg"),
            "the list is in preference order"
        );
    }

    #[test]
    fn a_file_icon_becomes_its_path() {
        let file = gio::File::for_path("/usr/share/pixmaps/thing.png");
        let icon = gio::FileIcon::new(&file);
        assert_eq!(
            icon_names(&icon.into()).as_deref(),
            Some("/usr/share/pixmaps/thing.png"),
            "an image no theme knows about travels as a path"
        );
    }

    #[test]
    fn an_icon_with_no_local_path_says_nothing() {
        // A file icon whose file is not a local path has nothing to hand a renderer:
        // the protocol carries an icon name or an absolute path, and a remote URI is
        // neither. `gio::ThemedIcon` cannot be built empty at all — GLib asserts that
        // the list has a first entry — so the empty case is not reachable from here.
        let remote = gio::File::for_uri("http://example.invalid/icon.png");
        let icon = gio::FileIcon::new(&remote);
        assert_eq!(
            icon_names(&icon.into()),
            None,
            "a path a renderer cannot open is worse than no icon at all"
        );
    }
}
