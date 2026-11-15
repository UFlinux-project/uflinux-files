//! Unpacking an archive into the folder that holds it.
//!
//! Browsing inside a container is read-only, and extracting is the one write the
//! interface offers: it is what a user reaching for an archive actually wants to
//! do, and unlike editing a member in place it cannot leave a half-written
//! container behind. The work itself belongs to `ufm-archive` — this module only
//! decides where the bytes land and how the answer gets back to the window.
//!
//! The destination is computed rather than asked for. A file manager that stops
//! for a dialog on every command is the reason people stop using file managers,
//! and "a folder named after the archive, beside it" is the answer in nearly
//! every case; the counter [`ops::unique_name`] adds is what makes the second
//! archive in the same folder work just as quietly. A destination chooser belongs
//! with the drag-and-drop and the conflict dialog, not here.
//!
//! Like the other operations, the work runs on a worker and reports through the
//! window's listing queue (see [`super::job`]). Extraction streams one member at
//! a time, so it costs a buffer rather than a copy of the container, but an
//! archive with ten thousand members still blocks for as long as it takes — long
//! enough that it registers a job, which is what lets the window say so and let
//! the user stop it.

use std::path::{Path, PathBuf};

use ufm_archive::Format;
use ufm_core::{Error, Location};

use super::job::{Event, Handle};
use super::jobs::{Jobs, Kind, Unit};
use super::ops::{self, Outcome};

/// Unpack the container at `location` into a folder beside it, on a worker.
///
/// Reports through the queue whatever happens, including the two cases where no
/// worker is started: a location that is not a file the kernel can see, and a
/// container with no folder to unpack into. A command that produces no answer at
/// all is indistinguishable from one that is still running. Those two refusals
/// report as plain operations rather than as jobs, because a command that never
/// started has nothing to show a bar for.
///
/// The whole container is unpacked whichever shape `location` had. That is what
/// makes the two entry points agree: the row the user selected is a plain path and
/// the folder on screen inside an archive is a position in one, and the reader only
/// speaks the second — so the path is turned into the root of its own archive
/// rather than passed through.
pub fn extract(handle: &Handle, jobs: &Jobs, location: Location) {
    let Some(container) = container_of(&location) else {
        handle.send(Event::Operation {
            outcome: refused(&format!(
                "{}: extract needs an archive file.",
                location.display()
            )),
        });
        return;
    };
    // Owned, because the worker below takes it and the borrow was into `location`,
    // which does not outlive this call. The message for a failure names the
    // container, so the worker needs the path rather than just the name.
    let container = container.to_path_buf();
    let Some(into) = destination(&container) else {
        handle.send(Event::Operation {
            outcome: refused(&format!(
                "{}: no folder to extract into.",
                container.display()
            )),
        });
        return;
    };
    // The folder the new one appears in is the one whose listing just went stale.
    // Measured from the container rather than from `location`, because the two
    // differ when the command was invoked from inside the archive: the folder on
    // screen is then a position in the container, and the folder that changed is
    // the one holding it.
    let changed = Location::file(container.clone()).parent();
    // Built directly rather than through `Location::archive`, which validates
    // components the empty inner path has no need of.
    let whole = Location::Archive {
        archive: container.clone(),
        inner: String::new(),
    };
    let name = container.file_name().map_or_else(
        || "archive".to_string(),
        |name| name.to_string_lossy().into_owned(),
    );
    let label = name.clone();
    // No total to begin with: the number of members is not known until the
    // container's index has been read, which happens on the worker. The first
    // report brings it, and a container unpacked by a helper process never brings
    // one at all — the strip says "Extracting" without a tally for that, which is
    // honest where a bar stuck at zero would look like a stall.
    ops::spawn_job(
        handle,
        jobs,
        Kind::Extract,
        &name,
        None,
        Unit::Rows,
        move |brake, reporter| {
            let mut told = |written: usize, total: usize, _bytes: u64| {
                let done = u64::try_from(written).unwrap_or(u64::MAX);
                let whole = u64::try_from(total).unwrap_or(u64::MAX);
                reporter.update(&label, done, Some(whole), Unit::Rows);
            };
            match ufm_archive::extract(&whole, &into, brake.vfs(), Some(&mut told)) {
                Ok(report) => arrived(report, changed),
                // A stopped extraction is not a failed one: the members that had
                // been written are removed by the extractor itself, so what the
                // user did was decline work rather than have it go wrong.
                Err(error) if error.is_cancelled() => Outcome {
                    ok: true,
                    message: format!("Extracting {label} stopped."),
                    changed: changed.into_iter().collect(),
                },
                Err(error) => Outcome {
                    ok: false,
                    message: complaint(&container, &error),
                    changed: Vec::new(),
                },
            }
        },
    );
}

/// The folder to unpack `container` into: a sibling named after the container
/// without the suffix that marks it as one, made unique against whatever is
/// already there.
///
/// [`Format::base_name`] is the same rule that names the single member of a
/// `.gz`, so `notes.txt.gz` browses to `notes.txt` and extracts to `notes.txt`
/// rather than to two different names for one file.
///
/// `None` means there is no folder to put one in: a path with no file name, or
/// one with no parent.
#[must_use]
pub fn destination(container: &Path) -> Option<PathBuf> {
    let folder = container.parent()?;
    container.file_name()?;
    let base = Format::base_name(container);
    // `symlink_metadata` rather than `exists`: a dangling symlink is a name that
    // is taken, and unpacking into it would fail on `create_dir` rather than
    // quietly picking the next name in the series.
    let taken = |name: &str| folder.join(name).symlink_metadata().is_ok();
    Some(folder.join(ops::unique_name(&base, taken)))
}

/// Whether `location` names a container this command can unpack, and where that
/// container is on disk.
///
/// Two shapes count. A file whose name says it is a container is the row the user
/// selected. The root of a container also counts, and names the file holding it: a
/// user who has browsed into an archive is looking straight at the thing they want
/// unpacked, and a command that refused there would be refusing at the one moment
/// it is most likely to be asked for.
///
/// Two do not. A position partway inside a container is left alone, because the
/// command unpacks a whole container and a user standing in `docs/` who asks for
/// that gets `docs/` back — the folder they are looking at is not what would
/// arrive, and a subfolder of an archive has no name the rule above could give it.
/// A container inside another container is left out for a related reason: the
/// nested one has to arrive before it can be opened, so the command would have to
/// unpack the outer file first and then say it had unpacked the inner one.
#[must_use]
pub fn container_of(location: &Location) -> Option<&Path> {
    let path = location.as_path()?;
    if location.is_archive()
        && location
            .archive_inner()
            .is_some_and(|inner| !inner.is_empty())
    {
        return None;
    }
    Format::from_name(path).is_some().then_some(path)
}

/// How a container that did unpack looks to the window: what arrived, and the
/// folder it arrived in.
fn arrived(report: ufm_archive::Report, changed: Option<Location>) -> Outcome {
    let mut outcome = Outcome {
        // Members that were left out are the interesting part of the answer, and
        // a partial success is reported as one for the same reason a partial
        // paste is.
        ok: report.skipped.is_empty(),
        message: report.summary(),
        changed: changed.into_iter().collect(),
    };
    if !report.skipped.is_empty() {
        outcome.message = format!("{}; {}", outcome.message, report.skipped.join("; "));
    }
    outcome
}

/// A command that stopped before starting any work.
fn refused(message: &str) -> Outcome {
    Outcome {
        ok: false,
        message: message.to_string(),
        changed: Vec::new(),
    }
}

/// Word a failure for the banner, naming the container unless the error already
/// does. An archive failure carries the path it came from; a filesystem failure
/// does not, and `Permission denied` on its own says nothing about which of the
/// archives in the folder it happened to.
fn complaint(container: &Path, error: &Error) -> String {
    let text = error.to_string();
    let name = container.display().to_string();
    if text.contains(&name) {
        text
    } else {
        format!("{name}: {text}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Write as _;

    fn touch(folder: &Path, name: &str) -> PathBuf {
        let path = folder.join(name);
        fs::File::create(&path)
            .expect("create")
            .write_all(b"PK\x03\x04")
            .expect("write");
        path
    }

    #[test]
    fn a_container_unpacks_beside_itself_named_after_itself() {
        let dir = tempfile::tempdir().expect("tempdir");
        let container = touch(dir.path(), "photos.tar.gz");
        assert_eq!(
            destination(&container),
            Some(dir.path().join("photos")),
            "the folder is named for the contents, not for the wrapper"
        );
    }

    #[test]
    fn a_container_named_after_a_package_format_loses_that_suffix_too() {
        let dir = tempfile::tempdir().expect("tempdir");
        let container = touch(dir.path(), "app.apk");
        assert_eq!(destination(&container), Some(dir.path().join("app")));
        let container = touch(dir.path(), "backup.7z");
        assert_eq!(destination(&container), Some(dir.path().join("backup")));
    }

    #[test]
    fn the_second_container_in_a_folder_gets_a_number() {
        let dir = tempfile::tempdir().expect("tempdir");
        let container = touch(dir.path(), "site.zip");
        fs::create_dir(dir.path().join("site")).expect("existing folder");
        assert_eq!(
            destination(&container),
            Some(dir.path().join("site (2)")),
            "an existing folder of the same name is not overwritten"
        );
    }

    #[test]
    fn a_name_that_is_only_a_suffix_keeps_it() {
        // `.zip` is a dotfile, not a container with nothing in front of it, so
        // the folder is named after the whole thing — and then has to move along,
        // because the container is standing in the way of its own name.
        let dir = tempfile::tempdir().expect("tempdir");
        let container = touch(dir.path(), ".zip");
        assert_eq!(
            destination(&container),
            Some(dir.path().join(".zip (2)")),
            "a folder cannot be unpacked on top of the archive it came from"
        );
    }

    #[test]
    fn a_path_with_no_parent_has_nowhere_to_unpack() {
        assert_eq!(destination(Path::new("/")), None);
        assert_eq!(destination(Path::new("a/b/..")), None);
    }

    #[test]
    fn a_container_inside_a_container_is_not_something_to_extract() {
        // Two levels deep: the row is a container, but the location is a position
        // inside another one, and unpacking here would mean unpacking the outer
        // file while claiming to unpack the inner.
        let nested = Location::archive("/t/outer.tar", &["outer.zip"]).expect("archive location");
        assert_eq!(container_of(&nested), None);
        assert_eq!(container_of(&Location::computer()), None);
        let plain = touch(tempfile::tempdir().expect("tempdir").path(), "notes.txt");
        assert_eq!(container_of(&Location::file(&plain)), None);
        let zip = touch(tempfile::tempdir().expect("tempdir").path(), "a.zip");
        assert_eq!(container_of(&Location::file(&zip)), Some(zip.as_path()));
    }

    #[test]
    fn the_container_being_browsed_is_something_to_extract() {
        // The root of an archive is the archive: browsing in and unpacking is the
        // sequence a user reaches for without thinking about the difference.
        let root = Location::archive("/t/a.zip", &[] as &[&str]).expect("archive location");
        assert_eq!(
            container_of(&root).map(Path::to_path_buf),
            Some(PathBuf::from("/t/a.zip"))
        );
        let deep = Location::archive("/t/a.zip", &["docs"]).expect("archive location");
        assert_eq!(container_of(&deep), None);
    }

    #[test]
    fn an_error_that_already_names_its_container_is_left_as_it_is() {
        let error = Error::Archive("/t/a.zip: member: bad bytes".to_string());
        assert_eq!(
            complaint(Path::new("/t/a.zip"), &error),
            "archive error: /t/a.zip: member: bad bytes"
        );
    }

    #[test]
    fn an_io_failure_gains_the_container_it_happened_to() {
        let error = Error::Io(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "Permission denied",
        ));
        assert_eq!(
            complaint(Path::new("/t/a.zip"), &error),
            "/t/a.zip: Permission denied"
        );
    }

    #[test]
    fn a_refused_command_says_so_and_asks_for_no_refresh() {
        let outcome = refused("nothing to do");
        assert!(!outcome.ok);
        assert_eq!(outcome.message, "nothing to do");
        assert!(outcome.changed.is_empty());
    }
}
