//! Writing a container's members out to a real folder.
//!
//! Browsing an archive is read-only everywhere else in the workspace, and that is
//! the right default: a container is one file, and editing inside it means
//! rewriting it. Extraction is the one write operation worth having, because it
//! turns a container into an ordinary folder that every other program can use —
//! which is what a user means by "unzip this" far more often than by "edit this
//! member in place".
//!
//! There are two ways to do it and this module uses both:
//!
//! - **The whole archive at once**, through the tool that owns the format —
//!   `unzip`, `tar`, `7z`. One process, no re-encode, and the tool's own rules
//!   about permissions and links. This is what makes extracting a 4 GiB container
//!   cost about what `unzip` would cost.
//! - **Member by member** through this crate's own readers, which is the only
//!   option for a single selected folder or file, for a `.gz` that holds one
//!   compressed file, and on a system where the helper is not installed.
//!
//! The second path is also the fallback for the first: a `tar` that cannot handle
//! its own compression still extracts correctly through the reader here, so the
//! fast path is an optimisation and never a requirement.
//!
//! Members are streamed, never buffered, for the same reason listings are: a
//! 2 GiB file inside an archive must not become a 2 GiB allocation in a process
//! with a 90 MiB budget. Each file is written to a temporary name and renamed, so
//! a container that fails halfway leaves no member that looks complete, and the
//! destination folder is removed whole when the extraction fails, so a cancelled
//! or failed extract leaves nothing to clean up by hand.

use std::fs::{self, File};
use std::io::Write;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

use ufm_core::entry::human_size;
use ufm_core::error::{Error, Result};
use ufm_core::location::Location;
use ufm_vfs::Cancel;

use crate::command;
use crate::format::Format;
use crate::reader::{ArchiveReader, Opened};
use crate::toc::{NodeKind, Toc};

/// A symlink target longer than this is not a link, it is a file that was recorded
/// as one. `PATH_MAX` is the real ceiling; anything past this is a damaged
/// container and is reported rather than written.
const MAX_LINK: usize = 4096;

/// What a finished extraction left behind.
#[derive(Debug)]
pub struct Report {
    /// The folder that now holds the members.
    pub folder: PathBuf,
    /// How many members were written, directories included.
    pub written: usize,
    /// Uncompressed bytes the extracted members hold.
    pub bytes: u64,
    /// Members that were left out, each already worded for the banner. A device
    /// node inside a tar is the usual reason, and a folder that arrived missing one
    /// member the user could see has to say so.
    pub skipped: Vec<String>,
}

impl Report {
    /// One line saying what happened, for the banner. The size is the uncompressed
    /// one, because that is the number which explains why a small container took a
    /// while to unpack.
    #[must_use]
    pub fn summary(&self) -> String {
        let items = if self.written == 1 {
            "1 item".to_string()
        } else {
            format!("{} items", self.written)
        };
        format!(
            "Extracted {items} ({}) into {}",
            human_size(self.bytes),
            self.folder.display()
        )
    }
}

/// One word from an extraction about a member it has just written: how many are
/// down, how many the walk intends to write, and how many bytes have landed.
///
/// The count is a count of members rather than of bytes because that is what an
/// extraction can know before it starts: a container's table of contents lists every
/// member it holds, so the whole is a number of rows, and the bytes only add up as
/// they are decompressed. A caller wanting a byte total would have to decompress the
/// container to find one.
pub type Progress<'a> = &'a mut dyn FnMut(usize, usize, u64);

/// Extract what `location` names into a new folder `into`.
///
/// The caller picks the name, which is its job: only the view knows what is
/// already in the destination folder, and a name the user did not choose should not
/// overwrite something they did. `location` may name the whole container, one
/// folder inside it, or one file.
///
/// The container is opened here rather than borrowed from the reader the view
/// browses with, because that reader sits behind the type-erased set the views
/// share and there is no way back to its type. The cost is one table-of-contents
/// scan: a pass over the index with nothing decompressed, which is noise beside the
/// write it is about to make.
///
/// `progress`, when given, is called once per member written. It goes un-called on
/// the fast path: handing the container to `unzip` or `tar` is what makes extracting
/// a 4 GiB file cheap, and a child process that reports nothing is the price of
/// that. A caller showing a bar therefore has to be able to say "Extracting" without
/// a percentage, which is why the callback is offered rather than required.
pub fn extract(
    location: &Location,
    into: &Path,
    cancel: &Cancel,
    progress: Option<Progress<'_>>,
) -> Result<Report> {
    let container = location
        .archive_path()
        .ok_or_else(not_an_archive(location))?;
    let inner = location
        .archive_inner()
        .ok_or_else(not_an_archive(location))?;
    let opened = ArchiveReader::new().contents(container)?;
    let at = opened.toc().find(inner).ok_or_else(|| {
        Error::Archive(format!(
            "{}: {} is not in the archive",
            container.display(),
            if inner.is_empty() { "/" } else { inner }
        ))
    })?;
    // Only a folder this call created is its to remove: extracting over a folder
    // the user pointed at leaves their own files alone when the extract fails.
    let fresh = into.symlink_metadata().is_err();
    fs::create_dir_all(into)?;
    let mut job = Job {
        opened: &opened,
        toc: opened.toc(),
        at,
        into,
        cancel,
        written: 0,
        bytes: 0,
        skipped: Vec::new(),
        dirs: Vec::new(),
        whole: 0,
        progress,
    };
    match job.run() {
        Ok(()) => Ok(job.report(into)),
        Err(error) => {
            // Half a tree reads as a success that lost some files, and the user
            // cannot tell which — so a folder we made and did not finish goes away.
            if fresh {
                let _ = fs::remove_dir_all(into);
            }
            Err(error)
        }
    }
}

fn not_an_archive(location: &Location) -> impl FnOnce() -> Error {
    let text = location.to_string();
    move || Error::InvalidLocation(format!("{text}: not inside an archive"))
}

/// One extraction in progress: what is being read, where it is going, and what has
/// been written so far.
///
/// Two lifetimes rather than one, because the two borrows have different lengths
/// and neither can be stretched to match the other: the container belongs to this
/// module's own local, while the callback belongs to the caller and outlives the
/// call. A single lifetime would demand that the shorter one be the longer.
struct Job<'a, 'p> {
    opened: &'a Opened,
    toc: &'a Toc,
    /// The member being extracted: the root for a whole container.
    at: usize,
    into: &'a Path,
    cancel: &'a Cancel,
    written: usize,
    bytes: u64,
    skipped: Vec<String>,
    /// Folders still waiting for their mode and timestamps. Directories are sealed
    /// after their contents, because a folder written down at `0500` cannot have
    /// anything created inside it.
    dirs: Vec<(PathBuf, Option<u32>, Option<SystemTime>)>,
    /// How many members the walk ahead intends to write, so a report can say how
    /// far along it is. Zero until the walk is known.
    whole: usize,
    progress: Option<Progress<'p>>,
}

impl Job<'_, '_> {
    fn run(&mut self) -> Result<()> {
        if self.at == Toc::ROOT && self.whole_archive()? {
            self.written = self.toc.len().saturating_sub(1);
            self.bytes = self.opened.total_bytes();
            return Ok(());
        }
        // Parents before children, which is what lets a file be written into a
        // folder that appears earlier in the same walk. Collected up front rather
        // than iterated lazily, which is also what makes the total a caller can be
        // told: the walk's length is knowable before the first member is written.
        let walk = self.toc.subtree(self.at);
        self.whole = walk.iter().filter(|index| **index != Toc::ROOT).count();
        for index in walk {
            if index == Toc::ROOT {
                continue;
            }
            self.cancel.ensure_active()?;
            self.materialise(index)?;
        }
        self.seal_dirs();
        Ok(())
    }

    fn report(&self, into: &Path) -> Report {
        Report {
            folder: into.to_path_buf(),
            written: self.written,
            bytes: self.bytes,
            skipped: self.skipped.clone(),
        }
    }

    /// The whole container in one helper process, when there is a tool for its
    /// format and it is installed. `false` means nothing ran, or the helper gave
    /// up, and the member walk has to do the work instead.
    fn whole_archive(&self) -> Result<bool> {
        let format = self.opened.format();
        let Some(program) = helper_program(format) else {
            return Ok(false);
        };
        let Some(path) = self.opened.path().to_str() else {
            return Ok(false);
        };
        if !command::available(program) {
            return Ok(false);
        }
        self.cancel.ensure_active()?;
        let mut command = helper_command(format, Path::new(path), self.into);
        // A helper that fails is not a failure to report: the member walk reads the
        // same container through this crate's own parser, so the message the user
        // ends up seeing describes the archive rather than the tool that gave up on
        // it. Re-checking cancellation after it matters — a helper that ran for a
        // minute before failing had a minute in which to be cancelled.
        let ok = command::status_ok(&mut command, self.opened.path(), program).is_ok();
        self.cancel.ensure_active()?;
        Ok(ok)
    }

    fn materialise(&mut self, index: usize) -> Result<()> {
        // Name, kind, mode and date are copied out rather than borrowed: writing a
        // member needs `&mut self`, and a `&Node` into the table of contents would
        // still be alive at that point.
        let Some(node) = self.toc.node(index) else {
            return Ok(());
        };
        let name = node.name.clone();
        let (kind, mode, modified) = (node.kind, node.mode, node.modified);
        let Some(target) = destination(self.into, &self.toc.path_of(index)) else {
            self.skipped
                .push(format!("{name}: cannot be placed in the folder."));
            return Ok(());
        };
        match kind {
            NodeKind::Dir => {
                fs::create_dir_all(&target)?;
                self.written += 1;
                self.dirs.push((target, mode, modified));
            }
            NodeKind::File => self.write_file(index, &target, &name, mode, modified)?,
            NodeKind::Symlink => self.write_link(index, &target, &name)?,
            // A device or a FIFO has no bytes to write; creating one would need the
            // major and minor numbers, which no reader here keeps.
            NodeKind::Special => self
                .skipped
                .push(format!("{name}: not a file or a folder.")),
        }
        // Said after the member landed rather than before, so a caller counting
        // reports never counts one for a member that is not there. A skipped member
        // reports too: it is a member the walk is finished with either way, and a
        // bar that never reaches its own total looks like an extract that stalled.
        let tally = (self.written, self.whole, self.bytes);
        if let Some(progress) = self.progress.as_mut() {
            progress(tally.0, tally.1, tally.2);
        }
        Ok(())
    }

    /// One member, through a temporary name and a rename.
    fn write_file(
        &mut self,
        index: usize,
        target: &Path,
        name: &str,
        mode: Option<u32>,
        modified: Option<SystemTime>,
    ) -> Result<()> {
        let part = part_path(target);
        replace(&part);
        replace(target);
        let written = match self.pump(index, &part, mode, modified) {
            Ok(written) => written,
            Err(error) => {
                let _ = fs::remove_file(&part);
                return Err(self.describe(name, error));
            }
        };
        fs::rename(&part, target).map_err(|error| self.describe(name, error.into()))?;
        self.written += 1;
        self.bytes += written;
        Ok(())
    }

    /// Copy one member into `part` and stamp it with its mode and date. Returns the
    /// byte count so the caller can add it to the total.
    fn pump(
        &mut self,
        index: usize,
        part: &Path,
        mode: Option<u32>,
        modified: Option<SystemTime>,
    ) -> Result<u64> {
        let cancel = self.cancel.clone();
        let mut file = File::create(part)?;
        let mut written = 0_u64;
        self.opened.read_member(index, &mut |chunk| {
            // Checked per chunk rather than per member: a two-gigabyte member
            // is eight thousand chunks, and a cancel that waited for the end of
            // one would feel like a window that had stopped answering.
            cancel.ensure_active()?;
            file.write_all(chunk)?;
            written += chunk.len() as u64;
            Ok(())
        })?;
        file.flush()?;
        // Both restorations are allowed to fail: an extracted file that lost its
        // timestamp is still the file that was asked for, and claiming otherwise
        // would invite a retry that ends up with two.
        apply(&file, mode, modified);
        Ok(written)
    }

    /// One symlink. Its bytes *are* the target — `tar` stores a link that way, and
    /// so does `zip` — so the member is read rather than copied.
    fn write_link(&mut self, index: usize, target: &Path, name: &str) -> Result<()> {
        let cancel = self.cancel.clone();
        let archive = self.opened.path().display().to_string();
        let mut body: Vec<u8> = Vec::new();
        let read = self.opened.read_member(index, &mut |chunk| {
            cancel.ensure_active()?;
            if body.len() + chunk.len() > MAX_LINK {
                return Err(Error::Archive(format!(
                    "{archive}: {name} has no usable link target"
                )));
            }
            body.extend_from_slice(chunk);
            Ok(())
        });
        read.map_err(|error| self.describe(name, error))?;
        let text = String::from_utf8_lossy(&body).into_owned();
        if text.is_empty() {
            self.skipped
                .push(format!("{name}: the link points nowhere."));
            return Ok(());
        }
        // Stored as written, absolute targets included, which is what `tar` and
        // `unzip` do. A link out of the folder is the archive saying so, not
        // something for this call to second-guess.
        replace(target);
        symlink(&text, target).map_err(|error| self.describe(name, error.into()))?;
        self.written += 1;
        Ok(())
    }

    /// Give every folder its recorded date and permissions, innermost first, so a
    /// parent sealed at `0500` still had its contents written into it.
    fn seal_dirs(&mut self) {
        for (path, mode, modified) in self.dirs.iter().rev() {
            let Ok(folder) = File::open(path) else {
                continue;
            };
            apply(&folder, *mode, *modified);
        }
    }

    /// Say which member a failure happened on. The readers report the container,
    /// which the user already knows; the member is the part that says what to fix.
    fn describe(&self, name: &str, error: Error) -> Error {
        if error.is_cancelled() {
            return Error::Cancelled;
        }
        let message = error.to_string();
        Error::Archive(format!(
            "{}: {name}: {}",
            self.opened.path().display(),
            message.trim_start_matches("archive error: "),
        ))
    }
}

/// Mode and date for a file or folder that has just been written.
fn apply(file: &File, mode: Option<u32>, modified: Option<SystemTime>) {
    if let Some(at) = modified {
        let _ = file.set_times(fs::FileTimes::new().set_modified(at));
    }
    // Only the permission bits: the type bits a mode carries are already decided by
    // what was created, and a setuid bit from another machine is not worth applying
    // to a folder the user just unpacked.
    if let Some(bits) = mode {
        let _ = file.set_permissions(fs::Permissions::from_mode(bits & 0o7777));
    }
}

/// `into` plus a member's inner path, or `None` when the name would land outside
/// `into`. The table of contents already rejects `..`, NULs and absolute names, so
/// this is a second opinion rather than the only check.
fn destination(into: &Path, relative: &str) -> Option<PathBuf> {
    let mut path = into.to_path_buf();
    for component in relative.split('/') {
        match component {
            "" | "." => {}
            ".." => return None,
            name => path.push(name),
        }
    }
    (path != *into).then_some(path)
}

/// Where a member is written before it is renamed into place. Dotted so a listing
/// of the destination taken mid-extract does not show it, and derived from the
/// final name so two members never share one.
fn part_path(target: &Path) -> PathBuf {
    let name = target.file_name().map_or_else(
        || "member".to_string(),
        |name| name.to_string_lossy().into_owned(),
    );
    target.with_file_name(format!(".{name}.part"))
}

/// Remove whatever already sits at `path`. Extraction writes into a folder the
/// caller picked to be free, so this only ever clears what an earlier attempt left.
fn replace(path: &Path) {
    let Ok(meta) = path.symlink_metadata() else {
        return;
    };
    if meta.is_dir() {
        let _ = fs::remove_dir(path);
    } else {
        let _ = fs::remove_file(path);
    }
}

/// The tool that can unpack a whole container of this format, or `None` for the
/// formats with no such thing: a `.gz` holding one file has to be inflated by the
/// reader that is already open.
const fn helper_program(format: Format) -> Option<&'static str> {
    match format {
        Format::Zip => Some("unzip"),
        Format::Tar(_) => Some("tar"),
        Format::Sevenz => Some("7z"),
        Format::Bare(_) => None,
    }
}

/// That tool, told to unpack `archive` into `into` and say as little as possible
/// while doing it.
fn helper_command(format: Format, archive: &Path, into: &Path) -> Command {
    let mut command = command::command(helper_program(format).unwrap_or("tar"));
    match format {
        // `-o` overwrites without asking, `-qq` is silent even about a warning, and
        // `--` keeps an archive named `-x` from being read as a flag.
        Format::Zip => {
            command
                .args(["-qq", "-o", "-d"])
                .arg(into)
                .arg("--")
                .arg(archive);
        }
        // No `-z`/`-j`/`-J`: `tar` picks the decompressor from the bytes, which is
        // both what a user typing the command would get and the only answer that
        // works for a `.tar.zst` on a build of `tar` without `--zstd`.
        Format::Tar(_) => {
            command.arg("-xf").arg(archive).arg("-C").arg(into);
        }
        Format::Sevenz => {
            // `-o` takes a path relative to the working directory, so the working
            // directory is made the destination rather than joined by hand.
            command
                .args(["x", "-y", "-bso0", "-bsp0"])
                .arg(format!("-o{}", into.display()))
                .arg("--")
                .arg(archive)
                .current_dir(into);
        }
        Format::Bare(_) => {}
    }
    command
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::Codec;
    use std::ffi::OsStr;
    use std::time::{Duration, UNIX_EPOCH};

    #[test]
    fn a_member_path_lands_inside_the_destination() {
        assert_eq!(
            destination(Path::new("/d"), "a/b.txt"),
            Some(PathBuf::from("/d/a/b.txt"))
        );
        // A leading "./" is what GNU tar writes; it changes nothing.
        assert_eq!(
            destination(Path::new("/d"), "./a/b.txt"),
            Some(PathBuf::from("/d/a/b.txt"))
        );
        assert_eq!(destination(Path::new("/d"), ""), None);
        assert_eq!(destination(Path::new("/d"), "./"), None);
    }

    #[test]
    fn a_path_that_would_escape_the_destination_is_refused() {
        assert_eq!(destination(Path::new("/d"), "../out"), None);
        assert_eq!(destination(Path::new("/d"), "a/../../out"), None);
        assert_eq!(destination(Path::new("/d"), "a/.."), None, "back to itself");
    }

    #[test]
    fn a_part_name_is_hidden_and_specific_to_its_member() {
        assert_eq!(
            part_path(Path::new("/d/a.txt")),
            PathBuf::from("/d/.a.txt.part")
        );
        // A path with no final component still gets a name rather than a panic.
        assert_eq!(part_path(Path::new("/")), PathBuf::from("/.member.part"));
    }

    #[test]
    fn only_a_format_with_a_helper_gets_one() {
        assert_eq!(helper_program(Format::Zip), Some("unzip"));
        assert_eq!(helper_program(Format::Tar(Codec::Gzip)), Some("tar"));
        assert_eq!(helper_program(Format::Sevenz), Some("7z"));
        // A `.gz` holding one file has no tool that unpacks it into a folder.
        assert_eq!(helper_program(Format::Bare(Codec::Gzip)), None);
    }

    #[test]
    fn the_zip_helper_is_told_to_overwrite_and_stay_quiet() {
        let command = helper_command(Format::Zip, Path::new("/t/a.zip"), Path::new("/t/out"));
        let args = args_of(&command);
        for flag in ["-qq", "-o"] {
            assert!(args.iter().any(|a| a == flag), "{flag} missing: {args:?}");
        }
        assert!(args.ends_with(&["--".to_string(), "/t/a.zip".to_string()]));
        // The destination is an argument of `-d`, never the working directory,
        // because `unzip` resolves a relative `-d` against the caller's own.
        assert!(args.windows(2).any(|w| w == ["-d", "/t/out"]));
    }

    #[test]
    fn the_tar_helper_picks_its_own_decompressor() {
        let command = helper_command(
            Format::Tar(Codec::Xz),
            Path::new("/t/a.tar.xz"),
            Path::new("/t/out"),
        );
        assert_eq!(args_of(&command), ["-xf", "/t/a.tar.xz", "-C", "/t/out"]);
    }

    #[test]
    fn the_7z_helper_writes_relative_to_the_destination() {
        let command = helper_command(Format::Sevenz, Path::new("/t/a.7z"), Path::new("/t/out"));
        let args = args_of(&command);
        assert!(
            args.iter().any(|a| a == "-o/t/out"),
            "the destination is passed as -o: {args:?}"
        );
        // `-o` resolves against the working directory, so it has to be the
        // destination for the option to mean what it says.
        assert_eq!(command.get_current_dir(), Some(Path::new("/t/out")));
    }

    #[test]
    fn every_helper_runs_without_a_translating_locale() {
        for format in [
            Format::Zip,
            Format::Tar(Codec::None),
            Format::Sevenz,
            Format::Bare(Codec::Gzip),
        ] {
            let command = helper_command(format, Path::new("/t/a"), Path::new("/t/out"));
            assert!(
                command
                    .get_envs()
                    .any(|(key, value)| key == "LC_ALL" && value == Some(OsStr::new("C"))),
                "{format:?} runs with a translating locale"
            );
        }
    }

    #[test]
    fn replace_clears_whatever_is_already_there() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let file = dir.path().join("file");
        let folder = dir.path().join("folder");
        let link = dir.path().join("link");
        fs::write(&file, b"x").expect("write");
        fs::create_dir(&folder).expect("mkdir");
        symlink("target", &link).expect("symlink");

        replace(&file);
        replace(&folder);
        replace(&link);
        replace(&dir.path().join("absent"));

        assert!(!file.exists() && !folder.exists() && !link.exists());
    }

    #[test]
    fn a_written_member_keeps_its_permissions_and_date() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let path = dir.path().join("note.txt");
        fs::write(&path, b"body").expect("write");
        let at = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        {
            let file = File::open(&path).expect("open");
            // The type bits a stored mode carries are dropped, the setuid bit is
            // not: what decides the kind of file is what was created here.
            apply(&file, Some(0o104_755), Some(at));
        }
        let meta = fs::symlink_metadata(&path).expect("stat");
        assert_eq!(meta.permissions().mode() & 0o7777, 0o4755);
        assert_eq!(
            meta.modified()
                .expect("dated")
                .duration_since(UNIX_EPOCH)
                .expect("after epoch")
                .as_secs(),
            1_700_000_000
        );
    }

    #[test]
    fn a_member_with_no_metadata_to_restore_is_left_as_written() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let path = dir.path().join("note.txt");
        fs::write(&path, b"body").expect("write");
        let before = fs::symlink_metadata(&path).expect("stat").permissions();
        apply(&File::open(&path).expect("open"), None, None);
        assert_eq!(
            fs::symlink_metadata(&path)
                .expect("stat")
                .permissions()
                .mode(),
            before.mode()
        );
    }

    #[test]
    fn a_summary_says_how_much_arrived_where() {
        let report = Report {
            folder: PathBuf::from("/home/user/unpacked"),
            written: 1,
            bytes: 12,
            skipped: Vec::new(),
        };
        assert_eq!(
            report.summary(),
            "Extracted 1 item (12 B) into /home/user/unpacked"
        );
        let report = Report {
            folder: PathBuf::from("/tmp/o"),
            written: 3,
            bytes: 2048,
            skipped: Vec::new(),
        };
        assert_eq!(report.summary(), "Extracted 3 items (2.0 kB) into /tmp/o");
    }

    #[test]
    fn a_location_outside_an_archive_names_itself() {
        let location = Location::file("/tmp/notes.txt");
        let message = not_an_archive(&location)().to_string();
        assert!(message.contains("not inside an archive"), "{message}");
        assert!(message.contains("notes.txt"), "{message}");
    }

    fn args_of(command: &Command) -> Vec<String> {
        command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }
}
