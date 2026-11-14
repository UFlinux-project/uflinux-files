//! File operations: new folder, rename, trash, delete, copy and paste.
//!
//! The work is `GIO`'s. [`gio::File`] does the reading, copying and removing,
//! which means this module does not reimplement `rename(2)` semantics, the
//! trash-directory specification, or the copy-then-delete fallback that a move
//! across filesystems needs. What it adds is the three decisions `GIO` leaves to
//! the caller: what to call something when the name is taken, which rows a
//! batch may not touch, and what to say when only part of a batch succeeded.
//!
//! Every operation runs on a worker thread and reports through the window's
//! listing queue (see [`super::job`]), for the same reason a directory read
//! does: a copy that crosses onto a USB stick or off a network mount blocks for
//! as long as the write takes, and a window that froze for it looks broken.
//!
//! One thing `GIO` does *not* do is copy a directory: `g_file_copy` refuses one
//! outright. [`copy_tree`] is the recursion that covers it, and it is also why
//! [`remove_all`] exists — `g_file_delete` will not remove a folder that still
//! has anything in it either.

use std::cell::Cell;
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, SystemTime};

use gio::prelude::*;
use gtk::prelude::*;

use ufm_core::Location;
use ufm_core::entry::Entry;

use super::job::{Event, Handle};

/// No cancellation flag. An operation that has started is not going to be
/// interrupted — there is no cancel button for one yet — and naming this keeps
/// the call sites from repeating `None::<&gio::Cancellable>`.
const NO_CANCEL: Option<&gio::Cancellable> = None;

/// Copy without following symlinks, so a link arrives as a link rather than as
/// a second copy of whatever it pointed at.
const COPY_FLAGS: gio::FileCopyFlags = gio::FileCopyFlags::NOFOLLOW_SYMLINKS;

/// Metadata flags for [`gio::FileExt::copy_attributes`]. `ALL_METADATA` asks
/// for everything the filesystem can carry, including the timestamps that a
/// plain copy would leave at now.
const META_FLAGS: gio::FileCopyFlags = gio::FileCopyFlags::ALL_METADATA;

/// The attributes an enumeration has to return for the recursion to work: the
/// name to recurse on, and the type to decide whether to.
const ENUMERATE: &str = "standard::name,standard::type";

/// The attributes a collision needs: what the row is, how big, and when it last
/// changed. Nothing else is worth a `stat` before a prompt opens.
const SIDE_ATTRIBUTES: &str = "standard::type,standard::size,time::modified";

/// The one attribute name [`side_of`] reads a timestamp from, so the list above
/// and the read below cannot drift apart.
const TIME_MODIFIED: &str = "time::modified";

/// The name the new-folder dialog offers before any counter is added.
const UNTITLED: &str = "Untitled folder";

/// What a paste should do with what the clipboard holds.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Transfer {
    /// Leave the originals where they are. The default, because a clipboard that
    /// has been written to by an older version of the settings file, or one the
    /// user has not chosen a transfer for yet, should not destroy anything.
    #[default]
    Copy,
    /// Move them, which is what a cut means.
    Move,
}

impl Transfer {
    /// The past tense of this operation, for the message it reports with.
    #[must_use]
    pub fn past(self) -> &'static str {
        match self {
            Self::Copy => "Copied",
            Self::Move => "Moved",
        }
    }
}

/// What the user picked up, and what a paste should do with it.
///
/// Held by the window rather than by a global clipboard: a cut in one window
/// pasting into another would need a selection to survive being copied out of a
/// listing that window no longer has, and the two windows can disagree about
/// whether a row is even still there.
#[derive(Debug, Clone, Default)]
pub struct Clipboard {
    sources: Vec<Entry>,
    transfer: Transfer,
}

impl Clipboard {
    /// Remember `entries` for a later paste. `transfer` says whether the user
    /// copied or cut them, which is the only difference a paste can see.
    pub fn set(&mut self, entries: &[Entry], transfer: Transfer) {
        self.sources = entries.to_vec();
        self.transfer = transfer;
    }

    /// Forget the selection. A cut that has been pasted stays put until the user
    /// picks something up again, which is what every other file manager does and
    /// what lets one cut feed two folders.
    pub fn clear(&mut self) {
        self.sources.clear();
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.sources.is_empty()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.sources.len()
    }

    #[must_use]
    pub fn transfer(&self) -> Transfer {
        self.transfer
    }

    /// What a paste into `folder` would actually do.
    #[must_use]
    pub fn plan(&self, folder: &Location) -> Plan {
        plan(&self.sources, folder, self.transfer)
    }
}

/// One planned step: put `from` at `to`, name included.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    /// Where it reads from.
    pub from: Location,
    /// Where it writes to. Unique unless the user answered a [`Conflict`] by
    /// replacing, in which case it is the taken name and `replace` says so.
    pub to: Location,
    /// Whether a row already at `to` is to be thrown away to make room. Set only
    /// by an answer to a collision, which is the only way this program comes to
    /// destroy a file it did not put there.
    pub replace: bool,
}

impl Step {
    /// A step that writes where nothing is in the way.
    #[must_use]
    pub fn new(from: Location, to: Location) -> Self {
        Self {
            from,
            to,
            replace: false,
        }
    }
}

/// A row whose name the target folder already has, and everything the prompt
/// needs to describe the two rows that want the same name.
///
/// Recorded by [`plan`] rather than resolved there. Whether a collision should
/// become `report (2).pdf` or throw the old file away is a question about what
/// the user meant, and a file manager that guesses wrong in one direction fills
/// the disk with copies while guessing wrong in the other loses a file.
#[derive(Debug, Clone)]
pub struct Conflict {
    /// Which entry of [`Plan::steps`] this is about. The index rather than a copy
    /// of the step is what lets an answer put the step back in the order the user
    /// picked the rows in.
    pub at: usize,
    /// The name both rows want.
    pub name: String,
    /// The folder being pasted into.
    pub into: Location,
    /// The row that would arrive, from the listing the paste was asked for.
    pub incoming: Side,
    /// The row already on disk, when a `stat` for it came back. Read from the
    /// filesystem rather than from a listing: a listing can be seconds old, and a
    /// prompt that describes a file which is already gone is worse than one that
    /// says nothing about it.
    pub existing: Option<Side>,
    /// How many further collisions this paste will run into after this one. The
    /// prompt turns that into a "apply to the rest" answer, and a prompt that
    /// offers to answer questions there are no longer is would be lying about
    /// the batch.
    pub more: usize,
}

/// One side of a collision, in the three terms a person actually compares: what
/// kind of row it is, how full, and when it last changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Side {
    /// Whether it is a folder rather than a file.
    pub folder: bool,
    /// Bytes for a file, entries for a folder.
    pub size: u64,
    /// When it last changed, when the filesystem says.
    pub modified: Option<SystemTime>,
}

impl Side {
    /// Describe the row a listing reports.
    #[must_use]
    pub fn of(entry: &Entry) -> Self {
        Self {
            folder: entry.kind.is_dir(),
            size: if entry.kind.is_dir() {
                u64::from(entry.child_count.unwrap_or(0))
            } else {
                entry.size
            },
            modified: entry.modified,
        }
    }
}

/// An answer to one collision. `*All` variants answer the rest of the batch
/// without asking again; see [`resolve`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Choose {
    /// Throw the row already there away and put this one in its place.
    Replace,
    /// Put this one beside it, under the free name [`plan`] already worked out.
    Rename,
    /// Leave the folder alone as far as this row goes.
    Skip,
    /// Replace, and answer the rows behind this one the same way.
    ReplaceAll,
    /// Rename, and answer the rows behind this one the same way.
    RenameAll,
    /// Skip, and answer the rows behind this one the same way.
    SkipAll,
    /// Abandon the whole paste.
    Cancel,
}

impl Step {
    /// The name to say in a message about this step.
    fn label(&self) -> String {
        self.from.file_name().unwrap_or("item").to_string()
    }
}

/// A rename worked out in advance: where the row goes, and what to say when it
/// cannot go there.
#[derive(Debug)]
pub enum RenamePlan {
    /// Rename this row to that location.
    Go {
        /// The row to rename.
        from: Location,
        /// The new name, already checked against what is on disk.
        to: Location,
        /// The folder the row lives in, for the refresh.
        folder: Location,
        /// The row's name before the change, for the message.
        label: String,
    },
    /// The name is the one the row already has. Nothing to do, nothing to say.
    Unchanged,
    /// The reason this rename was refused, worded for the banner.
    Refused(String),
}

/// A paste worked out in advance: what will happen, what will not, and what the
/// user has not answered yet.
///
/// A plan straight out of [`plan`] may still carry [`Plan::conflicts`], which
/// means its steps are written under names nobody has agreed to. [`resolve`] is
/// what clears them; [`paste`] resolves an unanswered plan by renaming, so a
/// caller with no prompt gets the behaviour this program had before there was
/// one rather than an overwrite.
#[derive(Debug, Default)]
pub struct Plan {
    /// The steps to run, in the order the user picked them.
    pub steps: Vec<Step>,
    /// Rows that will not be touched, each already worded for the banner.
    pub skipped: Vec<String>,
    /// Collisions waiting for an answer, in the order their steps appear.
    pub conflicts: Vec<Conflict>,
}

impl Plan {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.steps.is_empty()
    }
}

/// What happened to one row that was thrown away.
enum Gone {
    /// It is in the trash directory.
    Trashed,
    /// It is gone for good, because the user asked for that or because the
    /// filesystem has no trash directory to put it in.
    Deleted,
}

/// How an operation turned out, for the banner and for the refresh.
#[derive(Debug)]
pub struct Outcome {
    /// Whether the operation did what was asked. A partial success is `false`,
    /// because the rows that failed are the interesting part of the answer.
    pub ok: bool,
    /// What to tell the user. Empty means there is nothing to say.
    pub message: String,
    /// The folders whose listings this invalidated, deduplicated. The window
    /// re-reads the one on screen; a parent column that is stale gets a reload.
    /// A failure that changed nothing reports nothing here, which is what stops
    /// a refused operation from costing a directory read.
    pub changed: Vec<Location>,
}

impl Outcome {
    fn done(message: impl Into<String>) -> Self {
        Self {
            ok: true,
            message: message.into(),
            changed: Vec::new(),
        }
    }

    fn failed(message: impl Into<String>) -> Self {
        Self {
            ok: false,
            message: message.into(),
            changed: Vec::new(),
        }
    }

    /// Say which folder this left behind. Takes `&mut self` because every call
    /// site has the location to hand before it starts working.
    fn note(&mut self, folder: &Location) {
        if !self.changed.contains(folder) {
            self.changed.push(folder.clone());
        }
    }

    /// Attach the reasons some rows were left out, turning a clean success into
    /// a report. A banner is the only place a skipped row can be mentioned, and
    /// a row that vanished without a word reads as a bug.
    #[must_use]
    fn with_skipped(mut self, skipped: &[String]) -> Self {
        if skipped.is_empty() {
            return self;
        }
        self.ok = false;
        self.message = if self.message.is_empty() {
            skipped.join("; ")
        } else {
            format!("{}; {}", self.message, skipped.join("; "))
        };
        self
    }
}

/// Work out what pasting `sources` into `folder` means.
///
/// Pure apart from the existence checks, which are one `stat` per candidate name
/// and are what makes the resulting [`Step`]s safe to run in any order.
///
/// A row whose name the folder already has is planned under a free name *and*
/// recorded as a collision. The step is the answer a file manager may give
/// without asking; the record is what lets it ask instead. Call [`resolve`] to
/// turn a plan with conflicts into one that is safe to run as answered.
#[must_use]
pub fn plan(sources: &[Entry], folder: &Location, transfer: Transfer) -> Plan {
    let mut plan = Plan::default();
    // Archive rows and `computer://` have no directory to write into; the reader
    // that produced them can list them but not change them.
    if !folder.is_real() {
        plan.skipped.push(format!(
            "{}: paste needs a folder on disk.",
            folder.display()
        ));
        return plan;
    }
    for source in sources {
        let from = &source.location;
        if !from.is_real() {
            plan.skipped
                .push(format!("{}: not a file on disk.", source.name));
            continue;
        }
        if transfer == Transfer::Move {
            // Moving a row into the folder it is already in is a no-op that
            // `g_file_move` answers with an error; saying so here is both cheaper
            // and clearer than passing that error through.
            if from.parent().as_ref() == Some(folder) {
                plan.skipped
                    .push(format!("{}: already in this folder.", source.name));
                continue;
            }
            // `mv a a/b` is the one move that cannot be answered by renaming, and
            // `GIO` reports it as a failure partway through.
            if source.kind.is_dir() && folder.is_within(from) {
                plan.skipped.push(format!(
                    "{}: cannot move a folder into itself.",
                    source.name
                ));
                continue;
            }
        }
        let Ok(base) = folder.join(&source.name) else {
            plan.skipped
                .push(format!("{}: cannot be used as a name.", source.name));
            continue;
        };
        // One `stat` per row, and the answer decides everything below: a free
        // name needs no decision from anyone, a taken one needs both a fallback
        // and a record of what is standing there.
        let taken = exists(&base);
        if taken {
            plan.conflicts.push(Conflict {
                at: plan.steps.len(),
                name: source.name.clone(),
                into: folder.clone(),
                incoming: Side::of(source),
                existing: side_of(&base),
                more: 0,
            });
        }
        plan.steps.push(Step {
            from: from.clone(),
            // The rename answer, which is also the answer a plan with no prompt
            // gets. Computed even for a conflict so that answering `Rename` costs
            // nothing beyond moving the step back into place.
            to: if taken {
                accept(&base, exists)
            } else {
                base.clone()
            },
            replace: false,
        });
    }
    // Filled in at the end rather than per row: a collision can only say how
    // many others follow it once the whole batch has been walked.
    let total = plan.conflicts.len();
    for (index, conflict) in plan.conflicts.iter_mut().enumerate() {
        conflict.more = total - index - 1;
    }
    plan
}

/// Apply the user's answers to the collisions in `plan`, in the order its steps
/// run.
///
/// The answers arrive through a callback rather than as a list for two reasons:
/// a prompt can be opened only for the collisions that are still worth asking
/// about — a standing `Replace all` answer means the rows behind it are never
/// shown — and the order the questions get asked is the order the work happens,
/// which is the one thing a paste may not get wrong.
///
/// The result has no conflicts left, so [`paste`] will run it as answered. A
/// [`Choose::Cancel`] abandons the whole paste rather than the one row: a user
/// who cancels a prompt about the third file did not ask for a partial copy.
#[must_use]
pub fn resolve(mut plan: Plan, mut answer: impl FnMut(&Conflict) -> Choose) -> Plan {
    if plan.conflicts.is_empty() {
        return plan;
    }
    let conflicts = std::mem::take(&mut plan.conflicts);
    let steps = std::mem::take(&mut plan.steps);
    // How many rows the paste was going to write, for the message a cancel
    // reports. `steps` is moved out of the plan below, so the number is taken
    // before that happens.
    let total = steps.len();
    let mut kept: Vec<Step> = Vec::with_capacity(total);
    // The reasons carried over from planning, with the rows the user declined
    // added to them: a skipped row is reported exactly like a row the folder
    // refused, because from the banner's point of view it is the same thing.
    let mut skipped = std::mem::take(&mut plan.skipped);
    // Where in `conflicts` the next step's collision would be. The list is built
    // in step order, so one cursor is enough.
    let mut next = 0_usize;
    // A `*All` answer leaves a standing answer for the collisions behind it, so
    // that answering once really does answer for the batch.
    let mut standing: Option<Choose> = None;
    for (index, mut step) in steps.into_iter().enumerate() {
        if conflicts
            .get(next)
            .is_none_or(|conflict| conflict.at != index)
        {
            kept.push(step);
            continue;
        }
        let conflict = conflicts[next].clone();
        next += 1;
        let choose = standing.unwrap_or_else(|| answer(&conflict));
        standing = match choose {
            Choose::ReplaceAll => Some(Choose::Replace),
            Choose::RenameAll => Some(Choose::Rename),
            Choose::SkipAll => Some(Choose::Skip),
            _ => None,
        };
        match choose {
            Choose::Replace | Choose::ReplaceAll => {
                // The taken name, not the free one the plan was built with: this
                // answer says the row already there is the one that goes.
                if let Ok(joined) = conflict.into.join(&conflict.name) {
                    step.to = joined;
                }
                step.replace = true;
                kept.push(step);
            }
            Choose::Rename | Choose::RenameAll => kept.push(step),
            Choose::Skip | Choose::SkipAll => {
                skipped.push(format!("{}: not pasted.", conflict.name));
            }
            Choose::Cancel => {
                skipped.push(format!(
                    "Paste of {} into {} cancelled.",
                    count(total),
                    conflict.into.display()
                ));
                plan.steps.clear();
                plan.skipped = skipped;
                return plan;
            }
        }
    }
    plan.steps = kept;
    plan.skipped = skipped;
    plan
}

/// Describe the row already sitting at `location`, for the prompt that has to
/// compare two of them.
///
/// Read from the filesystem rather than from the listing on screen: a listing is
/// as old as the last time the folder was read, and a prompt that offers to
/// replace a file which is no longer there describes a file the user cannot see.
fn side_of(location: &Location) -> Option<Side> {
    let info = file(location)?
        .query_info(SIDE_ATTRIBUTES, gio::FileQueryInfoFlags::NONE, NO_CANCEL)
        .ok()?;
    let folder = info.file_type() == gio::FileType::Directory;
    Some(Side {
        folder,
        size: if folder {
            0
        } else {
            u64::try_from(info.size()).unwrap_or(0)
        },
        // Read as a string rather than as a number. `time::modified` is a
        // `UINT64` attribute, so `attribute_int64` trips a type assertion and
        // answers zero, and `attribute_status` is not a usable presence test —
        // it reports `Unset` for attributes a plain `stat` clearly filled in.
        // A value that is there prints as a number; one that is not prints as
        // nothing at all, which is the distinction the prompt cares about.
        modified: info
            .attribute_as_string(TIME_MODIFIED)
            .and_then(|reported| reported.parse::<i64>().ok())
            .map(epoch_seconds),
    })
}

/// A whole-second timestamp from the `time::modified` attribute, which `GIO`
/// reports as seconds around the epoch and a filesystem may report as a number
/// too large or too small to be a date at all.
///
/// Whole seconds is all the prompt compares: two rows modified in the same minute
/// are indistinguishable to a person deciding which one to keep. A clock so far
/// off the epoch that `SystemTime` cannot name it is reported as the epoch, which
/// reads as a file from 1970 — an honest answer to a filesystem lying about time.
#[must_use]
fn epoch_seconds(seconds: i64) -> SystemTime {
    let span = Duration::from_secs(seconds.unsigned_abs());
    let shifted = if seconds < 0 {
        SystemTime::UNIX_EPOCH.checked_sub(span)
    } else {
        SystemTime::UNIX_EPOCH.checked_add(span)
    };
    shifted.unwrap_or(SystemTime::UNIX_EPOCH)
}

/// `base`, or a free name beside it when `base` is taken.
fn accept(base: &Location, taken: impl Fn(&Location) -> bool) -> Location {
    if !taken(base) {
        return base.clone();
    }
    let Some(parent) = base.parent() else {
        return base.clone();
    };
    let name = base.file_name().unwrap_or("item");
    let named = unique_name(name, &mut |candidate: &str| {
        parent
            .join(candidate)
            .is_ok_and(|location| taken(&location))
    });
    parent.join(&named).unwrap_or_else(|_| base.clone())
}

/// A name `taken` says is free: `report.pdf`, then `report (2).pdf`,
/// `report (3).pdf`, and so on.
///
/// The counter goes before the extension because that is what keeps the
/// suggestion openable: `report.pdf (2)` is a file no image viewer will
/// recognise, and a name the user did not choose should still look like the file
/// it is a copy of.
#[must_use]
pub fn unique_name(name: &str, mut taken: impl FnMut(&str) -> bool) -> String {
    if !taken(name) {
        return name.to_string();
    }
    let (stem, extension) = split_name(name);
    for counter in 2..1000 {
        let candidate = format!("{stem} ({counter}){extension}");
        if !taken(&candidate) {
            return candidate;
        }
    }
    format!("{stem} (1000){extension}")
}

/// The name the new-folder dialog offers: `Untitled folder`, then `Untitled
/// folder 2`, `Untitled folder 3`.
///
/// No extension and no parentheses here: a folder is named after what it holds,
/// and a user who accepts the suggestion twice in a row expects the second one to
/// be the next in a series rather than a copy of a document.
#[must_use]
pub fn folder_name(mut taken: impl FnMut(&str) -> bool) -> String {
    if !taken(UNTITLED) {
        return UNTITLED.to_string();
    }
    for counter in 2..1000 {
        let candidate = format!("{UNTITLED} {counter}");
        if !taken(&candidate) {
            return candidate;
        }
    }
    format!("{UNTITLED} 1000")
}

/// Split `report.pdf` into `("report", ".pdf")`. A leading dot belongs to the
/// stem, so `.bashrc` grows its counter at the end rather than in the middle.
fn split_name(name: &str) -> (&str, &str) {
    match name.rfind('.') {
        Some(at) if at > 0 => (&name[..at], &name[at..]),
        _ => (name, ""),
    }
}

/// Whether a location already names something on disk.
fn exists(location: &Location) -> bool {
    location
        .as_path()
        .is_some_and(|path| gio::File::for_path(path).query_exists(NO_CANCEL))
}

/// The `GIO` handle for a location, when it is a path the kernel can see.
fn file(location: &Location) -> Option<gio::File> {
    location.as_path().map(gio::File::for_path)
}

/// Run one planned step.
///
/// A step answering a collision by replacing runs with `OVERWRITE`, which is what
/// lets `GIO` do the destroying: replacing a file is a rename or a copy onto an
/// existing name, and both are atomic where the filesystem supports them. A step
/// that did not come from a collision never passes the flag, so nothing this
/// program plans on its own can overwrite anything.
fn run(step: &Step, transfer: Transfer) -> Result<(), String> {
    let from = file(&step.from).ok_or("not a file on disk")?;
    let to = file(&step.to).ok_or("not a file on disk")?;
    let flags = if step.replace {
        COPY_FLAGS | gio::FileCopyFlags::OVERWRITE
    } else {
        COPY_FLAGS
    };
    let result = if transfer == Transfer::Move {
        // `g_file_move` renames where it can and falls back to copy-then-delete
        // where it cannot, across filesystems and for directories included.
        from.move_(&to, flags, NO_CANCEL, None)
    } else if is_directory(&from) {
        copy_tree(&from, &to, step.replace)
    } else {
        copy_file(&from, &to, flags)
    };
    result.map_err(|error| error.to_string())
}

fn is_directory(file: &gio::File) -> bool {
    file.query_file_type(gio::FileQueryInfoFlags::NOFOLLOW_SYMLINKS, NO_CANCEL)
        == gio::FileType::Directory
}

/// Copy one file and then its metadata.
///
/// Two calls, and the second is allowed to fail: a copy that lost its timestamps
/// is still the copy that was asked for, and reporting it as a failure would
/// invite the user to try again and end up with two files.
fn copy_file(
    from: &gio::File,
    to: &gio::File,
    flags: gio::FileCopyFlags,
) -> Result<(), glib::Error> {
    from.copy(to, flags, NO_CANCEL, None)?;
    from.copy_attributes(to, META_FLAGS, NO_CANCEL).ok();
    Ok(())
}

/// Copy a directory, its contents, and the metadata of everything in between.
///
/// `g_file_copy` refuses a directory, so the recursion is ours. The depth is
/// bounded by the worker's stack: a frame here is a handful of locals, so a 256
/// KiB stack lasts a few thousand levels, and no filesystem the reader can list
/// comes close.
///
/// `merge` says that a target which already exists is not a failure but the point:
/// replacing a folder means writing into the folder that is already there, so an
/// existing directory is walked into rather than complained about, and the files
/// inside it are overwritten one at a time. Without that, replacing a folder would
/// fail on its own name and leave the old contents mixed with the new.
fn copy_tree(from: &gio::File, to: &gio::File, merge: bool) -> Result<(), glib::Error> {
    match to.make_directory(NO_CANCEL) {
        Ok(()) => {}
        // Replacing a folder means merging into what is there, so the only error
        // worth passing on is one that is not "it already exists".
        Err(error) if merge && error.matches(gio::IOErrorEnum::Exists) => {}
        Err(error) => return Err(error),
    }
    from.copy_attributes(to, META_FLAGS, NO_CANCEL).ok();
    let flags = if merge {
        COPY_FLAGS | gio::FileCopyFlags::OVERWRITE
    } else {
        COPY_FLAGS
    };
    for name in child_names(from)? {
        let child = from.child(&name);
        let target = to.child(&name);
        if is_directory(&child) {
            copy_tree(&child, &target, merge)?;
        } else {
            copy_file(&child, &target, flags)?;
        }
    }
    Ok(())
}

/// The names of one directory's children.
///
/// Collected up front rather than while recursing: the enumerator holds the
/// directory open, and removing a folder whose read handle is still on it is the
/// kind of thing that works on one filesystem and not on another.
fn child_names(dir: &gio::File) -> Result<Vec<PathBuf>, glib::Error> {
    let mut out = Vec::new();
    let enumerator = dir.enumerate_children(
        ENUMERATE,
        gio::FileQueryInfoFlags::NOFOLLOW_SYMLINKS,
        NO_CANCEL,
    )?;
    while let Some(info) = enumerator.next_file(NO_CANCEL)? {
        out.push(info.name());
    }
    Ok(out)
}

/// Remove a row and, when it is a folder, everything inside it.
fn remove_all(file: &gio::File) -> Result<(), glib::Error> {
    if is_directory(file) {
        for name in child_names(file)? {
            remove_all(&file.child(&name))?;
        }
    }
    file.delete(NO_CANCEL)
}

/// Throw one row away, preferring the trash.
///
/// A filesystem with no trash directory answers `g_file_trash` with
/// `NotSupported`. That is not a failure to report — the row is gone either way —
/// so it is deleted instead and the difference shows up in the count the banner
/// reports.
fn discard_one(file: &gio::File) -> Result<Gone, String> {
    match file.trash(NO_CANCEL) {
        Ok(()) => Ok(Gone::Trashed),
        Err(trash_error) => remove_all(file)
            .map(|()| Gone::Deleted)
            .map_err(|error| format!("{trash_error}: {error}")),
    }
}

/// Hand work to a worker and report its outcome through the queue.
///
/// The fallback for a thread that could not start is the point of taking the
/// queue here rather than in the closure: nothing was done, and a window that
/// heard nothing back would go on showing rows that are still there.
pub(crate) fn spawn(handle: &Handle, work: impl FnOnce() -> Outcome + Send + 'static) {
    let worker = handle.clone();
    let spawned = thread::Builder::new()
        .stack_size(256 * 1024)
        .spawn(move || worker.send(Event::Operation { outcome: work() }));
    if spawned.is_err() {
        handle.send(Event::Operation {
            outcome: Outcome::failed("cannot start a worker for this operation."),
        });
    }
}

/// Move `entries` to the trash, or remove them for good when `permanent`.
pub fn discard(handle: &Handle, entries: &[Entry], permanent: bool) {
    let picked = entries.to_vec();
    spawn(handle, move || {
        let mut trashed = 0_usize;
        let mut deleted = 0_usize;
        let mut failures: Vec<String> = Vec::new();
        let mut changed: Vec<Location> = Vec::new();
        for entry in &picked {
            let Some(target) = file(&entry.location) else {
                failures.push(format!("{}: not a file on disk.", entry.name));
                continue;
            };
            let removed = if permanent {
                remove_all(&target)
                    .map(|()| Gone::Deleted)
                    .map_err(|error| error.to_string())
            } else {
                discard_one(&target)
            };
            match removed {
                Ok(Gone::Trashed) => trashed += 1,
                Ok(Gone::Deleted) => deleted += 1,
                Err(message) => {
                    failures.push(format!("{}: {message}", entry.name));
                    continue;
                }
            }
            // Only a row that actually left counts as a change. A refusal costs
            // nothing, and re-reading a folder to discover it is unchanged is a
            // directory walk spent on confirming that nothing happened.
            if let Some(parent) = entry.location.parent()
                && !changed.contains(&parent)
            {
                changed.push(parent);
            }
        }
        let mut parts = Vec::new();
        if trashed > 0 {
            parts.push(format!("{} moved to trash", count(trashed)));
        }
        if deleted > 0 {
            parts.push(format!("{} deleted", count(deleted)));
        }
        let mut outcome = if parts.is_empty() {
            Outcome::failed(String::new())
        } else {
            Outcome::done(String::new())
        };
        outcome.ok = failures.is_empty();
        outcome.message = report(&parts, &failures);
        outcome.changed = changed;
        outcome
    });
}

/// Run a planned paste. `transfer` comes from the clipboard the plan came from.
pub fn paste(handle: &Handle, plan: Plan, transfer: Transfer) {
    // Resolving here rather than running an unanswered plan is what keeps the
    // two callers of this function honest: a window that asked has nothing left
    // to resolve and passes straight through, and a caller with no way to ask
    // gets the rename fallback rather than an overwrite it never promised.
    let Plan {
        steps,
        skipped,
        conflicts: _,
    } = resolve(plan, |_| Choose::Rename);
    if steps.is_empty() {
        let message = if skipped.is_empty() {
            "Nothing to paste.".to_string()
        } else {
            skipped.join("; ")
        };
        handle.send(Event::Operation {
            outcome: Outcome::failed(message),
        });
        return;
    }
    spawn(handle, move || {
        let mut outcome = Outcome::done(String::new());
        let mut done = 0_usize;
        let mut failures: Vec<String> = Vec::new();
        for step in &steps {
            if let Err(message) = run(step, transfer) {
                failures.push(format!("{}: {message}", step.label()));
                continue;
            }
            done += 1;
            if let Some(parent) = step.to.parent() {
                outcome.note(&parent);
            }
            // A move empties the folder the row came from as well as filling the
            // one it went to, and a column showing the source has to know.
            if transfer == Transfer::Move
                && let Some(parent) = step.from.parent()
            {
                outcome.note(&parent);
            }
        }
        let parts = if done == 0 {
            Vec::new()
        } else {
            vec![format!("{} {}", transfer.past(), count(done))]
        };
        outcome.ok = failures.is_empty();
        outcome.message = report(&parts, &failures);
        outcome.with_skipped(&skipped)
    });
}

/// Work out what renaming `entry` to `name` means, without touching the disk
/// beyond the one existence check that a refusal needs. Pure enough to test, and
/// it is what keeps the worker from discovering a bad name after the dialog has
/// already been dismissed.
#[must_use]
pub fn plan_rename(entry: &Entry, name: &str) -> RenamePlan {
    let refused = |reason: String| RenamePlan::Refused(format!("{}: {reason}", entry.name));
    let Some(folder) = entry.location.parent() else {
        return refused("nowhere to rename it.".to_string());
    };
    let Ok(to) = folder.join(name) else {
        return RenamePlan::Refused(format!("{name}: cannot be used as a name."));
    };
    if to == entry.location {
        return RenamePlan::Unchanged;
    }
    if exists(&to) {
        return refused(format!("a file named {name} already exists."));
    }
    RenamePlan::Go {
        from: entry.location.clone(),
        to,
        folder,
        label: entry.name.clone(),
    }
}

/// Rename one row to `name`, in place.
pub fn rename(handle: &Handle, entry: &Entry, name: &str) {
    let (from, to, folder, label) = match plan_rename(entry, name) {
        RenamePlan::Go {
            from,
            to,
            folder,
            label,
        } => (from, to, folder, label),
        RenamePlan::Unchanged => {
            handle.send(Event::Operation {
                outcome: Outcome::done(String::new()),
            });
            return;
        }
        RenamePlan::Refused(message) => {
            handle.send(Event::Operation {
                outcome: Outcome::failed(message),
            });
            return;
        }
    };
    spawn(handle, move || match (file(&from), file(&to)) {
        (Some(source), Some(target)) => {
            // Within one directory `g_file_move` is a `rename(2)`, which is all a
            // rename inside a real folder can be.
            let result = source.move_(&target, gio::FileCopyFlags::NONE, NO_CANCEL, None);
            match result {
                Ok(()) => {
                    let mut renamed = Outcome::done(String::new());
                    renamed.note(&folder);
                    renamed
                }
                Err(error) => Outcome::failed(format!("{label}: {error}")),
            }
        }
        _ => Outcome::failed(format!("{label}: not a file on disk.")),
    });
}

/// Create a folder called `name` inside `folder`, growing a counter when that
/// name is already used.
pub fn create_folder(handle: &Handle, folder: &Location, name: &str) {
    let Ok(base) = folder.join(name) else {
        handle.send(Event::Operation {
            outcome: Outcome::failed(format!("{name}: cannot be used as a name.")),
        });
        return;
    };
    let to = accept(&base, exists);
    let label = to.file_name().unwrap_or(name).to_string();
    let place = folder.clone();
    spawn(handle, move || match file(&to) {
        None => Outcome::failed(format!("{label}: not a folder on disk.")),
        Some(target) => match target.make_directory(NO_CANCEL) {
            Ok(()) => {
                let mut created = Outcome::done(format!("Created {label}."));
                created.note(&place);
                created
            }
            Err(error) => Outcome::failed(format!("{label}: {error}")),
        },
    });
}

/// Ask for a new folder inside `folder`, then create it. `parent` is the window
/// the prompt belongs to; it is kept for the session the way [`super::connect`]
/// keeps its dialog, so closing the main window closes the prompt too.
pub fn ask_new_folder(parent: &gtk::ApplicationWindow, handle: &Handle, folder: &Location) {
    let name = folder_name(|candidate| {
        folder
            .join(candidate)
            .is_ok_and(|location| exists(&location))
    });
    let worker = handle.clone();
    let place = folder.clone();
    ask(
        parent,
        "New Folder",
        "Folder name:",
        &name,
        "_Create",
        None,
        move |typed| create_folder(&worker, &place, &typed),
    );
}

/// Ask for a new name for `entry`, then rename it.
pub fn ask_rename(parent: &gtk::ApplicationWindow, handle: &Handle, entry: &Entry) {
    let worker = handle.clone();
    let picked = entry.clone();
    let (stem, _) = split_name(&entry.name);
    // Select the stem, so typing replaces the part that identifies the file and
    // leaves the extension alone — the reason a rename is usually two keystrokes.
    let end = i32::try_from(stem.chars().count()).unwrap_or(i32::MAX);
    ask(
        parent,
        "Rename",
        "New name:",
        &entry.name,
        "_Rename",
        Some((0, end)),
        move |typed| rename(&worker, &picked, &typed),
    );
}

/// A modal prompt for one line of text, in the shape [`super::connect`] uses: a
/// plain [`gtk::Window`] rather than a dialog class, because `GTK4` has no entry
/// dialog to subclass.
///
/// Built before it is shown, and shown by attaching it to `parent` rather than by
/// the builder: a window that is transient for another is destroyed with it, which
/// is what stops a prompt left open from keeping the session alive after the last
/// folder window closes.
///
/// `select` is the region to highlight, in characters.
fn ask(
    parent: &gtk::ApplicationWindow,
    title: &str,
    hint: &str,
    initial: &str,
    ok_label: &str,
    select: Option<(i32, i32)>,
    on_ok: impl FnOnce(String) + 'static,
) {
    let window = compose(title, hint, initial, ok_label, select, on_ok);
    if let Some(app) = parent.application() {
        app.add_window(&window);
    }
    window.set_transient_for(Some(parent));
    window.set_destroy_with_parent(true);
    window.present();
}

/// Build a prompt without showing it, so the widget tests can look inside one
/// without an application to attach it to.
fn compose(
    title: &str,
    hint: &str,
    initial: &str,
    ok_label: &str,
    select: Option<(i32, i32)>,
    on_ok: impl FnOnce(String) + 'static,
) -> gtk::Window {
    let window = gtk::Window::builder()
        .title(title)
        .modal(true)
        .resizable(false)
        .build();

    let hint_label = gtk::Label::new(Some(hint));
    hint_label.set_widget_name("ufm-prompt-hint");
    hint_label.set_xalign(0.0);

    let entry = gtk::Entry::new();
    entry.set_widget_name("ufm-prompt-text");
    entry.set_text(initial);
    entry.set_activates_default(true);
    entry.set_width_chars(36);
    entry.set_max_width_chars(36);

    let status = gtk::Label::new(None);
    status.set_widget_name("ufm-prompt-status");
    status.set_wrap(true);
    status.set_lines(2);
    status.set_xalign(0.0);
    status.set_max_width_chars(40);
    status.set_visible(false);

    let cancel = gtk::Button::with_label("Cancel");
    cancel.set_widget_name("ufm-prompt-cancel");
    let ok = gtk::Button::with_mnemonic(ok_label);
    ok.set_widget_name("ufm-prompt-ok");
    ok.add_css_class("suggested-action");

    let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    buttons.set_halign(gtk::Align::End);
    buttons.set_margin_top(12);
    buttons.set_margin_start(18);
    buttons.set_margin_end(18);
    buttons.set_margin_bottom(18);
    buttons.append(&cancel);
    buttons.append(&ok);

    let fields = gtk::Box::new(gtk::Orientation::Vertical, 6);
    fields.set_margin_top(18);
    fields.set_margin_start(18);
    fields.set_margin_end(18);
    fields.append(&hint_label);
    fields.append(&entry);
    fields.append(&status);

    let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
    content.append(&fields);
    content.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    content.append(&buttons);
    window.set_child(Some(&content));
    window.set_default_widget(Some(&ok));

    cancel.connect_clicked({
        let window = window.clone();
        move |_| window.close()
    });

    // `Cell` rather than a plain capture: `connect_clicked` needs an `Fn`, and
    // the callback it carries is `FnOnce`. Taking it out is what makes the
    // second half of a double-click on the default button a no-op.
    let once = Cell::new(Some(Box::new(on_ok) as Box<dyn FnOnce(String)>));
    ok.connect_clicked({
        let window = window.clone();
        let entry = entry.clone();
        let status = status.clone();
        move |_| {
            let text = entry.text().trim().to_string();
            if text.is_empty() {
                status.set_label("A name is needed.");
                status.set_visible(true);
                return;
            }
            window.close();
            if let Some(callback) = once.take() {
                callback(text);
            }
        }
    });

    entry.grab_focus();
    // After focusing, because focusing an entry selects its whole text and a
    // rename prompt exists to make that one keystroke shorter.
    if let Some((start, end)) = select {
        entry.select_region(start, end);
    }

    window
}

/// `N items`, spelled out for one.
fn count(n: usize) -> String {
    if n == 1 {
        "1 item".to_string()
    } else {
        format!("{n} items")
    }
}

/// Put what happened in front of what went wrong, for the banner.
fn report(parts: &[String], failures: &[String]) -> String {
    let done = parts.join(", ");
    let failed = failures.join("; ");
    if failed.is_empty() {
        done
    } else if done.is_empty() {
        failed
    } else {
        format!("{done}; {failed}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::mpsc;

    use super::super::job::Queue;
    use ufm_core::entry::{EntryFlags, EntryKind};

    /// Names a test pretends are already used.
    struct Taken(Vec<String>);

    impl Taken {
        fn of(names: &[&str]) -> Self {
            Self(names.iter().map(|name| (*name).to_string()).collect())
        }

        fn check(&mut self, candidate: &str) -> bool {
            self.0.iter().any(|name| name == candidate)
        }
    }

    fn entry(root: &std::path::Path, name: &str, kind: EntryKind) -> Entry {
        Entry {
            location: Location::file(root.join(name)),
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

    fn file(root: &std::path::Path, name: &str) -> Entry {
        entry(root, name, EntryKind::File)
    }

    fn folder(root: &std::path::Path, name: &str) -> Entry {
        entry(root, name, EntryKind::Directory)
    }

    /// A directory holding `notes.txt` and `deep/inner.txt`.
    fn tree(root: &std::path::Path) {
        fs::create_dir_all(root.join("deep")).expect("a test tree");
        fs::write(root.join("notes.txt"), b"one").expect("a test file");
        fs::write(root.join("deep").join("inner.txt"), b"two").expect("a nested file");
    }

    #[test]
    fn a_free_name_is_used_as_it_stands() {
        let mut taken = Taken::of(&["other.txt"]);
        assert_eq!(
            unique_name("notes.txt", |name| taken.check(name)),
            "notes.txt"
        );
    }

    #[test]
    fn a_taken_name_grows_a_counter_before_the_extension() {
        let mut taken = Taken::of(&["report.pdf", "report (2).pdf"]);
        assert_eq!(
            unique_name("report.pdf", |name| taken.check(name)),
            "report (3).pdf"
        );
    }

    #[test]
    fn a_dotfile_grows_its_counter_at_the_end() {
        let mut taken = Taken::of(&[".bashrc"]);
        assert_eq!(
            unique_name(".bashrc", |name| taken.check(name)),
            ".bashrc (2)"
        );
    }

    #[test]
    fn a_name_without_an_extension_is_still_numbered() {
        let mut taken = Taken::of(&["README"]);
        assert_eq!(
            unique_name("README", |name| taken.check(name)),
            "README (2)"
        );
    }

    #[test]
    fn the_counter_stops_growing_at_a_thousand() {
        let mut taken = Taken::of(&["notes.txt"]);
        taken
            .0
            .extend((2..=1000).map(|n| format!("notes ({n}).txt")));
        assert_eq!(
            unique_name("notes.txt", |name| taken.check(name)),
            "notes (1000).txt"
        );
    }

    #[test]
    fn the_folder_suggestion_is_a_plain_series() {
        let mut empty = Taken::of(&[]);
        assert_eq!(folder_name(|name| empty.check(name)), UNTITLED);
        let mut taken = Taken::of(&[UNTITLED, "Untitled folder 2"]);
        assert_eq!(folder_name(|name| taken.check(name)), "Untitled folder 3");
    }

    #[test]
    fn only_the_last_dot_separates_an_extension() {
        assert_eq!(split_name("archive.tar.gz"), ("archive.tar", ".gz"));
        assert_eq!(split_name(".hidden"), (".hidden", ""));
        assert_eq!(split_name("plain"), ("plain", ""));
    }

    #[test]
    fn pasting_into_a_virtual_folder_is_refused_once_for_the_whole_batch() {
        let sources = [file(std::path::Path::new("/tmp"), "a.txt")];
        let plan = plan(&sources, &Location::computer(), Transfer::Copy);
        assert!(plan.steps.is_empty());
        assert_eq!(plan.skipped.len(), 1, "one reason, not one per row");
    }

    #[test]
    fn a_row_that_is_not_on_disk_is_skipped_with_a_reason() {
        let inside = Location::archive("/tmp/box.zip", &["readme.md"]).expect("an archive row");
        let mut row = file(std::path::Path::new("/tmp"), "readme.md");
        row.location = inside;
        let plan = plan(&[row], &Location::file("/tmp"), Transfer::Copy);
        assert!(plan.steps.is_empty());
        assert_eq!(plan.skipped.len(), 1);
    }

    #[test]
    fn a_move_into_the_folder_the_row_is_already_in_is_skipped() {
        let root = std::path::Path::new("/tmp");
        let plan = plan(
            &[file(root, "notes.txt")],
            &Location::file(root),
            Transfer::Move,
        );
        assert!(plan.steps.is_empty());
        assert!(
            plan.skipped[0].contains("already in this folder"),
            "{}",
            plan.skipped[0]
        );
    }

    #[test]
    fn a_copy_into_the_current_folder_is_planned_even_though_a_move_is_not() {
        let root = std::path::Path::new("/tmp");
        let plan = plan(
            &[file(root, "definitely-unique-ufm.txt")],
            &Location::file(root),
            Transfer::Copy,
        );
        assert_eq!(plan.steps.len(), 1);
        assert!(plan.skipped.is_empty());
    }

    #[test]
    fn a_folder_cannot_be_moved_into_itself() {
        let root = std::path::Path::new("/tmp");
        let plan = plan(
            &[folder(root, "projects")],
            &Location::file(root.join("projects").join("notes")),
            Transfer::Move,
        );
        assert!(plan.steps.is_empty());
        assert!(
            plan.skipped[0].contains("into itself"),
            "{}",
            plan.skipped[0]
        );
    }

    #[test]
    fn a_moved_row_lands_beside_the_folder_it_was_pasted_into() {
        let root = std::path::Path::new("/tmp");
        let plan = plan(
            &[file(root, "definitely-unique-ufm.txt")],
            &Location::file(root.join("elsewhere")),
            Transfer::Move,
        );
        assert_eq!(plan.steps.len(), 1);
        assert_eq!(
            plan.steps[0].to,
            Location::file(root.join("elsewhere").join("definitely-unique-ufm.txt"))
        );
    }

    #[test]
    fn a_plan_renames_only_when_the_new_name_is_free() {
        let root = std::path::Path::new("/tmp");
        let row = file(root, "definitely-unique-ufm.txt");
        let RenamePlan::Go { to, folder, .. } = plan_rename(&row, "renamed.txt") else {
            panic!("a free name in a real folder should be planned");
        };
        assert_eq!(to, Location::file(root.join("renamed.txt")));
        assert_eq!(folder, Location::file(root));
    }

    #[test]
    fn renaming_a_row_to_its_own_name_is_not_a_job() {
        let row = file(std::path::Path::new("/tmp"), "notes.txt");
        assert!(matches!(
            plan_rename(&row, "notes.txt"),
            RenamePlan::Unchanged
        ));
    }

    #[test]
    fn a_rename_onto_an_existing_file_is_refused() {
        let root = std::path::Path::new("/tmp");
        let row = file(root, "definitely-unique-ufm.txt");
        let RenamePlan::Refused(message) = plan_rename(&row, ".X11-unix") else {
            panic!("an existing name should be refused");
        };
        assert!(message.contains("already exists"), "{message}");
    }

    #[test]
    fn a_name_with_a_slash_in_it_is_refused_as_a_name() {
        let row = file(std::path::Path::new("/tmp"), "notes.txt");
        let RenamePlan::Refused(message) = plan_rename(&row, "two/names") else {
            panic!("a path is not a name");
        };
        assert!(message.contains("cannot be used as a name"), "{message}");
    }

    #[test]
    fn one_item_is_spelled_out_and_the_rest_are_counted() {
        assert_eq!(count(1), "1 item");
        assert_eq!(count(0), "0 items");
        assert_eq!(count(7), "7 items");
    }

    #[test]
    fn a_report_puts_the_failures_last() {
        let done = vec!["Copied 1 item".to_string()];
        assert_eq!(report(&done, &[]), "Copied 1 item");
        let failed = vec!["notes.txt: read-only".to_string()];
        assert_eq!(
            report(&done, &failed),
            "Copied 1 item; notes.txt: read-only"
        );
        assert_eq!(report(&[], &failed), "notes.txt: read-only");
    }

    #[test]
    fn a_clipboard_carries_the_transfer_it_was_filled_with() {
        let root = std::path::Path::new("/tmp");
        let mut clipboard = Clipboard::default();
        assert!(clipboard.is_empty());
        assert_eq!(clipboard.transfer(), Transfer::Copy);
        clipboard.set(&[file(root, "notes.txt")], Transfer::Move);
        assert_eq!(clipboard.len(), 1);
        assert_eq!(clipboard.transfer(), Transfer::Move);
        // The plan the clipboard makes is the plan for its own transfer.
        let plan = clipboard.plan(&Location::file(root));
        assert!(
            plan.steps.is_empty(),
            "a cut inside its own folder is a no-op"
        );
        clipboard.clear();
        assert!(clipboard.is_empty());
    }

    #[test]
    fn copying_a_folder_takes_the_nested_files_with_it() {
        let from = tempfile::tempdir().expect("a source folder");
        let to = tempfile::tempdir().expect("a destination folder");
        tree(from.path());
        let target = to.path().join("copy");
        copy_tree(
            &gio::File::for_path(from.path()),
            &gio::File::for_path(&target),
            false,
        )
        .expect("a folder should copy");
        assert_eq!(
            fs::read(target.join("deep").join("inner.txt")).expect("a copied file"),
            b"two"
        );
        assert!(target.join("notes.txt").exists());
    }

    #[test]
    fn removing_a_folder_takes_what_is_inside_it() {
        let root = tempfile::tempdir().expect("a folder to lose");
        tree(root.path());
        remove_all(&gio::File::for_path(root.path())).expect("a tree should be removable");
        assert!(!root.path().exists(), "the folder itself should go too");
    }

    #[test]
    fn a_row_is_thrown_whichever_way_the_filesystem_allows() {
        let root = tempfile::tempdir().expect("a folder to throw from");
        tree(root.path());
        let dir = gio::File::for_path(root.path().join("deep"));
        // Whether this filesystem has a trash directory depends on where the
        // temporary folder is; either answer is a success, and the row must be
        // gone for both.
        discard_one(&dir).expect("a folder should be discardable");
        assert!(!root.path().join("deep").exists());
        let plain = gio::File::for_path(root.path().join("notes.txt"));
        discard_one(&plain).expect("a file should be discardable");
        assert!(!root.path().join("notes.txt").exists());
    }

    #[test]
    fn a_free_name_is_chosen_and_a_taken_one_grows_a_counter() {
        let root = tempfile::tempdir().expect("a folder to name into");
        let base = Location::file(root.path().join("box"));
        assert_eq!(accept(&base, exists), base);
        fs::create_dir(root.path().join("box")).expect("a first folder");
        assert_eq!(
            accept(&base, exists),
            Location::file(root.path().join("box (2)"))
        );
    }

    #[test]
    fn a_pasted_step_reports_the_folder_it_changed() {
        let root = tempfile::tempdir().expect("a folder to work in");
        tree(root.path());
        let queue = Queue::without_source().expect("a pipe can be made");
        let plan = Plan {
            steps: vec![Step::new(
                Location::file(root.path().join("notes.txt")),
                Location::file(root.path().join("copied.txt")),
            )],
            ..Plan::default()
        };
        paste(&queue.handle(), plan, Transfer::Move);
        let outcome = operated(&queue);
        assert!(outcome.ok, "{}", outcome.message);
        assert!(root.path().join("copied.txt").exists());
        assert!(
            !root.path().join("notes.txt").exists(),
            "a move leaves nothing behind"
        );
        assert_eq!(outcome.changed, vec![Location::file(root.path())]);
        assert_eq!(outcome.message, "Moved 1 item");
    }

    #[test]
    fn a_folder_moved_by_a_worker_arrives_with_its_contents() {
        let from = tempfile::tempdir().expect("a folder to move");
        let to = tempfile::tempdir().expect("a folder to move into");
        tree(from.path());
        let queue = Queue::without_source().expect("a pipe can be made");
        let plan = Plan {
            steps: vec![Step::new(
                Location::file(from.path()),
                Location::file(to.path().join("moved")),
            )],
            ..Plan::default()
        };
        paste(&queue.handle(), plan, Transfer::Copy);
        let outcome = operated(&queue);
        assert!(outcome.ok, "{}", outcome.message);
        assert_eq!(
            fs::read(to.path().join("moved").join("deep").join("inner.txt"))
                .expect("a copied file"),
            b"two"
        );
        assert!(
            from.path().join("notes.txt").exists(),
            "a copy leaves the original alone"
        );
    }

    #[test]
    fn a_batch_that_threw_something_away_names_the_folder_once() {
        let root = tempfile::tempdir().expect("a folder to empty");
        tree(root.path());
        let rows = [
            file(root.path(), "notes.txt"),
            folder(root.path(), "deep"),
            file(root.path(), "gone.txt"),
        ];
        let queue = Queue::without_source().expect("a pipe can be made");
        discard(&queue.handle(), &rows, true);
        let outcome = operated(&queue);
        // The third row was never on disk, which is a failure worth reporting —
        // but the two that were must still be counted and the folder still named.
        assert!(!outcome.ok, "a missing row has to be admitted");
        assert!(
            outcome.message.contains("2 items deleted"),
            "{}",
            outcome.message
        );
        assert!(outcome.message.contains("gone.txt"), "{}", outcome.message);
        assert_eq!(outcome.changed, vec![Location::file(root.path())]);
        assert!(!root.path().join("notes.txt").exists());
        assert!(!root.path().join("deep").exists());
    }

    #[test]
    fn an_empty_paste_answers_without_a_worker() {
        let queue = Queue::without_source().expect("a pipe can be made");
        paste(&queue.handle(), Plan::default(), Transfer::Copy);
        let outcome = operated(&queue);
        assert!(!outcome.ok);
        assert_eq!(outcome.message, "Nothing to paste.");
        assert!(outcome.changed.is_empty());
    }

    /// Two rows from different folders, pasted into a folder that already holds
    /// both names: the smallest batch with two collisions.
    fn collisions(root: &std::path::Path) -> (Vec<Entry>, Location) {
        for name in ["x", "y", "dest"] {
            fs::create_dir_all(root.join(name)).expect("a folder to paste into");
        }
        fs::write(root.join("x").join("notes.txt"), b"first").expect("a row to paste");
        fs::write(root.join("y").join("todo.txt"), b"second").expect("a row to paste");
        fs::write(root.join("dest").join("notes.txt"), b"old").expect("a row in the way");
        fs::write(root.join("dest").join("todo.txt"), b"older").expect("a second row in the way");
        (
            vec![
                file(&root.join("x"), "notes.txt"),
                file(&root.join("y"), "todo.txt"),
            ],
            Location::file(root.join("dest")),
        )
    }

    #[test]
    fn a_collision_records_both_sides_and_counts_the_ones_behind_it() {
        let root = tempfile::tempdir().expect("a folder to collide in");
        let (sources, into) = collisions(root.path());
        let planned = plan(&sources, &into, Transfer::Copy);
        assert_eq!(planned.steps.len(), 2, "both rows are still planned");
        assert_eq!(planned.conflicts.len(), 2);
        // The first prompt can promise to answer one more collision; the last has
        // nothing left to promise.
        assert_eq!(planned.conflicts[0].more, 1);
        assert_eq!(planned.conflicts[1].more, 0);
        assert_eq!(planned.conflicts[0].at, 0);
        assert_eq!(planned.conflicts[1].at, 1);
        assert_eq!(planned.conflicts[0].name, "notes.txt");
        assert_eq!(planned.conflicts[0].into, into);
        assert_eq!(
            planned.conflicts[0].incoming,
            Side {
                folder: false,
                // The test listing says zero bytes; what matters is that the
                // incoming side is the row as listed, not a fresh `stat`.
                size: 0,
                modified: None,
            },
            "the incoming row is the one the listing reported"
        );
        let existing = planned.conflicts[0]
            .existing
            .as_ref()
            .expect("the row in the way can be read");
        assert!(!existing.folder);
        assert_eq!(
            existing.size, 3,
            "the bytes actually on disk, not the listing"
        );
        assert!(
            existing.modified.is_some(),
            "a file just written has a modification time"
        );
        // Without an answer the plan keeps the fallback name, so a caller with no
        // prompt still cannot overwrite anything.
        assert_eq!(
            planned.steps[0].to,
            Location::file(root.path().join("dest").join("notes (2).txt"))
        );
        assert!(!planned.steps[0].replace);
    }

    #[test]
    fn a_row_that_cannot_be_read_leaves_the_prompt_without_an_existing_side() {
        let root = tempfile::tempdir().expect("a folder to stat in");
        assert!(side_of(&Location::file(root.path().join("absent"))).is_none());
        fs::create_dir(root.path().join("box")).expect("a folder");
        let dir = side_of(&Location::file(root.path().join("box"))).expect("a folder can be read");
        assert!(dir.folder);
        assert_eq!(
            dir.size, 0,
            "a folder is counted by the prompt in another way"
        );
        fs::write(root.path().join("plain"), b"0123456789").expect("a file");
        let plain = side_of(&Location::file(root.path().join("plain"))).expect("a file");
        assert!(!plain.folder);
        assert_eq!(plain.size, 10);
    }

    #[test]
    fn a_timestamp_before_the_epoch_is_still_a_time() {
        let zero = epoch_seconds(0);
        assert_eq!(zero, SystemTime::UNIX_EPOCH);
        let later = epoch_seconds(3_600);
        assert!(later > zero);
        assert_eq!(
            later
                .duration_since(zero)
                .expect("an hour after the epoch")
                .as_secs(),
            3_600
        );
        // Filesystems with a clock set to before 1970 report negative seconds, and
        // a prompt that dropped them would say nothing about the row.
        let earlier = epoch_seconds(-3_600);
        assert!(earlier < zero);
        assert!(
            epoch_seconds(i64::MAX) >= zero,
            "a clock too far ahead is still shown as a date"
        );
        assert!(
            epoch_seconds(i64::MIN) <= zero,
            "the extremes of what a filesystem can report never stop the prompt"
        );
    }

    #[test]
    fn answering_replace_writes_over_the_row_already_there() {
        let root = tempfile::tempdir().expect("a folder to overwrite in");
        let (sources, into) = collisions(root.path());
        let answered = resolve(plan(&sources[..1], &into, Transfer::Copy), |_| {
            Choose::Replace
        });
        assert!(answered.conflicts.is_empty(), "the plan is safe to run");
        assert_eq!(answered.steps.len(), 1);
        assert_eq!(
            answered.steps[0].to,
            Location::file(root.path().join("dest").join("notes.txt")),
            "replace takes the taken name back"
        );
        assert!(answered.steps[0].replace);
        let queue = Queue::without_source().expect("a pipe can be made");
        paste(&queue.handle(), answered, Transfer::Copy);
        let outcome = operated(&queue);
        assert!(outcome.ok, "{}", outcome.message);
        assert_eq!(
            fs::read(root.path().join("dest").join("notes.txt")).expect("the row that was written"),
            b"first",
            "the old bytes are the ones that went"
        );
        assert!(
            !root.path().join("dest").join("notes (2).txt").exists(),
            "replacing does not also leave a copy behind"
        );
    }

    #[test]
    fn answering_rename_keeps_both_rows() {
        let root = tempfile::tempdir().expect("a folder to keep both in");
        let (sources, into) = collisions(root.path());
        let answered = resolve(plan(&sources[..1], &into, Transfer::Copy), |_| {
            Choose::Rename
        });
        assert!(!answered.steps[0].replace);
        assert_eq!(
            answered.steps[0].to,
            Location::file(root.path().join("dest").join("notes (2).txt"))
        );
        let queue = Queue::without_source().expect("a pipe can be made");
        paste(&queue.handle(), answered, Transfer::Copy);
        let outcome = operated(&queue);
        assert!(outcome.ok, "{}", outcome.message);
        assert_eq!(
            fs::read(root.path().join("dest").join("notes.txt")).expect("the old row"),
            b"old"
        );
        assert_eq!(
            fs::read(root.path().join("dest").join("notes (2).txt")).expect("the new row"),
            b"first"
        );
    }

    #[test]
    fn answering_skip_leaves_the_folder_alone_and_says_so() {
        let root = tempfile::tempdir().expect("a folder to skip into");
        let (sources, into) = collisions(root.path());
        let answered = resolve(plan(&sources, &into, Transfer::Copy), |_| Choose::Skip);
        assert!(answered.steps.is_empty(), "nothing was agreed to write");
        assert_eq!(answered.skipped.len(), 2, "one reason per row declined");
        assert!(
            answered.skipped[0].contains("not pasted"),
            "{}",
            answered.skipped[0]
        );
        assert_eq!(
            fs::read(root.path().join("dest").join("notes.txt")).expect("the row left alone"),
            b"old"
        );
    }

    #[test]
    fn a_standing_answer_answers_the_rows_behind_it_without_asking_again() {
        let root = tempfile::tempdir().expect("a folder to answer once in");
        let (sources, into) = collisions(root.path());
        let asked = std::cell::Cell::new(0_usize);
        let answered = resolve(plan(&sources, &into, Transfer::Copy), |_| {
            asked.set(asked.get() + 1);
            Choose::ReplaceAll
        });
        assert_eq!(asked.get(), 1, "one question for a batch answer");
        assert_eq!(answered.steps.len(), 2, "both rows were still written");
        assert!(
            answered.steps.iter().all(|step| step.replace),
            "the standing answer applies to the rows behind the one asked"
        );
    }

    #[test]
    fn a_standing_rename_leaves_the_fallback_names_in_place() {
        let root = tempfile::tempdir().expect("a folder to rename all in");
        let (sources, into) = collisions(root.path());
        let answered = resolve(plan(&sources, &into, Transfer::Copy), |_| Choose::RenameAll);
        assert_eq!(answered.steps.len(), 2);
        assert!(
            answered.steps.iter().all(|step| !step.replace),
            "a rename promises nothing about the row already there"
        );
        let names: Vec<&str> = answered
            .steps
            .iter()
            .map(|step| step.to.file_name().unwrap_or(""))
            .collect();
        assert_eq!(
            names,
            vec!["notes (2).txt", "todo (2).txt"],
            "both rows take a free name beside the folder"
        );
    }

    #[test]
    fn cancelling_abandons_the_whole_paste_and_says_how_much() {
        let root = tempfile::tempdir().expect("a folder to cancel in");
        let (sources, into) = collisions(root.path());
        let answered = resolve(plan(&sources, &into, Transfer::Copy), |_| Choose::Cancel);
        assert!(answered.steps.is_empty(), "a cancel is not a partial paste");
        assert_eq!(answered.skipped.len(), 1, "one message for the whole batch");
        assert!(
            answered.skipped[0].contains("Paste of 2 items"),
            "{}",
            answered.skipped[0]
        );
        assert!(
            answered.skipped[0].contains("cancelled"),
            "{}",
            answered.skipped[0]
        );
        assert_eq!(
            fs::read(root.path().join("dest").join("notes.txt")).expect("nothing was written"),
            b"old"
        );
    }

    #[test]
    fn a_paste_nobody_asked_about_renames_rather_than_overwrites() {
        let root = tempfile::tempdir().expect("a folder nobody asked in");
        let (sources, into) = collisions(root.path());
        let queue = Queue::without_source().expect("a pipe can be made");
        // The plan goes in straight from `plan`, conflicts and all.
        paste(
            &queue.handle(),
            plan(&sources, &into, Transfer::Copy),
            Transfer::Copy,
        );
        let outcome = operated(&queue);
        assert!(outcome.ok, "{}", outcome.message);
        assert_eq!(
            fs::read(root.path().join("dest").join("notes.txt")).expect("the old row"),
            b"old",
            "a paste that never asked must not destroy anything"
        );
        assert!(root.path().join("dest").join("notes (2).txt").exists());
        assert!(root.path().join("dest").join("todo (2).txt").exists());
    }

    #[test]
    fn replacing_a_folder_merges_into_the_one_already_there() {
        let root = tempfile::tempdir().expect("a folder to merge into");
        let from = root.path().join("src");
        let into = root.path().join("into");
        fs::create_dir_all(from.join("box").join("deep")).expect("a source folder");
        fs::write(from.join("box").join("notes.txt"), b"new").expect("a source file");
        fs::write(from.join("box").join("deep").join("inner.txt"), b"nested")
            .expect("a nested file");
        fs::create_dir_all(into.join("box")).expect("a destination folder");
        fs::write(into.join("box").join("keep.txt"), b"keep").expect("a file to keep");
        fs::write(into.join("box").join("notes.txt"), b"old").expect("a file to lose");
        let answered = resolve(
            plan(
                &[folder(&from, "box")],
                &Location::file(&into),
                Transfer::Copy,
            ),
            |_| Choose::Replace,
        );
        assert!(answered.steps[0].replace);
        let queue = Queue::without_source().expect("a pipe can be made");
        paste(&queue.handle(), answered, Transfer::Copy);
        let outcome = operated(&queue);
        assert!(outcome.ok, "{}", outcome.message);
        let box_path = into.join("box");
        assert_eq!(
            fs::read(box_path.join("notes.txt")).expect("the replaced file"),
            b"new",
            "the file that was in the way is the one that went"
        );
        assert_eq!(
            fs::read(box_path.join("keep.txt")).expect("the untouched file"),
            b"keep",
            "merging does not empty the folder it merges into"
        );
        assert_eq!(
            fs::read(box_path.join("deep").join("inner.txt")).expect("the nested file"),
            b"nested"
        );
    }

    /// The first operation event a worker reported, waiting for it to arrive.
    ///
    /// The harness queue has no main-loop source, so the event sits in the
    /// mutex until it is taken out.
    fn operated(queue: &Queue) -> Outcome {
        for _ in 0..2000 {
            for event in queue.take() {
                if let Event::Operation { outcome } = event {
                    return outcome;
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        panic!("no operation was ever reported");
    }

    #[test]
    fn a_prompt_starts_with_the_suggested_name_and_refuses_an_empty_one() {
        crate::ui::tk::ui(|| {
            let window = compose("Rename", "New name:", "notes.txt", "_Rename", None, |_| {
                unreachable!("nothing was accepted")
            });
            let text = find(&window, "ufm-prompt-text")
                .expect("the field is in the prompt")
                .downcast::<gtk::Entry>()
                .expect("the field is an entry");
            assert_eq!(text.text(), "notes.txt");
            let status = find(&window, "ufm-prompt-status")
                .expect("there is a status line")
                .downcast::<gtk::Label>()
                .expect("the status line is a label");
            assert!(!status.get_visible(), "nothing has gone wrong yet");
            let ok = find(&window, "ufm-prompt-ok")
                .expect("the button is in the prompt")
                .downcast::<gtk::Button>()
                .expect("the button is a button");
            text.set_text("   ");
            ok.emit_clicked();
            assert!(status.get_visible(), "a blank name has to be explained");
            assert_eq!(status.label().as_str(), "A name is needed.");
        });
    }

    #[test]
    fn a_prompt_hands_the_typed_name_over_once() {
        crate::ui::tk::ui(|| {
            let (tx, rx) = mpsc::channel();
            let window = compose(
                "New Folder",
                "Folder name:",
                "",
                "_Create",
                None,
                move |typed| {
                    tx.send(typed).expect("the test is still listening");
                },
            );
            let text = find(&window, "ufm-prompt-text")
                .expect("the field is in the prompt")
                .downcast::<gtk::Entry>()
                .expect("the field is an entry");
            text.set_text("Reports");
            // The button rather than the entry: activating an entry reaches the
            // default widget only through the key controller, which needs a
            // mapped window, and no test here has one.
            find(&window, "ufm-prompt-ok")
                .expect("the button is in the prompt")
                .downcast::<gtk::Button>()
                .expect("the button is a button")
                .emit_clicked();
            assert_eq!(
                rx.recv_timeout(std::time::Duration::from_secs(10))
                    .expect("the callback ran"),
                "Reports"
            );
        });
    }

    #[test]
    fn a_rename_prompt_selects_the_stem_and_leaves_the_extension() {
        crate::ui::tk::ui(|| {
            let window = compose(
                "Rename",
                "New name:",
                "report.pdf",
                "_Rename",
                Some((0, 6)),
                |_| {},
            );
            let text = find(&window, "ufm-prompt-text")
                .expect("the field is in the prompt")
                .downcast::<gtk::Entry>()
                .expect("the field is an entry");
            assert_eq!(text.selection_bounds(), Some((0, 6)));
        });
    }

    /// Pull one named widget out of a prompt, the way the `connect` tests reach
    /// their dialog.
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
