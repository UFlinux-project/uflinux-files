//! Putting an operation back.
//!
//! The job table records what an operation did while it did it (see
//! [`super::jobs`]); this module is what reads that record and reverses it. The
//! two halves are kept apart on purpose: [`ops`] decides what happened and worded
//! it for the banner, and only here does anything ask how a thing is undone.
//!
//! Three effects cover everything the file operations do. A row was moved, so it
//! goes back. A row was trashed, so it is fetched from the trash and goes back.
//! A row was created, so it is taken away again. Anything else — a delete that
//! kept no copy, a folder replaced wholesale — is reported as gone rather than
//! pretended otherwise, because an undo that quietly loses a second file is worse
//! than an undo that says it cannot help.
//!
//! ## Finding a trashed row again
//!
//! [`gio::FileExt::trash`] implements the freedesktop trash specification: the
//! bytes move to `<trash>/files/` and a `.trashinfo` sidecar in `<trash>/info/`
//! records where they came from. The sidecar's own name is the name the bytes got
//! inside `files/`, which is what makes a row findable without a second index —
//! and it is why the counter that two rows of the same name get appears in both
//! places rather than in neither.
//!
//! The `trash:` URI would be the shorter way to ask, but answering it is `GVfs`'s
//! job, and a minimal system has no `GVfs`. Reading the directory the specification
//! defines works wherever the trash wrote, which is the same set of systems —
//! see [`ufm_core::paths::trash_dir`].

use std::collections::HashMap;
use std::ffi::OsString;
use std::fs;
use std::os::unix::ffi::OsStringExt;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use gio::prelude::*;

use ufm_core::Location;
use ufm_core::paths;

use super::job::Handle;
use super::jobs::{Brake, Jobs, Kind, Reporter, Unit};
use super::ops::{self, Outcome, Step};

/// The suffix the specification gives a sidecar.
const SIDECAR: &str = ".trashinfo";

/// Put the window's last operation back, on a worker.
///
/// The record is spent before the work starts rather than after it: an undo that
/// failed has usually changed part of the folder, so the record no longer describes
/// what is there and running it again could take a row that belongs to something
/// else. One press, one attempt, and whatever went wrong is in the banner.
///
/// A job like the operations it reverses — restoring a folder out of the trash is
/// the same kind of disk work — so it registers a row on the strip and can be
/// stopped by the same button. Undoing is not itself undoable: the outcome this
/// reports carries no record, which is what keeps the feature one deep.
pub fn undo(handle: &Handle, jobs: &Jobs) {
    let Some(record) = jobs.take() else { return };
    let rows = record.rows();
    let total = u64::try_from(rows).ok();
    ops::spawn_job(
        handle,
        jobs,
        Kind::Undo,
        &ops::count(rows),
        total,
        Unit::Rows,
        move |brake, reporter| record.apply(brake, reporter),
    );
}

/// The per-filesystem trash directory, named for the user whose it is.
fn user_trash() -> String {
    format!(".Trash-{}", paths::uid())
}

/// What one operation did, in the terms an inverse needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    /// Rows sent to the trash, each with the entry holding its bytes when the
    /// trash could be read.
    Trashed(Vec<Trashed>),
    /// Rows that changed place: where each was, and where it went. A rename is one
    /// pair, and so is a move of one row across the room.
    Moved(Vec<(Location, Location)>),
    /// Rows that were not there before, which undo takes away again.
    Created(Vec<Location>),
}

impl Effect {
    /// How many rows this accounts for, which is what the strip counts out.
    #[must_use]
    pub fn rows(&self) -> usize {
        match self {
            Self::Trashed(rows) => rows.len(),
            Self::Moved(pairs) => pairs.len(),
            Self::Created(rows) => rows.len(),
        }
    }

    /// The sentence for the banner, in the past tense and naming the rows.
    ///
    /// Three different verbs, because the three effects leave the folder in three
    /// different states and a banner that said only "Undone" would not say whether
    /// a file had come back or gone away.
    #[must_use]
    pub fn said(&self, done: usize) -> String {
        let rows = ops::count(done);
        match self {
            Self::Trashed(_) => format!("Moved back {rows} from the trash"),
            Self::Moved(_) => format!("Moved back {rows}"),
            Self::Created(_) => format!("Removed {rows}"),
        }
    }
}

/// One row that went to the trash, and where its bytes ended up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Trashed {
    /// The folder and name the row will be restored to.
    pub home: Location,
    /// The entry inside the trash that holds it, when one could be matched to
    /// `home`. `None` says the row is gone but not where it went: a trash on a
    /// filesystem this program cannot see, or a sidecar another program wrote in
    /// a shape this one does not read.
    pub entry: Option<Location>,
}

/// What a step that replaces found standing in its way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Displaced {
    /// Nothing was there, which is the ordinary case: only a step answering a
    /// collision looks, and even then the row may have gone meanwhile.
    Nothing,
    /// A file. Undo may take the row that replaced it away again, because the
    /// bytes that were here are either in the stash or reported as lost.
    File,
    /// A folder, whose contents went with it. Undo has to leave the replacement
    /// where it is: the merge wrote into a tree that already held rows this
    /// operation never touched, and deleting the whole thing would take those too.
    Folder,
}

/// What one operation did, whole: its own effect, and whatever a replacement
/// destroyed on the way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    /// What the operation did to the rows it was asked about.
    pub effect: Effect,
    /// Rows a collision answer destroyed, each as the name it lived under and the
    /// path it was kept at. Put back after the effect is reversed, so that the
    /// name a move vacated is free for the file that used to hold it.
    pub replaced: Vec<(Location, Location)>,
    /// Rows that are gone and cannot come back, each already worded for the banner.
    pub lost: Vec<String>,
    /// The directory the replaced rows were kept in, when anything was.
    pub stash: Option<PathBuf>,
}

impl Record {
    /// A record for an operation that moved one row and displaced nothing, which
    /// is what a rename always is: a taken name is refused before any work starts.
    #[must_use]
    pub fn moved(from: &Location, to: &Location) -> Self {
        Self {
            effect: Effect::Moved(vec![(from.clone(), to.clone())]),
            replaced: Vec::new(),
            lost: Vec::new(),
            stash: None,
        }
    }

    /// A record for an operation that made one thing and destroyed nothing, which
    /// is what making a folder or unpacking an archive always is: the name is
    /// grown against whatever is there rather than taken from it.
    #[must_use]
    pub fn created(path: Location) -> Self {
        Self {
            effect: Effect::Created(vec![path]),
            replaced: Vec::new(),
            lost: Vec::new(),
            stash: None,
        }
    }

    /// How many rows undo will touch: the operation's own, and the ones a
    /// replacement displaced.
    #[must_use]
    pub fn rows(&self) -> usize {
        self.effect.rows() + self.replaced.len()
    }

    /// Throw away the bytes this record was keeping. Called when a newer
    /// operation takes the record's place, which is what bounds what a session
    /// holds: one operation's displaced files, for as long as it is the last one.
    pub fn sweep(&self) {
        if let Some(dir) = &self.stash {
            let _ = fs::remove_dir_all(dir);
        }
    }

    /// Put the operation back, reporting each row as it lands.
    ///
    /// The effect first and the displaced rows after, in the order they were
    /// displaced reversed. The order is not a preference: a move that replaced a
    /// file has to vacate the name before the file that used to hold it can go
    /// back into it, and doing it the other way round spends a rename on a name
    /// that is about to be taken again.
    pub fn apply(self, brake: &Brake, reporter: &mut Reporter) -> Outcome {
        let whole = u64::try_from(self.rows()).unwrap_or(u64::MAX);
        let Record {
            effect,
            replaced,
            lost,
            stash,
        } = self;
        let mut undoing = Undoing::new(whole);
        let stopped = match &effect {
            Effect::Trashed(rows) => back_from_trash(rows, &mut undoing, brake, reporter),
            Effect::Moved(pairs) => back_from_move(pairs, &mut undoing, brake, reporter),
            Effect::Created(made) => take_away(made, &mut undoing, brake, reporter),
        } || restore(&replaced, &mut undoing, brake, reporter);
        let outcome = undoing.into_outcome(&effect, stopped);
        // The kept bytes have either gone back or been given up on; either way
        // this is the last moment anything could ask for them.
        if let Some(dir) = &stash {
            let _ = fs::remove_dir_all(dir);
        }
        outcome.with_skipped(&lost)
    }
}

/// The running total while a record is being applied, so that the three effects
/// and the displaced rows can share one set of counters without the bookkeeping
/// being written four times.
struct Undoing {
    outcome: Outcome,
    failures: Vec<String>,
    done: usize,
    whole: u64,
}

impl Undoing {
    fn new(whole: u64) -> Self {
        Self {
            outcome: Outcome::done(String::new()),
            failures: Vec::new(),
            done: 0,
            whole,
        }
    }

    /// Fold one restored row into the report: the folder it landed in needs a
    /// re-read, the folder it left needs one too, and the strip counts it.
    fn spoke(
        &mut self,
        reporter: &mut Reporter,
        name: &str,
        landed: &Location,
        left: Option<Location>,
    ) {
        self.outcome.note(landed);
        if let Some(left) = left {
            self.outcome.note(&left);
        }
        self.done += 1;
        reporter.update(
            name,
            u64::try_from(self.done).unwrap_or(u64::MAX),
            Some(self.whole),
            Unit::Rows,
        );
    }

    fn failed(&mut self, name: &str, message: impl AsRef<str>) {
        self.failures.push(format!("{name}: {}", message.as_ref()));
    }

    /// Word the banner. The message names the effect that was reversed rather
    /// than the operation that caused it, because by now the operation is
    /// whatever the user last did and the useful fact is what the folder looks
    /// like as a result.
    fn into_outcome(self, effect: &Effect, stopped: bool) -> Outcome {
        let mut parts = Vec::new();
        if stopped {
            parts.push("Undoing stopped".to_string());
        }
        if self.done > 0 {
            parts.push(effect.said(self.done));
        }
        let mut outcome = self.outcome;
        outcome.ok = self.failures.is_empty();
        outcome.message = ops::report(&parts, &self.failures);
        outcome
    }
}

/// Fetch each trashed row out of the trash and put it back where it came from.
/// Returns whether the user stopped the job partway.
fn back_from_trash(
    rows: &[Trashed],
    undoing: &mut Undoing,
    brake: &Brake,
    reporter: &mut Reporter,
) -> bool {
    for row in rows {
        if brake.pulled() {
            return true;
        }
        let name = row.home.file_name().unwrap_or("item").to_string();
        let Some(entry) = row.entry.as_ref() else {
            undoing.failed(&name, "not in the trash");
            continue;
        };
        let place = ops::accept(&row.home, ops::exists);
        match carry(entry, &place, brake) {
            Ok(()) => undoing.spoke(
                reporter,
                &name,
                &place,
                entry.parent().or_else(|| row.home.parent()),
            ),
            Err(message) => undoing.failed(&name, message),
        }
    }
    false
}

/// Move each row back to the name it had before the operation. Returns whether
/// the user stopped the job partway.
fn back_from_move(
    pairs: &[(Location, Location)],
    undoing: &mut Undoing,
    brake: &Brake,
    reporter: &mut Reporter,
) -> bool {
    for (was, went) in pairs {
        if brake.pulled() {
            return true;
        }
        let name = was.file_name().unwrap_or("item").to_string();
        let place = ops::accept(was, ops::exists);
        match carry(went, &place, brake) {
            Ok(()) => undoing.spoke(reporter, &name, &place, went.parent()),
            Err(message) => undoing.failed(&name, message),
        }
    }
    false
}

/// Take away each row the operation made. Returns whether the user stopped the
/// job partway.
fn take_away(
    made: &[Location],
    undoing: &mut Undoing,
    brake: &Brake,
    reporter: &mut Reporter,
) -> bool {
    for path in made {
        if brake.pulled() {
            return true;
        }
        let name = path.file_name().unwrap_or("item").to_string();
        match erase(path, brake) {
            // The folder that lost the row is the one needing a re-read; the row
            // itself is gone by now, so noting it would ask for a walk of a
            // folder that no longer exists.
            Ok(()) => {
                if let Some(where_it_was) = path.parent() {
                    undoing.spoke(reporter, &name, &where_it_was, None);
                }
            }
            Err(message) => undoing.failed(&name, message),
        }
    }
    false
}

/// Put back the rows a collision answer destroyed, newest displacement first.
/// Returns whether the user stopped the job partway.
fn restore(
    replaced: &[(Location, Location)],
    undoing: &mut Undoing,
    brake: &Brake,
    reporter: &mut Reporter,
) -> bool {
    for (home, kept) in replaced.iter().rev() {
        if brake.pulled() {
            return true;
        }
        let name = home.file_name().unwrap_or("item").to_string();
        let place = ops::accept(home, ops::exists);
        match carry(kept, &place, brake) {
            Ok(()) => undoing.spoke(reporter, &name, &place, home.parent()),
            Err(message) => undoing.failed(&name, message),
        }
    }
    false
}

/// Move one row back to where it belongs. No overwrite flag anywhere: undo takes
/// a free name rather than destroying something it did not put there, which is the
/// same rule the operations themselves play by.
fn carry(from: &Location, to: &Location, brake: &Brake) -> Result<(), String> {
    let source = ops::file(from).ok_or("not a file on disk")?;
    let target = ops::file(to).ok_or("not a file on disk")?;
    source
        .move_(&target, gio::FileCopyFlags::NONE, Some(brake.gio()), None)
        .map_err(|error| error.to_string())
}

/// Take away a row the operation created, and everything it brought with it.
fn erase(path: &Location, brake: &Brake) -> Result<(), String> {
    let file = ops::file(path).ok_or("not a file on disk")?;
    ops::remove_all(&file, brake).map_err(|error| error.to_string())
}

/// The bytes a replacing step destroyed, held until the window forgets them.
///
/// Only a plain file is kept. A folder is not, because keeping it means moving a
/// whole tree and the tree may be as large as the one that replaced it — and
/// because the merge that destroyed it wrote into it rather than over it, so
/// there is no single thing to put back.
#[derive(Debug, Default)]
pub struct Stash {
    dir: Option<PathBuf>,
    kept: Vec<(Location, Location)>,
    lost: Vec<String>,
}

impl Stash {
    /// Set aside whatever stands at `step.to`, so that a step which replaces it
    /// can have the bytes back. Called before the step runs, because after it
    /// there is nothing left to read.
    pub fn keep(&mut self, step: &Step, brake: &Brake) -> Displaced {
        let Some(target) = ops::file(&step.to) else {
            return Displaced::Nothing;
        };
        if !target.query_exists(Some(brake.gio())) {
            return Displaced::Nothing;
        }
        if ops::is_directory(&target, brake) {
            self.lost.push(format!(
                "replaced {}, whose old contents are gone.",
                step.label()
            ));
            return Displaced::Folder;
        }
        let name = step.to.file_name().unwrap_or("item");
        let gone = || format!("replaced {}, and could not keep it.", step.label());
        let Some(dir) = self.place() else {
            self.lost.push(gone());
            return Displaced::File;
        };
        // A name of its own inside the stash, so that two rows replaced by one
        // paste do not overwrite each other before either is put back.
        let kept = ops::accept(&Location::file(dir.join(name)), ops::exists);
        let Some(store) = ops::file(&kept) else {
            self.lost.push(gone());
            return Displaced::File;
        };
        match target.move_(&store, gio::FileCopyFlags::NONE, Some(brake.gio()), None) {
            Ok(()) => {
                self.kept.push((step.to.clone(), kept));
                Displaced::File
            }
            Err(error) => {
                self.lost
                    .push(format!("replaced {}: {error}", step.label()));
                Displaced::File
            }
        }
    }

    /// A step that failed after displacing a row: put the row back rather than
    /// leave the folder short of a file the operation never actually replaced.
    pub fn return_it(&mut self, to: &Location, brake: &Brake) {
        let Some(at) = self.kept.iter().position(|(home, _)| home == to) else {
            return;
        };
        let (home, kept) = self.kept.remove(at);
        if carry(&kept, &home, brake).is_err() {
            // Still in the stash, so the record can still try.
            self.kept.push((home, kept));
        }
    }

    /// What to write into a [`Record`]: the rows kept, the rows given up on, and
    /// the directory to sweep when the record is done with.
    pub fn into_parts(self) -> (Vec<(Location, Location)>, Vec<String>, Option<PathBuf>) {
        (self.kept, self.lost, self.dir)
    }

    /// The stash directory, made if this is the first row kept. Made late so that
    /// the operations that replace nothing — nearly all of them — cost no `mkdir`.
    fn place(&mut self) -> Option<PathBuf> {
        if let Some(dir) = &self.dir {
            return Some(dir.clone());
        }
        let dir = paths::undone_dir();
        fs::create_dir_all(&dir).ok()?;
        self.dir = Some(dir.clone());
        Some(dir)
    }
}

/// The trash directories worth reading for a row that lived at `path`.
///
/// The user's own trash first, since that is where a row on the home filesystem
/// goes. Then a `.Trash-$UID` beside any ancestor of the row, which is where the
/// specification says a row goes when its filesystem is mounted elsewhere: at the
/// root of that filesystem, which walking up from the row always reaches.
#[must_use]
pub fn roots(path: &Path) -> Vec<PathBuf> {
    let mut out = vec![paths::trash_dir()];
    let named = user_trash();
    let mut cursor = path.parent();
    while let Some(dir) = cursor {
        let candidate = dir.join(&named);
        if !out.contains(&candidate) && candidate.is_dir() {
            out.push(candidate.clone());
        }
        cursor = dir.parent();
    }
    out
}

/// Every row sitting in the given trash directories, keyed by where it came from.
///
/// One read of `info/` per directory, which is why the whole batch is looked up
/// at once rather than row by row: throwing forty rows at the trash should cost
/// one directory read to make them all undoable, not forty.
///
/// When two sidecars name the same origin — a file trashed, restored, and
/// trashed again — the newer one wins, because it is the one holding the bytes
/// that are there now.
#[must_use]
pub fn index(roots: &[PathBuf]) -> HashMap<PathBuf, Location> {
    let mut found: HashMap<PathBuf, (Location, Option<SystemTime>)> = HashMap::new();
    for root in roots {
        let Some(sidecars) = fs::read_dir(root.join("info")).ok() else {
            continue;
        };
        for sidecar in sidecars.flatten() {
            let Some(entry) = entry_of(&sidecar.path()) else {
                continue;
            };
            let (home, at) = entry;
            let newer = found.get(&home).is_none_or(|(_, held)| {
                at.is_some_and(|when| held.is_none_or(|older| when > older))
            });
            if newer {
                let entry = sidecar_path(root, &sidecar.path());
                found.insert(home, (entry, at));
            }
        }
    }
    found
        .into_iter()
        .map(|(home, (entry, _))| (home, entry))
        .collect()
}

/// Where a sidecar says its row came from, and when it was thrown away — `None`
/// for anything that is not a sidecar, or is one whose row is no longer there.
fn entry_of(sidecar: &Path) -> Option<(PathBuf, Option<SystemTime>)> {
    if sidecar.extension().and_then(|e| e.to_str()) != Some(&SIDECAR[1..]) {
        return None;
    }
    let home = origin(&fs::read(sidecar).ok()?)?;
    let when = fs::metadata(sidecar).ok().and_then(|m| m.modified().ok());
    Some((home, when))
}

/// The `Path=` line of a sidecar, unescaped.
///
/// `GIO` writes it percent-encoded, so a name with a space, a `%` or a non-ASCII
/// letter arrives as bytes rather than as the text a reader would type. Decoding
/// into bytes rather than into `str` is what lets a name that is not valid `UTF-8`
/// still be found: the filesystem allows one, and a trash that could only restore
/// the printable rows would be the surprising kind of lossless.
fn origin(sidecar: &[u8]) -> Option<PathBuf> {
    let line = sidecar
        .split(|byte| *byte == b'\n')
        .find(|line| line.starts_with(b"Path="))?;
    let raw: Vec<u8> = percent_encoding::percent_decode(line.strip_prefix(b"Path=")?).collect();
    if raw.is_empty() {
        return None;
    }
    let home = PathBuf::from(OsString::from_vec(raw));
    home.is_absolute().then_some(home)
}

/// The entry a sidecar names: the same folder, `files/`, under the sidecar's own
/// name minus its suffix. The bytes may or may not still be there — an emptied
/// trash looks exactly like this from here, and undo reports the failure when it
/// tries to move the row.
fn sidecar_path(root: &Path, sidecar: &Path) -> Location {
    let name = sidecar
        .file_name()
        .map_or_else(String::new, |n| n.to_string_lossy().to_string());
    let bare = name.strip_suffix(SIDECAR).unwrap_or(&name);
    Location::file(root.join("files").join(bare))
}
