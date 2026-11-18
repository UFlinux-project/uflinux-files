//! What this row actually is, without leaving the folder.
//!
//! The window uses an embedded inspector. The older popover implementation below
//! remains as a compatibility reference for its focused widget tests; no window
//! installs it. Both presentations use the same bounded content builders.
//!
//! The original spacebar preview: a popover over the selected row that says what the row
//! holds — its type, its size, when it changed, and when possible the thing
//! itself: the picture for an image, the opening lines for a text file. The
//! point is the answer without a detour. Opening a document means waiting for an
//! application to start, and Properties means a dialog and a walk; a user
//! deciding which of six files to open wants neither.
//!
//! The popover is built fresh on every opening rather than updated in place,
//! because what a preview holds depends so completely on the row: a folder has
//! no text, a text file no picture, and a file on a share no local bytes to
//! read. Rebuilding is a handful of widgets, and the alternative is a widget
//! tree with holes in it that every rule has to remember to refill.
//!
//! Two of the three parts travel through machinery that already exists. The
//! picture is one request to the window's thumbnail [`Pipeline`], which already
//! owns the worker, the queue and the token table, and already drops an answer
//! whose widget has gone away — the ordinary fate of a preview dismissed before
//! its decode finished. The text is read on a worker of its own and reported
//! through the listing queue as [`Event::Preview`], because file I/O on the main
//! thread is what this whole program exists to avoid.
//!
//! The picture is asked for at [`EDGE`], which [`ufm_thumb`] files under the
//! spec's `large` band rather than the `normal` one the icons live in. That
//! separation is what makes the big request safe: both this popover and the icon
//! pipeline paint cached bytes at the size they were decoded for, so a preview
//! reading an icon-sized cache entry would paint a 16 px picture at thirty
//! times its size, and would look like a rendering bug rather than a cache one.

use std::cell::{Cell, RefCell};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use gtk::prelude::*;

use ufm_core::entry::Entry;

use super::job::{Event, Handle};
use super::row::Row;
use super::thumb::Pipeline;

/// Large enough for the inspector while keeping a decoded image bounded on a
/// small window. This remains in the thumbnail cache's large band.
const EDGE: u32 = 320;

/// How much of a text file is read. A preview is not a pager; the opening
/// kilobytes say whether this is the file the user was looking for, and reading
/// more would cost memory the popover has no use for.
const HEAD_BYTES: usize = 64 * 1024;

/// The tallest the text pane grows before it scrolls. Chosen so the popover
/// stays comfortably smaller than a window on a 768 px screen — the smallest
/// this program is designed for.
const TEXT_HEIGHT: i32 = 240;

/// A text read in flight, and the label its answer belongs to.
///
/// The label is held strongly and the window only as a key: an answer that
/// arrives after its popover was dismissed writes into a label that is no longer
/// in any widget tree, which costs one string copy and nothing else. What the
/// window pointer is for is [`forget`], so dismissing a preview drops its read
/// from the table instead of letting the table grow by one per opening.
struct Pending {
    token: u64,
    label: gtk::Label,
    window: usize,
}

thread_local! {
    static PENDING: RefCell<Vec<Pending>> = const { RefCell::new(Vec::new()) };
    static NEXT_TOKEN: Cell<u64> = const { Cell::new(1) };
}

/// The window's preview popover, built once and refilled on every opening.
///
/// One per window for the same reason the context menu is one per window: it is
/// parented to the window so that `win.` actions and the window's own coordinate
/// space apply to it, and a popover with no other owner is freed the moment the
/// call that built it returns.
#[derive(Debug, Clone)]
pub struct Preview {
    pop: gtk::Popover,
}

impl Preview {
    /// Build the popover, closed. Parenting goes through [`gtk::Widget::set_parent`]
    /// for the reason [`super::popup::Popup::install`] gives: the binding has no
    /// setter for a popover's `parent` property.
    #[must_use]
    pub fn install(window: &gtk::ApplicationWindow) -> Rc<Self> {
        let pop = gtk::Popover::new();
        pop.set_parent(window);
        pop.set_has_arrow(false);
        // Close on a click anywhere else, and on `Escape`. Without this the
        // popover is dismissed only by the same key that opened it, which is
        // wrong for a pointer user and surprising for one who has just used
        // `Escape` on every other popover in the program.
        pop.set_autohide(true);
        Rc::new(Self { pop })
    }

    #[must_use]
    pub fn is_visible(&self) -> bool {
        self.pop.is_visible()
    }

    /// Close it and drop any text read waiting on it.
    pub fn hide(&self, window: &gtk::ApplicationWindow) {
        forget(window);
        self.pop.popdown();
    }

    /// Open the preview for `entry`, or close this one if it is already open.
    ///
    /// The same key opens and closes: a popover that only ever appears makes a
    /// user hunt for its dismiss, and there is nothing here to dismiss beyond
    /// the popover itself.
    pub fn toggle(
        &self,
        window: &gtk::ApplicationWindow,
        handle: &Handle,
        thumbs: &Pipeline,
        entry: &Entry,
    ) {
        if self.pop.is_visible() {
            self.hide(window);
            return;
        }
        forget(window);
        let content = body(window, handle, thumbs, entry);
        self.pop.set_child(Some(&content));
        if let Some(rect) = pointing_to(window) {
            self.pop.set_pointing_to(Some(&rect));
        }
        self.pop.popup();
    }
}

/// A text file's opening bytes have arrived; put them where they were asked for.
///
/// Called from the window's event handling. An answer with nothing waiting for
/// it is the ordinary outcome of a preview dismissed mid-read and is dropped.
pub fn arrived(token: u64, text: Option<&str>) {
    PENDING.with(|pending| {
        let list = pending.borrow();
        if let Some(item) = list.iter().find(|item| item.token == token) {
            item.label.set_label(match text {
                Some("") => "(empty file)",
                Some(text) => text,
                None => "(cannot be shown)",
            });
        }
    });
}

/// Drop this window's outstanding reads, so an answer to a dismissed preview is
/// not even looked up. Answers arriving after a dismissal are harmless either
/// way — see [`Pending`] — this only keeps the table from growing.
fn forget(window: &gtk::ApplicationWindow) {
    let key = window.as_ptr() as usize;
    PENDING.with(|pending| pending.borrow_mut().retain(|item| item.window != key));
}

/// The popover's whole content, rebuilt for one entry.
fn body(
    window: &gtk::ApplicationWindow,
    handle: &Handle,
    thumbs: &Pipeline,
    entry: &Entry,
) -> gtk::Box {
    let box_ = gtk::Box::new(gtk::Orientation::Vertical, 8);
    box_.set_widget_name("ufm-preview");
    box_.add_css_class("ufm-preview");
    box_.set_margin_top(10);
    box_.set_margin_start(12);
    box_.set_margin_end(12);
    box_.set_margin_bottom(10);
    box_.append(&header(entry));
    box_.append(&facts(entry));
    if let Some(picture) = picture(thumbs, entry) {
        box_.append(&picture);
    }
    if let Some(text) = text(window, handle, entry) {
        box_.append(&text);
    }
    box_
}

/// The row's own icon, its name, and what it is — the same shape the properties
/// dialog and the chooser lead with, so the three agree about what a row is
/// before any of them say anything else.
fn header(entry: &Entry) -> gtk::Box {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    let icon = super::symbolic_image(entry.kind.icon_name(), "text-x-generic-symbolic", 32);
    icon.set_widget_name("ufm-preview-icon");
    icon.set_pixel_size(32);
    icon.set_valign(gtk::Align::Start);
    row.append(&icon);

    let names = gtk::Box::new(gtk::Orientation::Vertical, 2);
    names.set_valign(gtk::Align::Center);
    names.set_hexpand(true);
    let title = gtk::Label::new(Some(&entry.name));
    title.set_widget_name("ufm-preview-title");
    title.set_wrap(true);
    title.set_xalign(0.0);
    title.set_selectable(true);
    names.append(&title);
    let kind = gtk::Label::new(Some(&entry.kind_label()));
    kind.set_widget_name("ufm-preview-kind");
    kind.set_xalign(0.0);
    kind.add_css_class("dim-label");
    names.append(&kind);
    row.append(&names);
    row
}

/// The facts the listing already carries, as `key  value` lines. No walk and no
/// stat: everything here came up with the directory read, which is what makes
/// the popover instant on a folder of ten thousand rows.
fn facts(entry: &Entry) -> gtk::Grid {
    let grid = gtk::Grid::new();
    grid.set_row_spacing(2);
    grid.set_column_spacing(12);
    // Zipped with a counter of the right type rather than `enumerate`: the grid
    // wants `i32` coordinates and a preview has a handful of rows, so the range
    // cannot run out before the facts do.
    for (row, (key, value)) in (0_i32..).zip(facts_of(entry)) {
        grid.attach(&key_label(&key), 0, row, 1, 1);
        grid.attach(&value_label(&value), 1, row, 1, 1);
    }
    grid
}

/// The facts themselves, in reading order, with the rows the entry cannot fill
/// left out — the same rule the properties form follows, so a filesystem with
/// no mode bits reads as a short preview rather than as a broken one.
fn facts_of(entry: &Entry) -> Vec<(String, String)> {
    let mut rows = vec![
        ("Size".to_string(), entry.size_label()),
        ("Modified".to_string(), entry.modified_label()),
    ];
    if let Some(mode) = entry.permissions() {
        rows.push(("Permissions".to_string(), mode));
    }
    rows
}

fn key_label(text: &str) -> gtk::Label {
    let label = gtk::Label::new(Some(text));
    label.set_xalign(1.0);
    label.add_css_class("dim-label");
    label
}

fn value_label(text: &str) -> gtk::Label {
    let label = gtk::Label::new(Some(text));
    label.set_widget_name("ufm-preview-fact");
    label.set_xalign(0.0);
    label.set_selectable(true);
    label
}

/// The picture pane, when this row is a picture the pipeline will take.
///
/// The request goes to the window's shared [`Pipeline`] rather than to a decode
/// of this module's own: the pipeline already queues, decodes on one worker,
/// matches answers to widgets by token, and drops answers whose widget is gone —
/// which is exactly the lifecycle a dismissed preview needs. The gate inside
/// [`Pipeline::request`] is also the preference: with picture previews switched
/// off the request is refused and the popover shows the facts alone, which is
/// what the switch promises.
fn picture(thumbs: &Pipeline, entry: &Entry) -> Option<gtk::Image> {
    let image = gtk::Image::new();
    image.set_widget_name("ufm-preview-image");
    let row = Row::new(entry.clone());
    // The pixel size only matters until a texture arrives; the texture then
    // draws at its own size, which is at most `EDGE`.
    image.set_pixel_size(EDGE.cast_signed());
    thumbs
        .request(&image, &row, EDGE.cast_signed())
        .then_some(image)
}

/// The text pane: a placeholder label now, the file's opening bytes when the
/// worker brings them. `None` when the row is not something worth reading as
/// text, which is most rows — and the check that decides it is [`textual`], not
/// an attempt that might read a megabyte of a database file.
fn text(window: &gtk::ApplicationWindow, handle: &Handle, entry: &Entry) -> Option<gtk::Widget> {
    let path = text_source(entry)?;
    let label = gtk::Label::new(Some("(reading\u{2026})"));
    label.set_widget_name("ufm-preview-text");
    label.set_xalign(0.0);
    label.set_yalign(0.0);
    label.set_wrap(true);
    label.set_selectable(true);
    label.set_max_width_chars(72);
    label.set_width_chars(0);
    label.add_css_class("ufm-mono");

    let scroll = gtk::ScrolledWindow::new();
    scroll.set_widget_name("ufm-preview-scroll");
    scroll.set_policy(gtk::PolicyType::Automatic, gtk::PolicyType::Automatic);
    scroll.set_min_content_height(TEXT_HEIGHT);
    scroll.set_max_content_height(TEXT_HEIGHT);
    scroll.set_propagate_natural_width(false);
    scroll.set_child(Some(&label));

    let token = NEXT_TOKEN.with(|next| {
        let token = next.get();
        next.set(token + 1);
        token
    });
    PENDING.with(|pending| {
        pending.borrow_mut().push(Pending {
            token,
            label,
            window: window.as_ptr() as usize,
        });
    });
    start(handle, token, &path);
    Some(scroll.upcast())
}

/// Where a text preview reads from, when this row has bytes that could be text.
///
/// A member of an archive is deliberately absent: reading it would mean opening
/// the container on a worker while the view is browsing through it, which is the
/// borrow [`super::openwith`] solves by writing the member out — far too much
/// work for a glance. The facts above still say what the member is.
fn text_source(entry: &Entry) -> Option<PathBuf> {
    if entry.kind != ufm_core::entry::EntryKind::File
        || entry.flags.contains(ufm_core::entry::EntryFlags::BROKEN)
        || !entry.location.is_real()
    {
        return None;
    }
    let mime = entry.mime.as_deref()?;
    textual(mime)
        .then(|| entry.location.as_path())?
        .map(PathBuf::from)
}

/// Whether a MIME type names something worth showing as text.
///
/// `text/` is the honest majority. The rest are types whose data is text but
/// whose registration is not: JSON, XML and shell scripts are the ones a file
/// manager actually meets, and each is registered under `application/` by the
/// shared mime database. Anything else — a PDF, a database, a photo — gets the
/// facts and no pane, which is the right answer rather than a failure.
fn textual(mime: &str) -> bool {
    mime.starts_with("text/")
        || matches!(
            mime,
            "application/json"
                | "application/xml"
                | "application/javascript"
                | "application/x-shellscript"
                | "application/x-empty"
        )
}

/// Read the head of `path` on a worker and report it as [`Event::Preview`].
fn start(handle: &Handle, token: u64, path: &Path) {
    let worker = handle.clone();
    // `to_path_buf` rather than `clone`: on a `&Path` the latter copies the
    // reference, which is not something a worker may outlive the popover with.
    let path = path.to_path_buf();
    let spawned = std::thread::Builder::new()
        .stack_size(64 * 1024)
        .spawn(move || {
            worker.send(Event::Preview {
                token,
                text: head(&path),
            });
        });
    if spawned.is_err() {
        handle.send(Event::Preview { token, text: None });
    }
}

/// The opening bytes of `path`, decoded, or `None` when there is no text to
/// show: unreadable, or binary.
///
/// The read stops at [`HEAD_BYTES`] plus one, so a 2 MiB document costs the
/// same as a 2 KiB one. A NUL byte in what was read settles the question of
/// whether this is text: UTF-8 tolerates it, and every real text file this
/// program meets is either UTF-8 or something the user did not want a preview
/// of.
fn head(path: &Path) -> Option<String> {
    let mut file = std::fs::File::open(path).ok()?;
    let mut buffer = Vec::with_capacity(HEAD_BYTES + 1);
    file.by_ref()
        .take((HEAD_BYTES + 1) as u64)
        .read_to_end(&mut buffer)
        .ok()?;
    let truncated = buffer.len() > HEAD_BYTES;
    if truncated {
        buffer.truncate(HEAD_BYTES);
    }
    if buffer.contains(&0) {
        return None;
    }
    let mut text = String::from_utf8_lossy(&buffer).into_owned();
    if truncated {
        text.push_str("\n\u{2026}");
    }
    Some(text)
}

/// Where to point the popover: the focused widget's own bounds, which
/// [`gtk::Widget::compute_bounds`] already reports in window coordinates. The
/// preview is a keyboard affordance — the pointer has the row itself to look
/// at — so there is no press to point at, and the focused row is both the
/// honest target and the one that needs no cursor.
///
/// The fallback is for the case where there is no focused widget to point at:
/// a window that has just had a dialog or chooser close over it. Pointing at
/// the middle of the window is not where the row is, but it is where the window
/// is, and a popover that appears inside the window it belongs to is a preview
/// the user can read rather than one anchored to nothing.
fn pointing_to(window: &gtk::ApplicationWindow) -> Option<gtk::gdk::Rectangle> {
    // Qualified for the reason `popup.rs` gives: an `ApplicationWindow` answers
    // `focus` to two traits and the window's own answer is the right one.
    let target = gtk::prelude::GtkWindowExt::focus(window)
        .and_then(|focus| focus.compute_bounds(window))
        .or_else(|| window.compute_bounds(window))?;
    let corners = target.vertices();
    let from = corners[0].to_float();
    let to = corners[2].to_float();
    Some(gtk::gdk::Rectangle::new(
        f64::from(from[0] + (to[0] - from[0]) / 2.0).round() as i32,
        f64::from(from[1] + (to[1] - from[1]) / 2.0).round() as i32,
        1,
        1,
    ))
}

/// Selection inspector embedded beside the listing. The older popover above is
/// retained for its existing widget tests, but windows use this pane.
pub struct Inspector {
    root: gtk::Box,
    content: gtk::Box,
    video: RefCell<Option<gtk::Video>>,
}

impl Inspector {
    pub fn new() -> Self {
        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        root.add_css_class("ufm-inspector");
        root.set_widget_name("ufm-inspector");
        root.set_size_request(260, -1);
        let title = gtk::Label::new(Some("Preview"));
        title.add_css_class("heading");
        title.set_xalign(0.0);
        title.set_margin_top(14);
        title.set_margin_start(14);
        title.set_margin_bottom(8);
        root.append(&title);
        let scroll = gtk::ScrolledWindow::new();
        scroll.set_vexpand(true);
        let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
        content.set_margin_start(14);
        content.set_margin_end(14);
        scroll.set_child(Some(&content));
        root.append(&scroll);
        let inspector = Self {
            root,
            content,
            video: RefCell::new(None),
        };
        inspector.placeholder("Select a file to preview");
        inspector
    }

    pub fn widget(&self) -> &gtk::Box {
        &self.root
    }

    pub fn clear(&self, window: &gtk::ApplicationWindow) {
        forget(window);
        if let Some(video) = self.video.borrow_mut().take() {
            if let Some(stream) = video.media_stream() {
                stream.pause();
            }
            video.set_filename(None::<&Path>);
        }
        while let Some(child) = self.content.first_child() {
            self.content.remove(&child);
        }
    }

    fn placeholder(&self, message: &str) {
        let label = gtk::Label::new(Some(message));
        label.add_css_class("dim-label");
        label.set_wrap(true);
        label.set_margin_top(24);
        self.content.append(&label);
    }

    pub fn show_entries(
        &self,
        window: &gtk::ApplicationWindow,
        handle: &Handle,
        thumbs: &Pipeline,
        entries: &[Entry],
    ) {
        self.clear(window);
        let [entry] = entries else {
            if entries.is_empty() {
                self.placeholder("Select a file to preview");
            } else {
                self.placeholder(&format!("{} items selected", entries.len()));
            }
            return;
        };
        self.content.append(&body(window, handle, thumbs, entry));
        if super::trash::is_stored(&entry.location) {
            let details = gtk::Label::new(None);
            details.set_wrap(true);
            details.set_selectable(true);
            details.set_xalign(0.0);
            self.content.append(&details);
            let location = entry.location.clone();
            let details = details.downgrade();
            glib::spawn_future_local(async move {
                let item = gio::spawn_blocking(move || super::trash::find(&location)).await;
                if let (Some(details), Ok(Some(item))) = (details.upgrade(), item) {
                    details.set_text(&format!(
                        "Original location: {}\nDeleted: {}",
                        item.original.display(),
                        item.deleted
                    ));
                }
            });
        }

        let is_media = entry
            .mime
            .as_deref()
            .is_some_and(|m| m.starts_with("video/") || m.starts_with("audio/"));
        if is_media && let Some(path) = entry.location.as_path() {
            let play = gtk::Button::with_label("Play preview");
            play.set_margin_top(12);
            let video = gtk::Video::new();
            video.set_autoplay(false);
            video.set_size_request(240, 160);
            video.set_visible(false);
            let source = path.to_path_buf();
            let player = video.clone();
            play.connect_clicked(move |button| {
                player.set_filename(Some(&source));
                player.set_visible(true);
                if let Some(stream) = player.media_stream() {
                    stream.play();
                }
                button.set_visible(false);
            });
            self.content.append(&play);
            self.content.append(&video);
            *self.video.borrow_mut() = Some(video);
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::job::Queue;
    use crate::ui::tk;
    use ufm_core::Location;
    use ufm_core::entry::{EntryFlags, EntryKind};

    fn on_disk(name: &str, mime: Option<&str>) -> Entry {
        Entry {
            location: Location::file(Path::new("/tmp/t/tree").join(name)),
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

    fn folder(name: &str) -> Entry {
        let mut entry = on_disk(name, None);
        entry.kind = EntryKind::Directory;
        entry.flags = EntryFlags::empty();
        entry.icon_key = "folder".to_string();
        entry
    }

    /// The first line of a fact list, whatever the rows turn out to be: every
    /// row is a pair, and the test below cares about the keys, not the values
    /// the fixture's timestamps would print.
    fn keys(entry: &Entry) -> Vec<String> {
        facts_of(entry).into_iter().map(|(key, _)| key).collect()
    }

    #[test]
    fn every_row_gets_size_and_modified_even_when_later_rows_are_missing() {
        // The two rows an entry always has room to say something about, even a
        // virtual one with no mode and no timestamp — they print an em dash,
        // which is the preview saying "unknown" rather than saying nothing.
        let bare = Entry {
            location: Location::computer(),
            name: "Computer".to_string(),
            kind: EntryKind::MountPoint,
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
            icon_key: "computer".to_string(),
        };
        assert_eq!(keys(&bare), ["Size", "Modified"]);
    }

    #[test]
    fn a_file_with_mode_bits_says_so_and_a_folder_still_gets_a_preview() {
        assert!(
            keys(&on_disk("note.txt", Some("text/plain"))).contains(&"Permissions".to_string())
        );
        // A folder is the common case for the key, and the popover answers it
        // with its child count — the same number the list view's column says.
        let mut dir = folder("one");
        dir.child_count = Some(3);
        assert_eq!(keys(&dir), ["Size", "Modified", "Permissions"]);
    }

    #[test]
    fn text_is_decided_by_the_type_and_not_by_a_guess_at_the_bytes() {
        assert!(textual("text/plain"));
        assert!(textual("text/x-shellscript"));
        // The `application/` types the shared mime database registers for data
        // that is text, and which a user would expect a preview of.
        assert!(textual("application/json"));
        assert!(textual("application/x-shellscript"));
        // And the types a preview would get wrong: a PDF's first kilobyte is
        // binary noise, a photo's is a header, a directory has no bytes.
        assert!(!textual("application/pdf"));
        assert!(!textual("image/png"));
        assert!(!textual("application/octet-stream"));
    }

    #[test]
    fn only_a_real_plain_file_is_offered_as_text() {
        assert!(text_source(&on_disk("note.txt", Some("text/plain"))).is_some());
        // A folder has no bytes; a broken link's target is not this row's
        // promise; a member of an archive would cost opening the container.
        assert!(text_source(&folder("one")).is_none());
        let mut broken = on_disk("gone.txt", Some("text/plain"));
        broken.flags |= EntryFlags::BROKEN;
        assert!(text_source(&broken).is_none());
        let mut inner = on_disk("member.txt", Some("text/plain"));
        inner.location = Location::archive(Path::new("/tmp/t/a.zip"), &["member.txt"])
            .expect("a member of a container");
        assert!(
            text_source(&inner).is_none(),
            "an archive member stays behind the container"
        );
        // A row the reader could not type gets no pane: guessing from the
        // suffix would read a file named `data.txt` that holds a database.
        assert!(text_source(&on_disk("mystery", None)).is_none());
    }

    #[test]
    fn a_text_head_is_cut_at_the_cap_and_marked() {
        let dir = tempfile::tempdir().expect("a directory to read from");
        let path = dir.path().join("big.txt");
        std::fs::write(&path, vec![b'a'; HEAD_BYTES + 500]).expect("a file to read");
        let got = head(&path).expect("the head should read");
        assert!(got.len() <= HEAD_BYTES + "\n\u{2026}".len());
        assert!(
            got.ends_with('\u{2026}'),
            "a cut head should say it was cut"
        );
    }

    #[test]
    fn a_small_file_is_shown_whole_and_unmarked() {
        let dir = tempfile::tempdir().expect("a directory to read from");
        let path = dir.path().join("small.txt");
        std::fs::write(&path, b"hello").expect("a file to read");
        assert_eq!(head(&path).as_deref(), Some("hello"));
    }

    #[test]
    fn bytes_with_a_nul_are_not_shown_as_text() {
        // UTF-8 accepts a NUL, so the encoding cannot be the test. Every binary
        // this program meets has one in its first kilobyte, and every text file
        // it meets has none.
        let dir = tempfile::tempdir().expect("a directory to read from");
        let path = dir.path().join("blob.txt");
        std::fs::write(&path, [b'x', 0, b'y']).expect("a file to read");
        assert_eq!(head(&path), None);
    }

    #[test]
    fn a_file_that_cannot_be_opened_previews_as_nothing_rather_than_as_an_error() {
        // The same answer a missing thumbnail gives: the popover has the facts
        // either way, and a pane that says "cannot be shown" is enough.
        assert_eq!(head(Path::new("/tmp/t/definitely-not-here.txt")), None);
    }

    #[test]
    fn an_empty_file_previews_as_empty_rather_than_as_nothing() {
        let dir = tempfile::tempdir().expect("a directory to read from");
        let path = dir.path().join("empty.txt");
        std::fs::write(&path, b"").expect("a file to write");
        assert_eq!(head(&path).as_deref(), Some(""));
    }

    #[test]
    fn the_popover_holds_the_row_s_facts_and_nothing_else_until_a_reply_arrives() {
        tk::ui(|| {
            let app = gtk::Application::new(None::<&str>, gio::ApplicationFlags::NON_UNIQUE);
            app.register(None::<&gio::Cancellable>)
                .expect("test application");
            let window = gtk::ApplicationWindow::new(&app);
            let listing = gtk::Button::with_label("listing");
            window.set_child(Some(&listing));
            window.present();
            listing.grab_focus();
            let preview = Preview::install(&window);
            assert!(!preview.is_visible(), "a fresh preview is closed");
            // A pipeline with no handle refuses requests, which is the state a
            // test is in: the picture pane is then absent, and what is left is
            // exactly the part that must never depend on a decode.
            let thumbs = Pipeline::new();
            let queue = Queue::without_source().expect("a pipe can be made");
            let entry = on_disk("note.txt", Some("application/pdf"));
            preview.toggle(&window, &queue.handle(), &thumbs, &entry);
            assert!(preview.is_visible(), "toggle opens it");
            let content = preview.pop.child().expect("the popover should have a body");
            let names = find(&content, "ufm-preview-title").expect("the title should be there");
            let title = names.downcast_ref::<gtk::Label>().expect("a label");
            assert_eq!(title.text(), "note.txt");
            // A PDF gets no text pane and, with no pipeline handle, no picture
            // pane — only the facts, which is the honest minimum.
            assert!(find(&content, "ufm-preview-text").is_none());
            assert!(find(&content, "ufm-preview-image").is_none());
            preview.hide(&window);
            assert!(!preview.is_visible(), "hide closes it");
            window.close();
        });
    }

    #[test]
    fn a_text_row_gets_a_placeholder_and_its_reply_lands_in_it() {
        tk::ui(|| {
            let app = gtk::Application::builder()
                .application_id("org.uflinux.FileManager.preview.text.test")
                .build();
            let window = gtk::ApplicationWindow::new(&app);
            let preview = Preview::install(&window);
            let thumbs = Pipeline::new();
            let queue = Queue::without_source().expect("a pipe can be made");
            let entry = on_disk("note.txt", Some("text/plain"));
            preview.toggle(&window, &queue.handle(), &thumbs, &entry);
            let content = preview.pop.child().expect("a body");
            let node = find(&content, "ufm-preview-text").expect("a text pane for a text file");
            let label = node.downcast_ref::<gtk::Label>().expect("a label");
            assert_eq!(label.text(), "(reading\u{2026})", "the pane starts honest");
            // The reply, delivered the way the window's event handling does.
            let token = PENDING
                .with(|pending| pending.borrow().first().map(|item| item.token))
                .expect("the read should be registered");
            arrived(token, Some("hello"));
            assert_eq!(label.text(), "hello");
            // An answer for a preview that has gone is dropped, not painted
            // somewhere else.
            preview.hide(&window);
            arrived(token, Some("late"));
            assert_eq!(
                label.text(),
                "hello",
                "a dismissed preview keeps what it said"
            );
        });
    }

    #[test]
    fn opening_twice_closes_and_reopening_forgets_the_previous_read() {
        tk::ui(|| {
            let app = gtk::Application::builder()
                .application_id("org.uflinux.FileManager.preview.toggle.test")
                .build();
            let window = gtk::ApplicationWindow::new(&app);
            let preview = Preview::install(&window);
            let thumbs = Pipeline::new();
            let queue = Queue::without_source().expect("a pipe can be made");
            let entry = on_disk("note.txt", Some("text/plain"));
            preview.toggle(&window, &queue.handle(), &thumbs, &entry);
            preview.toggle(&window, &queue.handle(), &thumbs, &entry);
            assert!(!preview.is_visible(), "the same key that opens also closes");
            preview.toggle(&window, &queue.handle(), &thumbs, &entry);
            let stale = PENDING.with(|pending| pending.borrow().len());
            assert_eq!(stale, 1, "one open preview is one outstanding read");
            preview.hide(&window);
        });
    }

    /// Depth-first search by widget name, the way the other UI modules read
    /// their own trees.
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
