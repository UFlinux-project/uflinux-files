//! Which program should take this file.
//!
//! Double-clicking asks the default handler, and when there is no default handler
//! the answer is a banner saying so. This module is the other half: the row that
//! asks the question out loud and shows what is actually registered, which is the
//! only way a user finds out that the machine can open the file at all.
//!
//! The list comes from `GIO` rather than from a desktop-file parser of our own.
//! `GIO` already reads `/usr/share/applications` and its per-user counterparts,
//! already resolves `MimeType=` against the shared mime database, and already
//! knows which entry the user picked last time — and the file manager has to agree
//! with the rest of the desktop about what opens what, which it cannot do while
//! holding a second opinion in a second parser. The indexer's application catalog
//! is a search index, and searching it would answer "what is called Firefox"
//! rather than "what takes a PDF".
//!
//! Two constraints shape the code. `gio::AppInfo` is neither `Send` nor `Sync`, so
//! the list is read on the main thread and a candidate never travels to a worker or
//! back. And a member of an archive is not a file: no external program can be
//! handed `archive:///t/a.zip!/docs/readme.md`, so the member is written out under
//! [`paths::extracted_dir`] first, on a worker, and the chooser opens when the
//! bytes have landed. The container is opened by that worker rather than borrowed
//! from the reader the view browses with — the same reason [`super::extract`] gives
//! for doing it: the reader sits behind the type-erased set the views share and
//! there is no way back to its type, and the cost is one table-of-contents scan
//! with nothing decompressed.

use std::cell::{Cell, RefCell};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use gio::prelude::*;
use gtk::prelude::*;

use ufm_archive::{ArchiveReader, Opened};
use ufm_core::entry::Entry;
use ufm_core::{Error, Location, paths};

use super::job::{Event, Handle};
use super::ops::{self, Outcome};
use super::props;

/// How long a materialised member is kept before the next one sweeps it away. A
/// week is long enough that a window left open over the weekend still points at a
/// file, and short enough that the cache is a cache rather than a second copy of
/// every archive the user has looked inside.
const AGE: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// A member being written out, and where to take the answer.
///
/// The window is held weakly and the content type is held as a string because the
/// chooser is opened from the event handler rather than from here, and by then the
/// dialog that asked may be gone — the same arrangement [`super::props`] uses for a
/// folder size that arrives after its dialog was closed.
struct Pending {
    id: usize,
    window: glib::WeakRef<gtk::ApplicationWindow>,
    kind: String,
    name: String,
}

thread_local! {
    /// The materialisations in flight, one per request rather than one in total:
    /// two windows can each be waiting for a member.
    static PENDING: RefCell<Vec<Pending>> = const { RefCell::new(Vec::new()) };
    static NEXT_ID: Cell<usize> = const { Cell::new(1) };
}

/// Whether `entry` is something an external program could be pointed at.
///
/// A folder is entered rather than opened, and a location that is neither a file
/// the kernel can see nor a member of an archive has no bytes to hand over. Both
/// presentations and the window's sensitivity rule ask this, so the menu row and
/// the action cannot disagree about what is on screen.
#[must_use]
pub fn openable(entry: &Entry) -> bool {
    !entry.kind.is_dir()
        && (entry.location.is_real()
            || entry
                .location
                .archive_inner()
                .is_some_and(|inner| !inner.is_empty()))
}

/// Show the chooser for `entry`.
///
/// A file on disk is answered on the spot: reading the list of candidates is a
/// directory scan over desktop files, which is cheap and would be slower on a
/// worker because the answer is needed in order to build the widgets anyway. A
/// member of an archive is not, and opens its chooser when the write finishes.
pub fn show(parent: &gtk::ApplicationWindow, handle: &Handle, entry: &Entry) {
    let kind = content_type(entry);
    if let Some(uri) = uri_for(&entry.location) {
        chooser(parent, &entry.name, &kind, &uri);
        return;
    }
    if entry
        .location
        .archive_inner()
        .is_some_and(|inner| !inner.is_empty())
    {
        start(parent, handle, entry, &kind);
    }
}

/// The URI an external program can be handed for `location`, when it is a file the
/// kernel can see. An archive member has no URI yet — the bytes are still inside
/// the container — and a virtual folder has none at all.
fn uri_for(location: &Location) -> Option<String> {
    if !location.is_real() {
        return None;
    }
    location
        .as_path()
        .map(|path| gio::File::for_path(path).uri().to_string())
}

/// A member has been written out, so the chooser it was asked for can open.
///
/// Called from the window's event handling. An answer with nothing waiting for it
/// is the ordinary outcome of a window closed mid-write and is dropped.
pub fn arrived(id: usize, path: &Path) {
    let taken = PENDING.with(|pending| {
        let mut list = pending.borrow_mut();
        list.iter()
            .position(|item| item.id == id)
            .map(|at| list.remove(at))
    });
    let Some(pending) = taken else { return };
    let Some(window) = pending.window.upgrade() else {
        return;
    };
    chooser(
        &window,
        &pending.name,
        &pending.kind,
        &gio::File::for_path(path).uri(),
    );
}

/// Write one member out and report where it landed.
pub fn materialise(location: &Location, into: &Path) -> ufm_core::Result<PathBuf> {
    let reader = ArchiveReader::new();
    let (opened, at) = reader.node_of(location)?;
    let name = opened
        .toc()
        .node(at)
        .filter(|node| !node.kind.is_dir())
        .map(|node| node.name.clone())
        .ok_or_else(|| Error::Archive(format!("{}: nothing to write out.", location.display())))?;
    std::fs::create_dir_all(into)?;
    sweep(into);
    // The member's own name, with the counter [`ops::unique_name`] adds when a
    // chooser is already pointing at a file of that name. Two members called
    // `readme.txt` in different folders of one container is ordinary, and the
    // second one overwriting the first would leave the first chooser reading a
    // different document than the row it came from.
    let target = into.join(ops::unique_name(&name, |candidate| {
        into.join(candidate).exists()
    }));
    write_out(&opened, at, &target)?;
    Ok(target)
}

/// The content type a program is looked up for.
///
/// The MIME type the reader already worked out wins, because it was computed from
/// the same name the row shows and a second guess would be a second opinion. Only
/// a row with no type at all — a nameless share, a member whose container recorded
/// nothing — falls back to guessing from the name, which is what `GIO` does with a
/// suffix and nothing else to go on.
fn content_type(entry: &Entry) -> String {
    if let Some(kind) = entry
        .mime
        .as_deref()
        .filter(|mime| !mime.is_empty())
        .and_then(gio::content_type_from_mime_type)
    {
        return kind.to_string();
    }
    let name = entry.location.file_name().unwrap_or(entry.name.as_str());
    let (kind, _) = gio::content_type_guess(Some(name), None::<&[u8]>);
    kind.to_string()
}

/// The programs that could take this file, in the order `GIO` offers them.
///
/// Both lists are asked for. `GIO` answers a content type with the applications
/// registered for it and, when that list is empty, with a set of generic
/// fallbacks; the fallbacks are the point of asking, because a file with nothing
/// registered for its type is exactly the case where this dialog has something
/// useful to say.
fn candidates(kind: &str) -> Vec<gio::AppInfo> {
    merge(
        gio::AppInfo::all_for_type(kind),
        gio::AppInfo::fallback_for_type(kind),
    )
}

/// The two lists `GIO` gives, in order, with the entries that repeat dropped.
///
/// A desktop file can be registered for a type directly and reached again through
/// a fallback, and `GIO` does not deduplicate across the two calls. An entry that
/// says it should not be shown is left out: it is there to be found by name, not to
/// be offered.
fn merge(all: Vec<gio::AppInfo>, fallback: Vec<gio::AppInfo>) -> Vec<gio::AppInfo> {
    let mut seen: Vec<String> = Vec::new();
    let mut out: Vec<gio::AppInfo> = Vec::new();
    for app in all.into_iter().chain(fallback) {
        if !app.should_show() {
            continue;
        }
        let name = key(&app);
        if seen.contains(&name) {
            continue;
        }
        seen.push(name);
        out.push(app);
    }
    out
}

/// What tells two candidates apart: the desktop file id when `GIO` has one, and the
/// display name otherwise. The second case is not hypothetical — an entry built
/// from a keyfile has no file behind it and so no id — and two rows with one name
/// are the failure this is here to prevent.
fn key(app: &gio::AppInfo) -> String {
    app.id()
        .map_or_else(|| app.display_name().to_string(), |id| id.to_string())
}

/// Open the chooser for a file that already has a path, and therefore a URI.
fn chooser(parent: &gtk::ApplicationWindow, name: &str, kind: &str, uri: &str) {
    let window = build(name, kind, &candidates(kind), uri);
    // A plain window attached to the application rather than an
    // `ApplicationWindow`, for the reason the properties dialog gives: it is not
    // exported on the bus, which keeps the object paths the harness asserts against
    // stable. Not modal either — a user who looks at the list and then wants the
    // folder behind it should not have to close the list first.
    if let Some(app) = parent.application() {
        app.add_window(&window);
    }
    window.set_transient_for(Some(parent));
    window.set_destroy_with_parent(true);
    window.present();
}

/// Ask for a member of an archive, which has to be written out before anything can
/// read it.
fn start(parent: &gtk::ApplicationWindow, handle: &Handle, entry: &Entry, kind: &str) {
    let id = NEXT_ID.with(|next| {
        let id = next.get();
        next.set(id + 1);
        id
    });
    PENDING.with(|pending| {
        pending.borrow_mut().push(Pending {
            id,
            window: parent.downgrade(),
            kind: kind.to_string(),
            name: entry.name.clone(),
        });
    });
    let worker = handle.clone();
    let location = entry.location.clone();
    let spawned =
        std::thread::Builder::new()
            .stack_size(256 * 1024)
            .spawn(
                move || match materialise(&location, &paths::extracted_dir()) {
                    Ok(path) => worker.send(Event::Materialised { id, path }),
                    Err(error) => worker.send(Event::Operation {
                        outcome: Outcome {
                            ok: false,
                            message: error.to_string(),
                            changed: Vec::new(),
                            removed: Vec::new(),
                            undo: None,
                        },
                    }),
                },
            );
    if spawned.is_err() {
        forget(id);
        handle.send(Event::Operation {
            outcome: refused(&format!(
                "{}: cannot start a worker to write it out.",
                entry.name
            )),
        });
    }
}

/// Drop a request without an answer, so a later one cannot be mistaken for it.
fn forget(id: usize) {
    PENDING.with(|pending| pending.borrow_mut().retain(|item| item.id != id));
}

/// An outcome with nothing refreshed and something to say in the banner.
fn refused(message: &str) -> Outcome {
    Outcome {
        ok: false,
        message: message.to_string(),
        changed: Vec::new(),
        removed: Vec::new(),
        undo: None,
    }
}

/// Build the chooser without showing it, so the widget tests can read a list
/// without an application to attach it to.
fn build(name: &str, kind: &str, apps: &[gio::AppInfo], uri: &str) -> gtk::Window {
    let window = gtk::Window::builder()
        .title("Open With")
        .resizable(false)
        .build();
    window.set_widget_name("ufm-open-with");

    let content = gtk::Box::new(gtk::Orientation::Vertical, 12);
    content.set_margin_top(18);
    content.set_margin_start(18);
    content.set_margin_end(18);
    content.append(&header(name, kind));

    let status = gtk::Label::new(None);
    status.set_widget_name("ufm-open-with-status");
    status.set_wrap(true);
    status.set_xalign(0.0);
    status.set_visible(false);

    let list = gtk::Box::new(gtk::Orientation::Vertical, 3);
    list.set_widget_name("ufm-open-with-apps");
    list.add_css_class("ufm-open-with");
    if apps.is_empty() {
        list.append(&nothing_registered(kind));
    } else {
        for app in apps {
            list.append(&app_row(app, uri, &status));
        }
    }
    content.append(&list);
    content.append(&status);

    let close = gtk::Button::with_mnemonic("_Close");
    close.set_widget_name("ufm-open-with-close");
    close.connect_clicked({
        let window = window.clone();
        move |_| window.close()
    });
    let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    buttons.set_halign(gtk::Align::End);
    buttons.append(&close);
    content.append(&buttons);

    window.set_child(Some(&content));
    window.set_default_widget(Some(&close));
    window.add_controller(props::escape_to_close(&window));
    window
}

/// The file's own icon, its name and what it is, across the top. The icon comes
/// from the content type rather than from the entry's kind so that a member of an
/// archive is shown as the document it is rather than as a generic file.
fn header(name: &str, kind: &str) -> gtk::Box {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    let icon = gtk::Image::from_gicon(&gio::content_type_get_icon(kind));
    icon.set_widget_name("ufm-open-with-icon");
    icon.set_pixel_size(48);
    icon.set_valign(gtk::Align::Start);
    row.append(&icon);

    let names = gtk::Box::new(gtk::Orientation::Vertical, 3);
    names.set_valign(gtk::Align::Center);
    names.set_hexpand(true);
    let title = gtk::Label::new(Some(name));
    title.set_widget_name("ufm-open-with-title");
    title.set_wrap(true);
    title.set_xalign(0.0);
    title.set_selectable(true);
    names.append(&title);
    let what = gtk::Label::new(Some(&describe(kind)));
    what.set_widget_name("ufm-open-with-kind");
    what.set_xalign(0.0);
    what.add_css_class("dim-label");
    names.append(&what);
    row.append(&names);
    row
}

/// The honest empty state. A dialog that is simply empty reads as a program that
/// failed to load, and the one thing worth saying here is that nothing *is*
/// registered — which is also what `gio mime` says when asked the same question.
fn nothing_registered(kind: &str) -> gtk::Box {
    let box_ = gtk::Box::new(gtk::Orientation::Vertical, 3);
    let line = gtk::Label::new(Some("No application is registered for this file."));
    line.set_widget_name("ufm-open-with-none");
    line.set_wrap(true);
    line.set_xalign(0.0);
    box_.append(&line);
    let what = gtk::Label::new(Some(&describe(kind)));
    what.set_widget_name("ufm-open-with-none-kind");
    what.set_xalign(0.0);
    what.add_css_class("dim-label");
    box_.append(&what);
    box_
}

/// One candidate: its icon, its name, and the launch itself.
fn app_row(app: &gio::AppInfo, uri: &str, status: &gtk::Label) -> gtk::Button {
    let name = app.display_name().to_string();
    let button = gtk::Button::new();
    button.set_widget_name("ufm-open-with-app");
    button.set_halign(gtk::Align::Fill);

    let row = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    // The icon the desktop file names, or a generic one: an entry built from a
    // keyfile has no `Icon=` line, and a row with no picture at all reads as
    // broken rather than as an entry that did not say.
    let icon = app.icon().map_or_else(
        || gtk::Image::from_icon_name("application-x-executable"),
        |icon| gtk::Image::from_gicon(&icon),
    );
    icon.set_pixel_size(24);
    row.append(&icon);
    let title = gtk::Label::new(Some(&name));
    title.set_xalign(0.0);
    title.set_hexpand(true);
    title.set_ellipsize(gtk::pango::EllipsizeMode::End);
    row.append(&title);
    button.set_child(Some(&row));
    // A button whose child is a box has no label of its own, so the name a screen
    // reader reads has to be stated. Without this the row is reachable and
    // activatable but says nothing at all.
    button.update_property(&[gtk::accessible::Property::Label(&name)]);
    if let Some(comment) = app.description() {
        button.set_tooltip_text(Some(&format!("{name}: {comment}")));
    }

    button.connect_clicked({
        let app = app.clone();
        let uri = uri.to_string();
        let status = status.clone();
        let name = name.clone();
        move |_| {
            match app.launch_uris(&[&uri], None::<&gio::AppLaunchContext>) {
                Ok(()) => status.set_label(&format!("Started {name}.")),
                Err(error) => {
                    status.set_label(&format!("Cannot open with {name}: {error}"));
                }
            }
            status.set_visible(true);
        }
    });
    button
}

/// What a content type is called, falling back to the type itself when the mime
/// database has no description for it — which it has not for a type that is only a
/// suffix.
fn describe(kind: &str) -> String {
    let words = gio::content_type_get_description(kind);
    if words.is_empty() {
        kind.to_string()
    } else {
        words.to_string()
    }
}

/// Stream one member into `target`. The bytes are copied in chunks rather than
/// read whole, which is what keeps a 4 GiB member costing a buffer.
fn write_out(opened: &Opened, at: usize, target: &Path) -> ufm_core::Result<()> {
    let mut file = std::fs::File::create(target)?;
    opened.read_member(at, &mut |chunk| {
        file.write_all(chunk).map_err(ufm_core::Error::from)
    })?;
    Ok(())
}

/// Throw away materialised members old enough that no window can still be pointing
/// at one. Best-effort: a cache that fails to clean itself is not worth failing a
/// launch over, and the file the user asked for has already been written.
fn sweep(into: &Path) {
    let Ok(read) = std::fs::read_dir(into) else {
        return;
    };
    let now = SystemTime::now();
    for item in read.flatten() {
        let path = item.path();
        let old = path
            .metadata()
            .and_then(|meta| meta.modified())
            .is_ok_and(|when| too_old(&when, &now));
        if old {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// Whether a materialised member has been sitting here long enough to throw away.
/// A function of two times so that the rule can be tested without waiting a week.
fn too_old(when: &SystemTime, now: &SystemTime) -> bool {
    now.duration_since(*when).is_ok_and(|age| age >= AGE)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::tk;
    use std::path::PathBuf;
    use ufm_core::entry::{Entry, EntryFlags, EntryKind};

    /// The fixture the archive checks in the harness use, reached from the crate
    /// directory. A real container rather than a file named like one, because what
    /// is being asserted is the bytes that come out of a real reader.
    const FIXTURE: &str = "../../tests/archive-fixtures/nested.tar";

    fn fixture() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join(FIXTURE)
            .canonicalize()
            .expect("the archive fixture should be there")
    }

    fn on_disk(name: &str, mime: Option<&str>) -> Entry {
        Entry {
            location: Location::file(PathBuf::from("/tmp/t/tree").join(name)),
            name: name.to_string(),
            kind: EntryKind::File,
            flags: EntryFlags::WRITABLE,
            size: 4,
            allocated: None,
            modified: None,
            accessed: None,
            created: None,
            mode: Some(0o644),
            uid: None,
            gid: None,
            mime: mime.map(str::to_string),
            child_count: None,
            icon_key: "text/plain".to_string(),
        }
    }

    /// A desktop file that exists only in memory. `gio::DesktopAppInfo` accepts a
    /// keyfile with a name and an `Exec` line, which is the whole of what a
    /// candidate needs to be — so the rules below can be tested without installing
    /// anything into a system directory that every other test shares.
    fn synthetic(name: &str, nodisplay: bool) -> gio::AppInfo {
        let key = glib::KeyFile::new();
        key.set_string("Desktop Entry", "Type", "Application");
        key.set_string("Desktop Entry", "Name", name);
        key.set_string("Desktop Entry", "Exec", "true %u");
        key.set_string("Desktop Entry", "MimeType", "text/plain;");
        if nodisplay {
            key.set_boolean("Desktop Entry", "NoDisplay", true);
        }
        gio::DesktopAppInfo::from_keyfile(&key)
            .expect("a keyfile with a name and an exec line is an entry")
            .upcast()
    }

    #[test]
    fn the_type_comes_from_the_mime_the_reader_already_worked_out() {
        assert_eq!(
            content_type(&on_disk("note.txt", Some("text/plain"))),
            "text/plain"
        );
    }

    #[test]
    fn a_row_with_no_type_at_all_is_still_named_by_its_suffix() {
        // The reader derives a MIME type from the name, so a row without one is the
        // exception; guessing from the same name is what `GIO` itself does.
        assert_eq!(content_type(&on_disk("notes.txt", None)), "text/plain");
    }

    #[test]
    fn a_type_nothing_owns_says_so_rather_than_passing_as_a_real_one() {
        // The empty state is only honest if the type behind it is known to be a
        // guess. `application/octet-stream` is what a suffix nobody knows grows into.
        assert!(gio::content_type_is_unknown("application/octet-stream"));
        assert!(!gio::content_type_is_unknown("text/plain"));
    }

    #[test]
    fn one_program_is_one_row_even_when_it_is_reached_twice() {
        let twice = vec![synthetic("Probe Editor", false)];
        let again = vec![synthetic("Probe Editor", false)];
        assert_eq!(merge(twice, again).len(), 1);
    }

    #[test]
    fn an_entry_that_asks_not_to_be_shown_is_not_offered() {
        let hidden = vec![synthetic("Probe Hidden", true)];
        assert!(
            merge(hidden, Vec::new()).is_empty(),
            "a hidden entry is there to be found by name, not offered"
        );
    }

    #[test]
    fn a_fallback_counts_when_nothing_is_registered_for_the_type() {
        let fallback = vec![synthetic("Probe Text Editor", false)];
        let got = merge(Vec::new(), fallback);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].display_name(), "Probe Text Editor");
    }

    #[test]
    fn two_different_programs_stay_two_rows() {
        let all = vec![synthetic("Probe One", false), synthetic("Probe Two", false)];
        assert_eq!(merge(all, Vec::new()).len(), 2);
    }

    #[test]
    fn a_member_is_written_out_under_its_own_name_with_its_own_bytes() {
        let dir = tempfile::tempdir().expect("a cache directory to write into");
        let container = fixture();
        let location = Location::archive(&container, &["notes.txt"]).expect("a member");
        let path = materialise(&location, dir.path()).expect("the member should be written");
        assert_eq!(path.file_name().and_then(|n| n.to_str()), Some("notes.txt"));
        // 2120 bytes is what the fixture's `notes.txt` holds; the point is that real
        // bytes came through the reader rather than an empty file being created.
        let written = std::fs::read(&path).expect("the file should be readable");
        assert_eq!(written.len(), 2120, "the member should be written whole");
    }

    #[test]
    fn a_second_member_of_the_same_name_does_not_clobber_the_first() {
        let dir = tempfile::tempdir().expect("a cache directory to write into");
        let container = fixture();
        let location = Location::archive(&container, &["notes.txt"]).expect("a member");
        let first = materialise(&location, dir.path()).expect("the first copy");
        let second = materialise(&location, dir.path()).expect("the second copy");
        assert_ne!(first, second, "both should still be there");
        assert!(
            second
                .file_name()
                .is_some_and(|n| n.to_string_lossy().contains("(2)")),
            "the second copy should be named after the first: {second:?}"
        );
    }

    #[test]
    fn a_folder_inside_an_archive_is_not_something_to_write_out() {
        let dir = tempfile::tempdir().expect("a cache directory to write into");
        let container = fixture();
        let location = Location::archive(&container, &["docs"]).expect("a folder");
        assert!(
            materialise(&location, dir.path()).is_err(),
            "a folder has no bytes, and an empty file would be a lie"
        );
    }

    #[test]
    fn the_cache_forgets_what_is_a_week_old() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let fresh = now - Duration::from_secs(3600);
        let stale = now - AGE;
        assert!(
            !too_old(&fresh, &now),
            "a minute ago is still this session's"
        );
        assert!(
            too_old(&stale, &now),
            "a week of nothing pointing at it is enough"
        );
    }

    #[test]
    fn a_file_on_disk_is_something_to_open_and_a_folder_is_not() {
        assert!(openable(&on_disk("note.txt", Some("text/plain"))));
        let mut folder = on_disk("one", None);
        folder.kind = EntryKind::Directory;
        assert!(!openable(&folder), "a folder is entered, not opened");
    }

    #[test]
    fn a_member_inside_an_archive_is_something_to_open() {
        let container = fixture();
        let mut member = on_disk("notes.txt", Some("text/plain"));
        member.location =
            Location::archive(&container, &["notes.txt"]).expect("a member of the fixture");
        assert!(openable(&member), "it has bytes, once they are written out");

        // The root of an archive is the folder on screen, and a folder is entered.
        let mut root = member.clone();
        root.location = Location::archive(&container, &[] as &[&str]).expect("the root");
        root.kind = EntryKind::Archive;
        assert!(
            !openable(&root),
            "standing inside a container is not a file"
        );
    }

    #[test]
    fn the_chooser_names_the_file_and_what_it_is() {
        tk::ui(|| {
            let window = build("notes.txt", "text/plain", &[], "file:///tmp/notes.txt");
            let title = find(&window, "ufm-open-with-title");
            let label = title
                .and_downcast_ref::<gtk::Label>()
                .expect("the title should be a label");
            assert_eq!(label.label(), "notes.txt");
            let kind = find(&window, "ufm-open-with-kind");
            let what = kind
                .and_downcast_ref::<gtk::Label>()
                .expect("the kind should be a label");
            // The words come from the shared mime database, which is what this
            // dialog is for — it is the desktop's own answer, not ours. Asserting
            // the wording itself would tie a test to a data file, so what is
            // asserted is that the description was used rather than the bare type,
            // and that a type as common as this one has one.
            assert_eq!(what.label(), describe("text/plain"));
            assert_ne!(
                what.label(),
                "text/plain",
                "a common type has a description, and the header should show it"
            );
        });
    }

    #[test]
    fn a_chooser_with_nothing_registered_says_so() {
        tk::ui(|| {
            let window = build("notes.txt", "text/plain", &[], "file:///tmp/notes.txt");
            assert!(
                find(&window, "ufm-open-with-none").is_some(),
                "an empty list has to explain itself"
            );
            assert!(find(&window, "ufm-open-with-app").is_none());
        });
    }

    #[test]
    fn each_registered_program_gets_a_row_that_says_its_name() {
        tk::ui(|| {
            let apps = [synthetic("Probe Editor", false)];
            let window = build("notes.txt", "text/plain", &apps, "file:///tmp/notes.txt");
            let row =
                find(&window, "ufm-open-with-app").expect("one candidate should draw one row");
            // The row is a button holding a box rather than a button holding a
            // label, because these bindings give `GTK4`'s button no image setter.
            // That is fine for the eye and a problem for the accessibility tree,
            // where a button with a box inside it has no name — so the name has to
            // be both drawn and stated, and both are checked here.
            let button = row
                .downcast_ref::<gtk::Button>()
                .expect("the row should be a button");
            assert!(button.child().is_some(), "the row draws icon and name");
            let mut said = Vec::new();
            labels(&row, &mut said);
            assert!(
                said.iter().any(|text| text == "Probe Editor"),
                "the row should say which program it is: {said:?}"
            );
            assert!(
                find(&window, "ufm-open-with-close").is_some(),
                "the chooser should be closable from itself"
            );
        });
    }

    /// The first widget named `name`, depth-first. Same shape as the properties
    /// dialog's helper: a chooser is a tree of boxes and only the names are stable.
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

    /// Every label under `root`, in the order they are drawn. Used to read what a
    /// row actually says, since a row's text is what a user — or a screen reader —
    /// is offered.
    fn labels<W: IsA<gtk::Widget>>(root: &W, out: &mut Vec<String>) {
        let root: &gtk::Widget = root.as_ref();
        for child in root.observe_children().snapshot() {
            let Ok(widget) = child.downcast::<gtk::Widget>() else {
                continue;
            };
            if let Some(label) = widget.downcast_ref::<gtk::Label>() {
                out.push(label.text().to_string());
            }
            labels(&widget, out);
        }
    }
}
