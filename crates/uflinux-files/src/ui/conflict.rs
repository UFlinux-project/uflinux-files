//! Asking what to do when the folder being pasted into already has that name.
//!
//! [`ops::plan`] refuses to guess on its own: it records every collision it
//! finds and still writes the step under a free name, so a caller with no way to
//! ask has something to do and a caller with a window has something to show. This
//! module is the asking.
//!
//! The prompt is a plain [`gtk::Window`], the same shape [`super::connect`] and
//! the name prompts use — `GTK4` has no dialog class left to subclass — and it is
//! driven synchronously rather than by a callback chain. That is the unusual part,
//! and it is deliberate. A paste is a *sequence*: the answer to the third
//! collision can be reached only after the first two have been answered, and the
//! order the questions get asked has to be the order the work happens. Expressed
//! as callbacks that is a state machine per operation; expressed as a nested loop
//! it is a `while`, and [`ops::resolve`] can then be a plain function over a plan
//! — which is what makes answering testable without a display.
//!
//! The loop is [`glib::MainContext::iteration`] rather than a private poll: every
//! source the window owns keeps running while the prompt is up, so the listing
//! behind it still repaints, a watch event still invalidates, and a paste that was
//! already running still reports. What does not happen is reentrancy into the same
//! command: the prompt is modal, and the actions that would start another paste
//! belong to the window behind it.

use std::cell::Cell;
use std::rc::Rc;

use glib::Propagation;
use gtk::prelude::*;

use ufm_core::config::format_modified;
use ufm_core::entry::human_size;

use super::job::Handle;
use super::jobs::Jobs;
use super::ops::{self, Choose, Conflict, Plan, Side, Transfer};
use super::row::format_items;

/// Run a paste, stopping for the collisions it runs into.
///
/// A plan with nothing to ask about — the common case, and the only case a folder
/// of new files has — reaches [`ops::paste`] without ever building a window. That
/// matters more than it looks: a prompt that exists but is never shown still costs
/// a widget tree, and a paste of a thousand files would pay for it a thousand
/// times over.
///
/// `jobs` is the window's table, which the paste registers itself in once the
/// questions are answered. Answering a prompt can take a minute and the strip
/// stays quiet through it — a job that has not been agreed to yet is not a job
/// the user is waiting on.
pub fn paste(
    parent: &gtk::ApplicationWindow,
    handle: &Handle,
    jobs: &Jobs,
    plan: Plan,
    transfer: Transfer,
) {
    if plan.conflicts.is_empty() {
        ops::paste(handle, jobs, plan, transfer);
        return;
    }
    let prompt = Prompt::default();
    let answered = ops::resolve(plan, |conflict| prompt.ask(parent, conflict));
    ops::paste(handle, jobs, answered, transfer);
}

/// One prompt's worth of state: the answer, shared with the window that carries it.
///
/// The answer outlives the window — the loop reads it after the window is gone —
/// so it lives outside rather than being read back out of a widget that may
/// already be destroyed.
#[derive(Default)]
struct Prompt {
    answer: Rc<Cell<Option<Choose>>>,
}

impl Prompt {
    /// Show the prompt for `conflict` and wait for the answer.
    ///
    /// A window that is closed rather than answered — `Escape`, the window
    /// manager, or a parent that went away — counts as a cancel, which is the
    /// answer that destroys nothing.
    fn ask(&self, parent: &gtk::ApplicationWindow, conflict: &Conflict) -> Choose {
        let answer = Rc::clone(&self.answer);
        answer.set(None);
        let form = compose(Some(parent), conflict, &answer);
        if let Some(app) = parent.application() {
            app.add_window(&form.window);
        }
        form.window.present();
        wait(&form.window, &answer);
        // The window is dropped here rather than kept: a prompt that has been
        // answered has nothing left to show, and a `gtk::Window` still alive after
        // that is a widget tree held for the rest of the paste.
        self.answer.take().unwrap_or(Choose::Cancel)
    }
}

/// Run the main context until the prompt is answered.
///
/// Blocking is safe here because the caller owns the context: this runs on the
/// `GTK` thread inside the action handler that opened the prompt, and `GLib`
/// releases ownership of the context while dispatching, which is exactly what lets
/// a nested iteration run. This is how `gtk_dialog_run` worked, and it is why no
/// second thread is needed to keep a window alive while it waits.
///
/// An answer is the only way out, and every way a prompt can end sets one: a
/// button records its own, and [`wire`] records a cancel for a close nobody
/// answered. The visibility check is the backstop for the one ending that sets
/// nothing — a parent closing takes a `destroy-with-parent` child down without a
/// close request — and the caller reads a missing answer as a cancel too, so a
/// prompt the compositor never showed cannot hold a window locked forever.
fn wait(window: &gtk::Window, answer: &Cell<Option<Choose>>) {
    let context = glib::MainContext::default();
    while answer.get().is_none() && window.get_visible() {
        context.iteration(true);
    }
}

/// Every widget the prompt owns, so that laying them out and wiring them can be
/// two readable steps. The answer cell is shared with whoever built the prompt,
/// which is what lets a test press a button and read the result.
struct Form {
    window: gtk::Window,
    cancel: gtk::Button,
    skip: gtk::Button,
    rename: gtk::Button,
    replace: gtk::Button,
    /// The "apply to the rest" checkbox, present only when there is a rest to
    /// apply it to. An `Option` rather than a hidden widget: a control that is
    /// built, hidden, and then consulted is a control that can be consulted
    /// wrongly, and the answer it would give is one nobody was offered.
    every: Option<gtk::CheckButton>,
    answer: Rc<Cell<Option<Choose>>>,
}

/// Build the prompt for one collision, wired but not shown. No parent means a
/// prompt that floats on its own, which is what the widget tests build.
fn compose(
    parent: Option<&gtk::ApplicationWindow>,
    conflict: &Conflict,
    answer: &Rc<Cell<Option<Choose>>>,
) -> Form {
    let form = layout(conflict, answer);
    wire(&form, parent);
    form
}

/// Construct the widgets and lay them out. Nothing is connected yet.
fn layout(conflict: &Conflict, answer: &Rc<Cell<Option<Choose>>>) -> Form {
    // The title says "file" or "folder" rather than "item", because what is being
    // thrown away is one or the other and a user deciding whether to replace a
    // directory needs to be told that is what it is.
    let folder = existing_is_folder(conflict);
    let window = gtk::Window::builder()
        .title(if folder {
            "Replace this folder?"
        } else {
            "Replace this file?"
        })
        .modal(true)
        .resizable(false)
        .build();

    let heading = gtk::Label::new(Some(&format!(
        "A {} named \u{201c}{}\u{201d} is already in {}.",
        if folder { "folder" } else { "file" },
        conflict.name,
        conflict.into.display()
    )));
    heading.set_widget_name("ufm-conflict-heading");
    heading.set_wrap(true);
    heading.set_xalign(0.0);
    heading.set_max_width_chars(52);

    let incoming = compare("Replace it with:", &describe(&conflict.incoming));
    let existing = conflict
        .existing
        .as_ref()
        .map(|side| compare("The one already there:", &describe(side)));

    let grid = gtk::Grid::new();
    grid.set_row_spacing(4);
    grid.set_column_spacing(12);
    grid.set_margin_top(18);
    grid.set_margin_bottom(12);
    grid.set_margin_start(18);
    grid.set_margin_end(18);
    grid.attach(&heading, 0, 0, 2, 1);
    grid.attach(&incoming.0, 0, 1, 1, 1);
    grid.attach(&incoming.1, 1, 1, 1, 1);
    if let Some(existing) = &existing {
        grid.attach(&existing.0, 0, 2, 1, 1);
        grid.attach(&existing.1, 1, 2, 1, 1);
    }

    // Only built when the plan holds another collision; a control that cannot
    // change the answer is noise, and one that is hidden but still consulted is a
    // bug waiting for a widget name to be typed.
    let every = (conflict.more > 0).then(|| {
        let every = gtk::CheckButton::with_label(&format!(
            "Apply to the {} remaining collisions",
            conflict.more
        ));
        every.set_widget_name("ufm-conflict-all");
        every.set_margin_start(18);
        every.set_margin_end(18);
        every.set_margin_bottom(6);
        every
    });

    let cancel = button("Cancel", "ufm-conflict-cancel");
    let skip = button("Skip", "ufm-conflict-skip");
    let rename = button("_Rename", "ufm-conflict-rename");
    let replace = button("_Replace", "ufm-conflict-replace");
    rename.add_css_class("suggested-action");

    let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    buttons.set_halign(gtk::Align::End);
    buttons.set_margin_top(12);
    buttons.set_margin_start(18);
    buttons.set_margin_end(18);
    buttons.set_margin_bottom(18);
    buttons.append(&cancel);
    buttons.append(&skip);
    buttons.append(&rename);
    buttons.append(&replace);

    let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
    content.append(&grid);
    if let Some(every) = &every {
        content.append(every);
    }
    content.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    content.append(&buttons);
    window.set_child(Some(&content));
    // Renaming rather than replacing is the default, and `Enter` follows it: the
    // answer that keeps both files is the one a mistimed keystroke survives.
    window.set_default_widget(Some(&rename));
    cancel.grab_focus();

    Form {
        window,
        cancel,
        skip,
        rename,
        replace,
        every,
        answer: Rc::clone(answer),
    }
}

/// Connect the four answers, the checkbox that answers for the rest, and `Escape`.
fn wire(form: &Form, parent: Option<&gtk::ApplicationWindow>) {
    if let Some(parent) = parent {
        form.window.set_transient_for(Some(parent));
        form.window.set_destroy_with_parent(true);
    }

    // Which answer a button gives depends on the checkbox, so the widening lives
    // in one function rather than at four call sites.
    for (button, choose) in [
        (&form.cancel, Choose::Cancel),
        (&form.skip, Choose::Skip),
        (&form.rename, Choose::Rename),
        (&form.replace, Choose::Replace),
    ] {
        let answer = Rc::clone(&form.answer);
        let window = form.window.clone();
        let every = form.every.clone();
        button.connect_clicked(move |_| {
            answer.set(Some(widen(choose, every.as_ref())));
            window.close();
        });
    }

    // A question nobody answered still has to be answered, because [`wait`] cannot
    // tell a closed window from an unanswered one and a paste cannot proceed on a
    // collision with no answer. `Escape` and a window manager both arrive here as a
    // close request, and a button reaches it only after its own handler has already
    // recorded an answer — the handlers set the answer before calling `close` — so
    // whatever this one sees unset is a question that really went unanswered.
    //
    // Not the `destroy` signal: `GTK`4 emits that when the widget is disposed, and a
    // prompt still referenced by the code that opened it is not disposed when it
    // closes, so a handler there would never run.
    let closing = Rc::clone(&form.answer);
    form.window.connect_close_request(move |_| {
        if closing.get().is_none() {
            closing.set(Some(Choose::Cancel));
        }
        Propagation::Proceed
    });
}

/// Turn a single-row answer into the batch answer the checkbox asks for.
///
/// With no checkbox there is nothing to widen to: the prompt was built for the
/// last collision in the batch, and answering rows that were never counted would
/// be answering questions nobody was asked.
#[must_use]
fn widen(choose: Choose, every: Option<&gtk::CheckButton>) -> Choose {
    let Some(every) = every else {
        return choose;
    };
    if !every.is_active() {
        return choose;
    }
    match choose {
        Choose::Replace => Choose::ReplaceAll,
        Choose::Rename => Choose::RenameAll,
        Choose::Skip => Choose::SkipAll,
        other => other,
    }
}

/// One labelled row of the comparison, kept apart so the two are built the same
/// way and can never disagree about layout.
#[must_use]
fn compare(label: &str, value: &str) -> (gtk::Label, gtk::Label) {
    let name = gtk::Label::new(Some(label));
    name.set_xalign(0.0);
    name.add_css_class("dim-label");
    let detail = gtk::Label::new(Some(value));
    detail.set_widget_name("ufm-conflict-detail");
    detail.set_wrap(true);
    detail.set_xalign(0.0);
    detail.set_max_width_chars(40);
    (name, detail)
}

#[must_use]
fn button(label: &str, name: &str) -> gtk::Button {
    let button = gtk::Button::with_mnemonic(label);
    button.set_widget_name(name);
    button
}

/// The three facts a person compares when deciding whether a file is worth
/// keeping: what it is, how full, and when it last changed.
#[must_use]
fn describe(side: &Side) -> String {
    let what = if side.folder {
        if side.size > 0 {
            format!("A folder of {}", format_items(side.size))
        } else {
            "A folder".to_string()
        }
    } else {
        human_size(side.size)
    };
    match side.modified {
        Some(when) => format!("{what}, modified {}", format_modified(when)),
        None => what,
    }
}

/// Whether the row already in the folder is a directory.
///
/// A row that could not be read is called a folder. The word "folder" is the
/// careful one: a user who reads it and meant "file" is being cautious, and one
/// who reads "file" and meant "folder" is about to lose a tree they did not
/// expect to.
#[must_use]
fn existing_is_folder(conflict: &Conflict) -> bool {
    conflict.existing.as_ref().is_none_or(|side| side.folder)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use ufm_core::Location;
    use ufm_core::entry::{Entry, EntryFlags, EntryKind};

    #[test]
    fn a_side_from_a_listing_counts_a_folder_by_what_it_holds() {
        let mut entry = row_entry("notes.txt", EntryKind::File, 2048);
        let side = Side::of(&entry);
        assert!(!side.folder);
        assert_eq!(side.size, 2048);
        entry.kind = EntryKind::Directory;
        entry.child_count = Some(7);
        assert_eq!(Side::of(&entry).size, 7);
    }

    #[test]
    fn a_folder_with_nothing_counted_says_so_without_a_number() {
        assert_eq!(
            describe(&Side {
                folder: true,
                size: 0,
                modified: None
            }),
            "A folder"
        );
        assert_eq!(
            describe(&Side {
                folder: true,
                size: 3,
                modified: None
            }),
            "A folder of 3 items"
        );
    }

    #[test]
    fn a_row_that_could_not_be_read_is_called_a_folder() {
        let known = Conflict {
            existing: Some(Side {
                folder: false,
                size: 1,
                modified: None,
            }),
            ..conflict("notes.txt", 0)
        };
        assert!(!existing_is_folder(&known), "a file is a file");
        let unknown = Conflict {
            existing: None,
            ..conflict("notes.txt", 0)
        };
        assert!(
            existing_is_folder(&unknown),
            "what cannot be read gets the careful word"
        );
    }

    #[test]
    fn the_checkbox_widens_only_an_answer_it_was_shown_for() {
        crate::ui::tk::ui(|| {
            let form = compose(None, &conflict("notes.txt", 2), &Rc::new(Cell::new(None)));
            let every = form
                .every
                .as_ref()
                .expect("two more collisions are waiting");
            assert!(!every.is_active());
            assert_eq!(widen(Choose::Replace, Some(every)), Choose::Replace);
            every.set_active(true);
            assert_eq!(widen(Choose::Replace, Some(every)), Choose::ReplaceAll);
            assert_eq!(widen(Choose::Rename, Some(every)), Choose::RenameAll);
            assert_eq!(widen(Choose::Skip, Some(every)), Choose::SkipAll);
            assert_eq!(
                widen(Choose::Cancel, Some(every)),
                Choose::Cancel,
                "cancel is never a batch answer"
            );
        });
    }

    #[test]
    fn the_last_collision_in_a_batch_is_not_offered_as_one() {
        crate::ui::tk::ui(|| {
            let form = compose(None, &conflict("notes.txt", 0), &Rc::new(Cell::new(None)));
            assert!(
                form.every.is_none(),
                "nothing is left to apply an answer to"
            );
            // The answer for the row in front of the user must survive the absence
            // of a checkbox rather than widen into a batch of one.
            assert_eq!(widen(Choose::Replace, None), Choose::Replace);
            assert_eq!(widen(Choose::Skip, None), Choose::Skip);
        });
    }

    #[test]
    fn every_button_records_its_own_answer() {
        crate::ui::tk::ui(|| {
            for (name, wanted) in [
                ("ufm-conflict-cancel", Choose::Cancel),
                ("ufm-conflict-skip", Choose::Skip),
                ("ufm-conflict-rename", Choose::Rename),
                ("ufm-conflict-replace", Choose::Replace),
            ] {
                let answer = Rc::new(Cell::new(None));
                let form = compose(None, &conflict("notes.txt", 1), &answer);
                assert!(answer.get().is_none(), "building asks nothing");
                find(&form.window, name)
                    .unwrap_or_else(|| panic!("{name} is in the prompt"))
                    .downcast::<gtk::Button>()
                    .expect("a button")
                    .emit_clicked();
                assert_eq!(answer.get(), Some(wanted), "{name} answered wrong");
            }
        });
    }

    #[test]
    fn a_prompt_that_was_never_answered_cancels_the_paste() {
        crate::ui::tk::ui(|| {
            // The path an `Escape` or a window manager takes: nothing records an
            // answer, so the close request has to. Emitted rather than reached
            // through `close()`, which on a window that was never realized skips
            // the request and destroys straight away.
            let answer = Rc::new(Cell::new(None));
            let form = compose(None, &conflict("notes.txt", 0), &answer);
            // `close-request` returns whether the window was claimed, and
            // `Propagation::Proceed` is the false of that: the prompt records an
            // answer and still lets itself be closed.
            let claimed: bool = form.window.emit_by_name("close-request", &[]);
            assert_eq!(
                answer.get(),
                Some(Choose::Cancel),
                "a question nobody answered must not be answered by replacing"
            );
            assert!(!claimed, "the prompt does not claim the request");
            // An answer already given survives the same request, which is what a
            // button produces: it records first and closes second.
            let answer = Rc::new(Cell::new(Some(Choose::ReplaceAll)));
            let form = compose(None, &conflict("notes.txt", 0), &answer);
            let _: bool = form.window.emit_by_name("close-request", &[]);
            assert_eq!(
                answer.get(),
                Some(Choose::ReplaceAll),
                "closing must not overwrite the answer a button gave"
            );
        });
    }

    #[test]
    fn the_prompt_names_both_rows_and_the_folder_they_share() {
        crate::ui::tk::ui(|| {
            let form = compose(None, &conflict("report.pdf", 1), &Rc::new(Cell::new(None)));
            let heading = find(&form.window, "ufm-conflict-heading")
                .expect("the heading is in the prompt")
                .downcast::<gtk::Label>()
                .expect("the heading is a label");
            let text = heading.label().to_string();
            assert!(text.contains("report.pdf"), "{text}");
            assert!(text.starts_with("A file named"), "{text}");
            let details = details_of(&form.window);
            assert_eq!(details.len(), 2, "both sides are described");
            assert!(details[0].contains("4.1 kB"), "incoming: {}", details[0]);
            assert!(details[1].contains("1.0 kB"), "existing: {}", details[1]);
        });
    }

    /// A collision over a file called `name`, with `more` collisions behind it.
    fn conflict(name: &str, more: usize) -> Conflict {
        Conflict {
            at: 0,
            name: name.to_string(),
            into: Location::file(Path::new("/tmp")),
            incoming: Side {
                folder: false,
                size: 4096,
                modified: None,
            },
            existing: Some(Side {
                folder: false,
                size: 1024,
                modified: None,
            }),
            more,
        }
    }

    fn row_entry(name: &str, kind: EntryKind, size: u64) -> Entry {
        Entry {
            location: Location::file(Path::new("/tmp").join(name)),
            name: name.to_string(),
            kind,
            flags: EntryFlags::empty(),
            size,
            allocated: None,
            modified: None,
            accessed: None,
            created: None,
            mode: None,
            uid: None,
            gid: None,
            mime: None,
            child_count: None,
            icon_key: String::new(),
        }
    }

    /// The text of every detail label, top to bottom.
    fn details_of<W: IsA<gtk::Widget>>(root: &W) -> Vec<String> {
        let mut out = Vec::new();
        collect(root.as_ref(), &mut out);
        out
    }

    fn collect(widget: &gtk::Widget, out: &mut Vec<String>) {
        if widget.widget_name() == "ufm-conflict-detail"
            && let Ok(label) = widget.clone().downcast::<gtk::Label>()
        {
            out.push(label.label().to_string());
        }
        for child in widget.observe_children().snapshot() {
            if let Ok(widget) = child.downcast::<gtk::Widget>() {
                collect(&widget, out);
            }
        }
    }

    /// Pull one named widget out of a prompt, the way the other widget tests do.
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
