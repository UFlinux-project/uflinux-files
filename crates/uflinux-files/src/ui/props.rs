//! What one thing is made of.
//!
//! The properties dialog is the one place the interface shows everything the
//! reader already collected and the listing has no room for: the mode bits, the
//! owner, the MIME type, the moment the file was last written. It is also the
//! only place that answers "how big is this folder", which nothing else in a
//! listing says — a folder's size column holds its item count, because a real
//! size costs a walk and a listing should not pay for one it was not asked for.
//!
//! So the dialog does pay for it, and pays the way the rest of the interface
//! does: on a worker, with the answer arriving through the window's queue (see
//! [`super::job`]) and the walk itself bounded by the caps in [`ufm_vfs::du`].
//! A folder that takes a second to count holds up nothing but its own row, which
//! reads `Counting…` until the answer lands, and closing the dialog stops the
//! walk rather than letting it run for an answer nobody will read.
//!
//! The window is a plain [`gtk::Window`] in the shape [`super::ops`] uses —
//! built by hand, attached to the application, destroyed with its parent — for
//! the same two reasons: `GTK4` has no dialog class worth subclassing, and a
//! window that is not an `ApplicationWindow` is not exported on the bus, which
//! keeps the object paths a test asserts against stable across whatever else
//! opens. Unlike the prompts this one is not modal: reading a folder's size and
//! then clicking into it is the ordinary sequence, and a dialog that blocks the
//! window makes that two steps instead of one.

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use gtk::prelude::*;

use ufm_core::Location;
use ufm_core::config::{format_full, format_modified};
use ufm_core::entry::{Entry, human_size};
use ufm_vfs::Cancel;
use ufm_vfs::du::{self, Tally};

use super::job::{Event, Handle};
use super::row::format_items;

/// The dialog's own state, kept off the window: the answer to a walk needs the
/// label it belongs in, and the only thing that outlives the call which built
/// the dialog is the label itself.
struct Dialog {
    id: usize,
    /// The folder the walk was started for, matched against the answer.
    path: PathBuf,
    /// The row the total belongs in.
    size: gtk::Label,
    /// Set when the dialog goes away, so the walk stops early.
    cancel: Cancel,
}

thread_local! {
    /// The dialogs currently open. A `Vec` rather than one slot: two windows
    /// can each have a dialog, and one window can have two for different files.
    static OPEN: RefCell<Vec<Dialog>> = const { RefCell::new(Vec::new()) };
    static NEXT_ID: Cell<usize> = const { Cell::new(1) };
}

/// Show the properties of `entry`.
pub fn show(parent: &gtk::ApplicationWindow, handle: &Handle, entry: &Entry) {
    let (window, size) = build(entry);
    if let Some(app) = parent.application() {
        app.add_window(&window);
    }
    window.set_transient_for(Some(parent));
    window.set_destroy_with_parent(true);
    window.present();

    // `build` hands back the size row only for a folder the kernel can see,
    // which is exactly the set worth walking: a file's size is already exact, an
    // archive member's is what the container says it is, and a virtual folder
    // has no path to walk.
    let Some(size) = size else { return };
    // `as_path` rather than the entry's location: inside an archive the row is a
    // position in a file, and only a `File` location has a folder to count.
    let Some(path) = entry.location.as_path().map(Path::to_path_buf) else {
        return;
    };
    let cancel = Cancel::new();
    if !start(handle, &path, &cancel) {
        size.set_label(SIZE_UNKNOWN);
        return;
    }
    let id = NEXT_ID.with(|next| {
        let id = next.get();
        next.set(id + 1);
        id
    });
    OPEN.with(|open| {
        open.borrow_mut().push(Dialog {
            id,
            path,
            size,
            cancel: cancel.clone(),
        });
    });
    // `destroy` rather than `close_request`: a dialog closed with its parent is
    // destroyed without being asked first, and a walk left running for a label
    // that is no longer on screen is memory the window paid for and never used.
    window.connect_destroy(move |_| {
        cancel.cancel();
        OPEN.with(|open| open.borrow_mut().retain(|dialog| dialog.id != id));
    });
}

/// Put a folder's total in the dialog that asked for it, if it is still open.
///
/// Called from the window's event handling. An answer with no dialog waiting is
/// the ordinary outcome of closing one quickly and is dropped, not reported.
pub fn deliver(path: &Path, tally: &Tally) {
    OPEN.with(|open| {
        for dialog in open.borrow().iter() {
            if dialog.path != path {
                continue;
            }
            dialog.size.set_label(&folder_words(tally));
            return;
        }
    });
}

/// What the size row says while the walk is running.
const COUNTING: &str = "Counting\u{2026}";

/// What it says when the walk could not be started at all.
const SIZE_UNKNOWN: &str = "Size unknown";

/// Start a walk. `false` means no thread could be started, which the caller
/// reports in the row rather than leaving as `Counting…` forever.
fn start(handle: &Handle, path: &Path, cancel: &Cancel) -> bool {
    let worker = handle.clone();
    let folder = path.to_path_buf();
    let flag = cancel.clone();
    // The same small stack every worker in this program gets: the walk holds a
    // `Vec` of paths and a `DirEntry` at a time, and a deep folder is bounded by
    // the depth cap in `ufm_vfs::du` rather than by the stack.
    std::thread::Builder::new()
        .stack_size(256 * 1024)
        .spawn(move || {
            let tally = du::total(&folder, &flag);
            worker.send(Event::Sized {
                path: folder,
                tally,
            });
        })
        .is_ok()
}

/// Build the dialog without showing it, so the widget tests can read a form
/// without an application to attach it to.
///
/// The second value is the size row when it is one that a walk will rewrite —
/// that is, when `entry` is a folder the kernel can see — and `None` otherwise.
fn build(entry: &Entry) -> (gtk::Window, Option<gtk::Label>) {
    let window = gtk::Window::builder()
        .title("Properties")
        .resizable(false)
        .build();
    window.set_widget_name("ufm-props");

    let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
    content.append(&header(entry));

    let grid = gtk::Grid::new();
    grid.set_row_spacing(6);
    grid.set_column_spacing(18);
    grid.set_margin_top(12);
    grid.set_margin_start(18);
    grid.set_margin_end(18);
    let live = add_rows(&mut Form::new(&grid), entry);
    content.append(&grid);

    let close = gtk::Button::with_mnemonic("_Close");
    close.set_widget_name("ufm-props-close");
    close.connect_clicked({
        let window = window.clone();
        move |_| window.close()
    });
    let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    buttons.set_halign(gtk::Align::End);
    buttons.set_margin_top(12);
    buttons.set_margin_start(18);
    buttons.set_margin_end(18);
    buttons.set_margin_bottom(18);
    buttons.append(&close);
    content.append(&buttons);

    window.set_child(Some(&content));
    window.set_default_widget(Some(&close));
    window.add_controller(escape_to_close(&window));
    (window, live)
}

/// Escape closes the dialog, as it does every other prompt in this program.
fn escape_to_close(window: &gtk::Window) -> gtk::EventControllerKey {
    let keys = gtk::EventControllerKey::new();
    let target = window.clone();
    keys.connect_key_pressed(move |_, key, _code, _state| {
        if key == gtk::gdk::Key::Escape {
            target.close();
            glib::Propagation::Stop
        } else {
            glib::Propagation::Proceed
        }
    });
    keys
}

/// The icon, the name and what it is, across the top.
fn header(entry: &Entry) -> gtk::Box {
    let box_ = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    box_.set_margin_top(18);
    box_.set_margin_start(18);
    box_.set_margin_end(18);

    let icon = gtk::Image::from_icon_name(entry.kind.icon_name());
    icon.set_widget_name("ufm-props-icon");
    icon.set_pixel_size(48);
    icon.set_valign(gtk::Align::Start);
    box_.append(&icon);

    let names = gtk::Box::new(gtk::Orientation::Vertical, 3);
    names.set_valign(gtk::Align::Center);
    names.set_hexpand(true);

    let title = gtk::Label::new(Some(&entry.name));
    title.set_widget_name("ufm-props-title");
    title.set_wrap(true);
    title.set_xalign(0.0);
    title.set_selectable(true);
    names.append(&title);

    let kind = gtk::Label::new(Some(&entry.kind_label()));
    kind.set_widget_name("ufm-props-kind");
    kind.set_xalign(0.0);
    kind.add_css_class("dim-label");
    names.append(&kind);

    box_.append(&names);
    box_
}

/// The form itself: one row per fact the entry has room to say.
///
/// A row is added only when there is something to put in it, which is what makes
/// a file on a filesystem with no mode bits read as a short form rather than as
/// a form with holes in it.
fn add_rows(form: &mut Form, entry: &Entry) -> Option<gtk::Label> {
    form.add("type", "Type", &entry.kind_label());
    if let Some(folder) = container_of(entry) {
        form.add("location", "Location", &folder);
    }
    if let Some(inner) = entry
        .location
        .archive_inner()
        .filter(|inner| !inner.is_empty())
    {
        form.add("inside", "Inside archive", inner);
    }
    let live = if entry.kind.is_dir() && entry.location.is_real() {
        let label = form.add("size", "Size", COUNTING);
        Some(label)
    } else {
        form.add("size", "Size", &human_size(entry.size));
        None
    };
    if let Some(allocated) = entry.allocated.filter(|on| *on != entry.size) {
        form.add("allocated", "On disk", &human_size(allocated));
    }
    if entry.kind.is_dir() {
        form.add(
            "contents",
            "Contents",
            &entry
                .child_count
                .map_or_else(|| "\u{2014}".to_string(), |n| format_items(u64::from(n))),
        );
    }
    for (name, key, when) in [
        ("modified", "Modified", entry.modified),
        ("accessed", "Accessed", entry.accessed),
        ("created", "Created", entry.created),
    ] {
        if let Some(when) = when {
            form.add(name, key, &moment(when));
        }
    }
    if let Some(mode) = entry.permissions() {
        form.add_mono("permissions", "Permissions", &mode);
    }
    if let Some(owner) = entry.owner() {
        form.add("owner", "Owner", &owner);
    }
    if let Some(group) = entry.group() {
        form.add("group", "Group", &group);
    }
    if let Some(mime) = entry.mime.as_deref().filter(|mime| !mime.is_empty()) {
        form.add("mime", "MIME type", mime);
    }
    live
}

/// The folder that holds `entry`, spelled out. A virtual location has none, and
/// an archive member is shown inside the file that holds it rather than inside
/// the archive, because the file is what a user would go looking for.
fn container_of(entry: &Entry) -> Option<String> {
    match &entry.location {
        Location::File { path } => path.parent().map(|p| p.display().to_string()),
        Location::Archive { archive, .. } => Some(archive.display().to_string()),
        Location::Virtual(_) | Location::Search(_) => None,
    }
}

/// An absolute date and a clock time, with the relative wording the listing
/// uses beside it: the first says when, the second says how long ago, and a
/// properties dialog is where both are worth the space.
fn moment(when: SystemTime) -> String {
    format!("{}  ({})", format_full(when), format_modified(when))
}

/// The size row for a folder, once the walk has answered.
///
/// The item counts come along because a total on its own invites the question
/// the walk already paid to answer, and `partial` becomes the honest words
/// rather than a number that is quietly a lower bound.
fn folder_words(tally: &Tally) -> String {
    if tally.is_empty() {
        return if tally.partial {
            "Nothing could be read".to_string()
        } else {
            "Empty".to_string()
        };
    }
    let at_least = if tally.partial { "at least " } else { "" };
    let mut text = format!(
        "{at_least}{} in {}",
        human_size(tally.bytes),
        plural(tally.files, "file")
    );
    if tally.folders > 0 {
        text.push_str(", ");
        text.push_str(&plural(tally.folders, "folder"));
    }
    if tally.unreadable > 0 {
        text.push_str("; ");
        text.push_str(&plural(tally.unreadable, "entry"));
        text.push_str(" unreadable");
    }
    text
}

/// `3 files` / `1 file`, the same shape [`format_items`] uses.
fn plural(count: u64, noun: &str) -> String {
    if count == 1 {
        format!("1 {noun}")
    } else {
        format!("{count} {noun}s")
    }
}

/// The rows written so far, so each one can be told where it goes without the
/// caller counting.
struct Form {
    grid: gtk::Grid,
    row: i32,
}

impl Form {
    fn new(grid: &gtk::Grid) -> Self {
        Self {
            grid: grid.clone(),
            row: 0,
        }
    }

    /// One row, its value selectable so a path can be copied out of it.
    fn add(&mut self, name: &str, key: &str, text: &str) -> gtk::Label {
        self.field(name, key, text, false)
    }

    /// One row whose value is fixed-width, for a mode string whose columns are
    /// the information.
    fn add_mono(&mut self, name: &str, key: &str, text: &str) -> gtk::Label {
        self.field(name, key, text, true)
    }

    fn field(&mut self, name: &str, key: &str, text: &str, mono: bool) -> gtk::Label {
        let label = gtk::Label::new(Some(key));
        label.set_widget_name(&format!("ufm-props-key-{name}"));
        label.set_xalign(0.0);
        label.set_valign(gtk::Align::Start);
        label.add_css_class("dim-label");

        let value = gtk::Label::new(Some(text));
        value.set_widget_name(&format!("ufm-props-{name}"));
        value.set_xalign(0.0);
        value.set_valign(gtk::Align::Start);
        value.set_selectable(true);
        value.set_wrap(true);
        if mono {
            // A style class rather than a property: `GTK4` puts the monospace
            // font on `TextView`, and a label's font is the stylesheet's call —
            // see the `ufm-mono` rule in [`super::css`].
            value.add_css_class("ufm-mono");
        }

        let row = self.row;
        self.grid.attach(&label, 0, row, 1, 1);
        self.grid.attach(&value, 1, row, 1, 1);
        self.row += 1;
        value
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ufm_core::entry::{EntryFlags, EntryKind};

    fn metadata(kind: EntryKind, name: &str) -> Entry {
        Entry {
            location: ufm_core::Location::file(PathBuf::from("/srv/data").join(name)),
            name: name.to_string(),
            kind,
            flags: EntryFlags::empty(),
            size: 2_048,
            allocated: None,
            modified: None,
            accessed: None,
            created: None,
            mode: None,
            uid: None,
            gid: None,
            mime: None,
            child_count: None,
            icon_key: "text-x-generic".to_string(),
        }
    }

    fn file() -> Entry {
        let mut entry = metadata(EntryKind::File, "notes.txt");
        entry.mime = Some("text/plain".to_string());
        entry.mode = Some(0o100_644);
        entry.uid = Some(0);
        entry.allocated = Some(4_096);
        entry
    }

    fn folder() -> Entry {
        let mut entry = metadata(EntryKind::Directory, "data");
        entry.child_count = Some(7);
        entry.mode = Some(0o040_755);
        entry
    }

    fn tally(bytes: u64, files: u64, folders: u64) -> Tally {
        Tally {
            bytes,
            files,
            folders,
            unreadable: 0,
            partial: false,
        }
    }

    #[test]
    fn a_folder_total_names_the_files_behind_it() {
        assert_eq!(
            folder_words(&tally(1024, 3, 2)),
            "1.0 KiB in 3 files, 2 folders"
        );
        assert_eq!(folder_words(&tally(10, 1, 0)), "10 B in 1 file");
    }

    #[test]
    fn a_stopped_walk_says_at_least() {
        let mut walked = tally(1_024, 4, 1);
        walked.partial = true;
        assert_eq!(
            folder_words(&walked),
            "at least 1.0 KiB in 4 files, 1 folder"
        );
    }

    #[test]
    fn a_walk_that_could_not_read_anything_says_that() {
        let none = Tally {
            bytes: 0,
            files: 0,
            folders: 0,
            unreadable: 3,
            partial: true,
        };
        assert_eq!(folder_words(&none), "Nothing could be read");
        assert_eq!(folder_words(&Tally::default()), "Empty");
    }

    #[test]
    fn unreadable_corners_are_reported_alongside_a_total() {
        let mut walked = tally(1_024, 2, 0);
        walked.unreadable = 1;
        assert_eq!(
            folder_words(&walked),
            "1.0 KiB in 2 files; 1 entry unreadable"
        );
    }

    #[test]
    fn one_is_spelled_out_and_the_rest_take_an_s() {
        assert_eq!(plural(1, "file"), "1 file");
        assert_eq!(plural(2, "file"), "2 files");
        assert_eq!(plural(0, "folder"), "0 folders");
    }

    #[test]
    fn an_archive_member_is_located_in_the_file_that_holds_it() {
        let mut entry = metadata(EntryKind::File, "inner.txt");
        entry.location = Location::archive(PathBuf::from("/srv/pack.zip"), &["dir", "inner.txt"])
            .expect("a member inside an archive");
        assert_eq!(container_of(&entry).as_deref(), Some("/srv/pack.zip"));
        assert_eq!(
            entry.location.archive_inner().filter(|i| !i.is_empty()),
            Some("dir/inner.txt")
        );
    }

    #[test]
    fn a_virtual_location_has_no_folder_to_show() {
        let mut entry = metadata(EntryKind::Directory, "Computer");
        entry.location = Location::computer();
        assert_eq!(container_of(&entry), None);
    }

    #[test]
    fn a_moment_gives_both_the_clock_and_the_calendar() {
        let text = moment(SystemTime::UNIX_EPOCH);
        assert!(text.starts_with("1970-01-01 00:00"), "{text}");
        // The relative half is the listing's own wording, which becomes a plain
        // date once a moment is far enough back that "45 days ago" stops being
        // useful — so the recent one is what proves the second half is there.
        let recent = moment(SystemTime::now() - std::time::Duration::from_secs(7_200));
        assert!(recent.contains("2 hours ago"), "{recent}");
    }

    #[test]
    fn a_dialog_for_a_file_reports_its_size_without_a_walk() {
        crate::ui::tk::ui(|| {
            let (window, live) = build(&file());
            assert!(live.is_none(), "a file's size is already exact");
            let size = find(&window, "ufm-props-size").expect("a size row");
            assert_eq!(size.downcast_ref::<gtk::Label>().unwrap().text(), "2.0 KiB");
            let allocated = find(&window, "ufm-props-allocated").expect("an on-disk row");
            assert_eq!(
                allocated.downcast_ref::<gtk::Label>().unwrap().text(),
                "4.0 KiB"
            );
            let permissions = find(&window, "ufm-props-permissions").expect("a mode row");
            let permissions = permissions.downcast_ref::<gtk::Label>().unwrap();
            assert_eq!(permissions.text(), "-rw-r--r--");
            assert!(
                permissions.has_css_class("ufm-mono"),
                "a mode string is only readable in a fixed-width font"
            );
            assert!(
                find(&window, "ufm-props-contents").is_none(),
                "a file has no contents row"
            );
        });
    }

    #[test]
    fn a_dialog_for_a_folder_counts_until_the_answer_arrives() {
        crate::ui::tk::ui(|| {
            let (window, live) = build(&folder());
            let live = live.expect("a folder needs a walk");
            assert_eq!(live.text(), COUNTING);
            let contents = find(&window, "ufm-props-contents").expect("a contents row");
            assert_eq!(
                contents.downcast_ref::<gtk::Label>().unwrap().text(),
                "7 items",
                "the count the listing already had is shown at once"
            );
        });
    }

    #[test]
    fn an_answer_rewrites_the_row_of_the_dialog_that_asked() {
        crate::ui::tk::ui(|| {
            let (_, live) = build(&folder());
            let live = live.expect("a folder needs a walk");
            let path = PathBuf::from("/srv/data");
            OPEN.with(|open| {
                open.borrow_mut().push(Dialog {
                    id: NEXT_ID.with(|n| {
                        let id = n.get();
                        n.set(id + 1);
                        id
                    }),
                    path: path.clone(),
                    size: live.clone(),
                    cancel: Cancel::new(),
                });
            });
            deliver(&path, &tally(3_072, 5, 1));
            assert_eq!(live.text(), "3.0 KiB in 5 files, 1 folder");
            // An answer for a folder nobody asked about leaves the row alone —
            // this is what dropping the reply to a closed dialog looks like.
            deliver(Path::new("/elsewhere"), &tally(1, 1, 0));
            assert_eq!(live.text(), "3.0 KiB in 5 files, 1 folder");
            OPEN.with(|open| open.borrow_mut().clear());
        });
    }

    #[test]
    fn a_dialog_says_what_a_thing_is_and_where_it_lives() {
        crate::ui::tk::ui(|| {
            let (window, _) = build(&file());
            for (name, expected) in [
                ("ufm-props-title", "notes.txt"),
                ("ufm-props-kind", "text/plain"),
                ("ufm-props-location", "/srv/data"),
                ("ufm-props-mime", "text/plain"),
            ] {
                let label = find(&window, name).unwrap_or_else(|| panic!("a {name} row"));
                assert_eq!(
                    label.downcast_ref::<gtk::Label>().unwrap().text(),
                    expected,
                    "{name}"
                );
            }
        });
    }

    #[test]
    fn a_form_without_mode_bits_omits_the_rows_it_cannot_fill() {
        crate::ui::tk::ui(|| {
            let (window, _) = build(&metadata(EntryKind::File, "bare"));
            for name in [
                "ufm-props-permissions",
                "ufm-props-owner",
                "ufm-props-group",
                "ufm-props-modified",
                "ufm-props-mime",
            ] {
                assert!(find(&window, name).is_none(), "{name} has nothing to say");
            }
        });
    }

    /// Pull one named widget out of a dialog, the way the other widget tests do.
    fn find<W: IsA<gtk::Widget>>(root: &W, name: &str) -> Option<gtk::Widget> {
        let root: &gtk::Widget = root.as_ref();
        for child in root.observe_children().snapshot() {
            let Ok(widget) = child.downcast::<gtk::Widget>() else {
                continue;
            };
            if widget.widget_name() == name {
                return Some(widget);
            }
            if let Some(deeper) = find(&widget, name) {
                return Some(deeper);
            }
        }
        None
    }
}
