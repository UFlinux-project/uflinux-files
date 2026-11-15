//! Writing a container's members out to a real folder, driven through the public
//! API.
//!
//! Browsing is tested in `fixtures.rs`; this file tests the one write the archive
//! code offers, and tests it on disk rather than against a mock, because the whole
//! claim of extraction is that an ordinary folder arrives. What an ordinary folder
//! has to hold is the interesting part: not only each member's bytes but its
//! permissions, whether it was a link, and which parents had to be created to hold
//! it.
//!
//! Two paths exist inside [`extract`] and both are exercised here: the whole
//! container handed to `unzip`, `tar` or `7z`, and the member-by-member walk
//! through this crate's own readers. They have to agree, so most assertions below
//! go through [`assert_arrived`], which does not care which one ran. The walk is
//! forced by narrowing `$PATH` — a helper that is not installed is a helper that
//! cannot be used — which is also how the fallback is covered: a `.tar.gz` on a
//! machine without `tar` still has to unpack, because the reader here already knows
//! how to decompress it.
//!
//! Dates are only asserted where this crate did the writing. Busybox's `unzip`
//! leaves a member with the time it was extracted at, and GNU `tar` does the same
//! for directories, so a date checked after a helper ran says nothing about this
//! crate.
//!
//! `$PATH` is process-wide and these tests rewrite it, so the ones that do and the
//! ones that merely need a codec installed share one lock. Without it a compressed
//! fixture could be opened while a sibling test had narrowed the environment and
//! fail for a reason that has nothing to do with the code under test.

use std::collections::BTreeSet;
use std::io::Write as _;
use std::os::unix::fs::{PermissionsExt as _, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;

use ufm_archive::{ArchiveReader, NodeKind, Report, extract};
use ufm_core::{Error, Location, SortKey};
use ufm_vfs::{Cancel, read_all};

/// Where the committed fixtures live, relative to this crate.
const FIXTURES: &str = "../../tests/archive-fixtures";

/// 2023-11-14T21:33:20Z, the date `tests/archive-fixtures/make.sh` stamps every
/// member with.
const AT: u64 = 1_700_000_000;

/// Guards `$PATH`, which several tests rewrite. See the module comment.
static PATH_LOCK: Mutex<()> = Mutex::new(());

/// One member of a container built by these tests.
struct Member {
    name: &'static str,
    kind: NodeKind,
    body: &'static str,
    mode: u32,
    /// Store a member's bytes as they are, so a test can find and damage them.
    stored: bool,
}

impl Member {
    fn file(name: &'static str, body: &'static str) -> Self {
        Self {
            name,
            kind: NodeKind::File,
            body,
            mode: 0o100_644,
            stored: false,
        }
    }

    fn stored(mut self) -> Self {
        self.stored = true;
        self
    }

    fn dir(name: &'static str) -> Self {
        Self {
            name,
            kind: NodeKind::Dir,
            body: "",
            mode: 0o040_755,
            stored: true,
        }
    }

    /// `target` is the body: both formats store a link's target as its content.
    fn link(name: &'static str, target: &'static str) -> Self {
        Self {
            name,
            kind: NodeKind::Symlink,
            body: target,
            mode: 0o120_777,
            stored: true,
        }
    }

    fn special(name: &'static str) -> Self {
        Self {
            name,
            kind: NodeKind::Special,
            body: "",
            mode: 0o020_666,
            stored: true,
        }
    }

    fn mode(mut self, bits: u32) -> Self {
        self.mode = bits;
        self
    }
}

/// The tree every hand-built container here holds: two folders, a name with
/// non-ASCII and a space, an executable, a dotfile and a link. The same shapes the
/// committed fixtures carry, small enough to assert member by member.
fn tree() -> Vec<Member> {
    vec![
        Member::dir("bin"),
        Member::file("bin/run.sh", "#!/bin/sh\necho run\n").mode(0o100_755),
        Member::dir("docs"),
        Member::file("docs/readme.md", "# Readme\n"),
        Member::file("docs/prázdninový seznam!.txt", "Názvy s diakritikou.\n"),
        Member::file(".hidden", "hidden\n"),
        Member::file("notes.txt", "line one\nline two\n"),
        Member::link("latest.txt", "notes.txt"),
    ]
}

/// The paths `tree()` has to leave behind, parents included.
const ARRIVED: [&str; 8] = [
    ".hidden",
    "bin",
    "bin/run.sh",
    "docs",
    "docs/prázdninový seznam!.txt",
    "docs/readme.md",
    "latest.txt",
    "notes.txt",
];

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join(FIXTURES)
        .canonicalize()
        .expect("fixtures committed")
}

fn fixture(name: &str) -> PathBuf {
    let path = fixtures().join(name);
    assert!(path.is_file(), "{} is committed", path.display());
    path
}

fn root(path: &Path) -> Location {
    Location::archive(path, &[] as &[&str]).expect("root location")
}

fn inside(path: &Path, inner: &[&str]) -> Location {
    Location::archive(path, inner).expect("inner location")
}

/// Extract and insist, because a failure here is always worth reading about.
fn unpack(location: &Location, into: &Path) -> Report {
    extract(location, into, &Cancel::new(), None)
        .unwrap_or_else(|error| panic!("{location} -> {}: {error}", into.display()))
}

/// Whether the `7z` CLI is here, the same guard `fixtures.rs` uses.
fn sevenz_available() -> bool {
    Command::new("7z")
        .arg("i")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// Whether `program` is installed, so a test that needs a codec can skip rather
/// than fail on a machine that has no `gzip`.
fn installed(program: &str) -> bool {
    where_(program).is_some()
}

fn where_(program: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join(program))
        .find(|candidate| candidate.is_file())
}

/// Everything under `folder`, relative, `/`-separated and sorted. Symlinks are
/// listed as themselves rather than walked into, so a link cannot make a folder
/// appear twice.
fn paths(folder: &Path) -> BTreeSet<String> {
    fn walk(folder: &Path, at: &Path, out: &mut BTreeSet<String>) {
        for child in std::fs::read_dir(at)
            .unwrap_or_else(|e| panic!("{} readable: {e}", at.display()))
            .map(|entry| entry.expect("entry readable").path())
        {
            let name = child
                .strip_prefix(folder)
                .expect("inside the folder")
                .to_string_lossy()
                .into_owned();
            out.insert(name);
            if child.symlink_metadata().is_ok_and(|m| m.is_dir()) {
                walk(folder, &child, out);
            }
        }
    }
    let mut out = BTreeSet::new();
    walk(folder, folder, &mut out);
    out
}

/// Uncompressed bytes `members` claim to hold, which is what a report has to count.
fn body_bytes(members: &[Member]) -> u64 {
    members
        .iter()
        .filter(|member| member.kind == NodeKind::File)
        .map(|member| member.body.len() as u64)
        .sum()
}

/// Whether a path under the destination is one of the temporary files extraction
/// writes before renaming. The suffix is this crate's own, so an exact comparison
/// is what is wanted rather than a case-insensitive one.
fn is_part(relative: &str) -> bool {
    Path::new(relative)
        .extension()
        .is_some_and(|ext| ext == "part")
}

/// Assert that `tree()` arrived whole, whichever path inside [`extract`] wrote it.
fn assert_arrived(folder: &Path) {
    assert_eq!(
        paths(folder),
        ARRIVED.iter().map(|name| (*name).to_string()).collect(),
        "every member, and no temporary file"
    );
    assert_eq!(
        std::fs::read(folder.join("notes.txt")).expect("read"),
        b"line one\nline two\n",
        "a member's bytes survive"
    );
    assert_eq!(
        std::fs::read(folder.join("docs/prázdninový seznam!.txt")).expect("read"),
        "Názvy s diakritikou.\n".as_bytes(),
        "a name with a space, a `!` and non-ASCII survives"
    );
    assert_eq!(
        mode_of(folder, "bin/run.sh"),
        0o755,
        "an executable stays executable"
    );
    assert!(
        std::fs::read_link(folder.join("latest.txt"))
            .is_ok_and(|target| target == Path::new("notes.txt")),
        "the link points where the container said"
    );
}

fn mode_of(folder: &Path, relative: &str) -> u32 {
    std::fs::symlink_metadata(folder.join(relative))
        .expect("stat")
        .permissions()
        .mode()
        & 0o7777
}

fn modified_of(folder: &Path, relative: &str) -> Option<u64> {
    std::fs::symlink_metadata(folder.join(relative))
        .expect("stat")
        .modified()
        .ok()
        .and_then(|at| at.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
}

/// Run a test with `$PATH` holding only `tools`, and put it back afterwards even
/// if the test panics. An empty list means no archive helper is installed at all,
/// which is how the member-by-member path is forced.
///
/// The tools are linked rather than the whole `$PATH` dropped, because a test that
/// removes `tar` still needs `ln`, `sh` and the rest of coreutils around to set the
/// scene up.
struct OnlyPath {
    dir: tempfile::TempDir,
    previous: Option<std::ffi::OsString>,
}

impl OnlyPath {
    fn new(tools: &[&str]) -> Self {
        let dir = tempfile::TempDir::new().expect("bin dir");
        let previous = std::env::var_os("PATH");
        let wanted: BTreeSet<&str> = tools.iter().copied().chain(STANDARDS).collect();
        for tool in wanted {
            let found = where_(tool).unwrap_or_else(|| panic!("{tool} is installed"));
            symlink(&found, dir.path().join(tool)).expect("linked into the private PATH");
        }
        // Setting it is unsafe because the rest of the process sees the change;
        // `PATH_LOCK` is what makes that true one test at a time.
        unsafe { std::env::set_var("PATH", dir.path()) };
        Self { dir, previous }
    }

    /// Add a fake helper of this name, hiding any real one.
    fn fake(&self, program: &str, script: &str) {
        let path = self.dir.path().join(program);
        let _ = std::fs::remove_file(&path);
        std::fs::write(&path, script).expect("fake helper written");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .expect("fake helper made runnable");
    }
}

impl Drop for OnlyPath {
    fn drop(&mut self) {
        match &self.previous {
            Some(previous) => unsafe { std::env::set_var("PATH", previous) },
            None => unsafe { std::env::remove_var("PATH") },
        }
    }
}

/// The programs a test still needs once `$PATH` is narrowed: to build a container,
/// to make a folder, and to run a `#!/bin/sh` script.
const STANDARDS: [&str; 6] = ["sh", "ln", "mkdir", "rm", "cat", "gzip"];

/// Take the lock that guards `$PATH`, keeping the poison from a sibling test from
/// turning one failure into a cascade.
fn tools_locked() -> std::sync::MutexGuard<'static, ()> {
    PATH_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Write a zip with the `zip` crate, so no `zip` binary is needed to make one.
fn as_zip(path: &Path, members: &[Member]) {
    let mut writer = zip::ZipWriter::new(
        std::fs::File::create(path).unwrap_or_else(|e| panic!("{}: {e}", path.display())),
    );
    let stamp = zip::DateTime::from_date_and_time(2023, 11, 14, 22, 13, 20).expect("in range");
    for member in members {
        let options = zip::write::SimpleFileOptions::default()
            .last_modified_time(stamp)
            .unix_permissions(member.mode & 0o7777);
        match member.kind {
            // The writer stores directories and links itself: a directory has no
            // bytes, and a link's target is worth less than the overhead of
            // compressing it.
            NodeKind::Dir => writer
                .add_directory(member.name, options)
                .expect("directory written"),
            NodeKind::Symlink => writer
                .add_symlink(member.name, member.body, options)
                .expect("link written"),
            _ => {
                let method = if member.stored {
                    zip::CompressionMethod::Stored
                } else {
                    zip::CompressionMethod::Deflated
                };
                let options = options.compression_method(method);
                writer
                    .start_file(member.name, options)
                    .expect("file started");
                writer
                    .write_all(member.body.as_bytes())
                    .expect("body written");
            }
        }
    }
    writer.finish().expect("container finished");
}

/// Write a plain tar by hand, the way `tar.rs`'s own tests do, so a container
/// exists without a `tar` binary and a device node can be included.
fn as_tar(path: &Path, members: &[Member]) {
    let mut bytes = Vec::new();
    for member in members {
        let flag = match member.kind {
            NodeKind::Dir => b'5',
            NodeKind::Symlink => b'2',
            NodeKind::Special => b'3',
            NodeKind::File => b'0',
        };
        let body = member.body.as_bytes();
        let mut block = [0u8; 512];
        field(&mut block, 0, member.name.as_bytes());
        field(
            &mut block,
            100,
            format!("{:07o}", member.mode & 0o7777).as_bytes(),
        );
        field(&mut block, 108, b"0000000");
        field(&mut block, 116, b"0000000");
        field(&mut block, 124, format!("{:011o}", body.len()).as_bytes());
        field(&mut block, 136, format!("{AT:011o}").as_bytes());
        block[156] = flag;
        field(&mut block, 257, b"ustar\0");
        field(&mut block, 263, b"00");
        bytes.extend_from_slice(&block);
        bytes.extend_from_slice(body);
        bytes.resize(bytes.len().div_ceil(512) * 512, 0);
    }
    bytes.extend_from_slice(&[0; 1024]);
    std::fs::write(path, bytes).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
}

fn field(block: &mut [u8], at: usize, value: &[u8]) {
    let len = value.len().min(512 - at);
    block[at..at + len].copy_from_slice(&value[..len]);
}

/// Wrap a file in gzip. A hand-written deflate stream would test this test rather
/// than the codec, so the real compressor is used and a machine without it skips.
fn gzip_file(source: &Path, target: &Path) {
    let packed = Command::new("gzip")
        .args(["-c", "--"])
        .arg(source)
        .output()
        .expect("gzip runs");
    assert!(packed.status.success(), "gzip failed");
    std::fs::write(target, packed.stdout).expect("written");
}

#[test]
fn a_whole_zip_arrives_in_the_destination() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let container = dir.path().join("tree.zip");
    as_zip(&container, &tree());
    let into = dir.path().join("out");

    let report = unpack(&root(&container), &into);
    assert_eq!(report.folder, into, "the report names the destination");
    assert_arrived(&into);
}

#[test]
fn a_whole_plain_tar_arrives_in_the_destination() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let container = dir.path().join("tree.tar");
    as_tar(&container, &tree());
    let into = dir.path().join("out");

    unpack(&root(&container), &into);
    assert_arrived(&into);
}

#[test]
fn a_container_unpacks_member_by_member_when_no_helper_is_installed() {
    let _lock = tools_locked();
    let dir = tempfile::TempDir::new().expect("temp dir");
    let container = dir.path().join("tree.zip");
    as_zip(&container, &tree());
    let into = dir.path().join("out");

    // No `unzip`, so the fast path is unavailable rather than merely unused.
    let _path = OnlyPath::new(&[]);
    unpack(&root(&container), &into);
    assert_arrived(&into);
    assert_eq!(
        modified_of(&into, "notes.txt"),
        Some(AT),
        "the member walk restores the recorded date, which a helper is not asked to"
    );
    assert_eq!(
        modified_of(&into, "bin"),
        Some(AT),
        "and does it for the folders too, innermost first"
    );
}

#[test]
fn a_tar_unpacks_through_the_reader_when_tar_is_missing() {
    let _lock = tools_locked();
    if !installed("gzip") {
        return;
    }
    let dir = tempfile::TempDir::new().expect("temp dir");
    let plain = dir.path().join("tree.tar");
    as_tar(&plain, &tree());
    let container = dir.path().join("tree.tar.gz");
    gzip_file(&plain, &container);
    let into = dir.path().join("out");

    // `gzip` stays reachable — the reader needs it to open the container at all —
    // and `tar` goes, which is the case the fallback exists for.
    let _path = OnlyPath::new(&["gzip"]);
    unpack(&root(&container), &into);
    assert_arrived(&into);
}

#[test]
fn a_helper_that_gives_up_does_not_stop_the_extraction() {
    let _lock = tools_locked();
    let dir = tempfile::TempDir::new().expect("temp dir");
    let container = dir.path().join("tree.zip");
    as_zip(&container, &tree());
    let into = dir.path().join("out");
    let path = OnlyPath::new(&[]);
    path.fake("unzip", "#!/bin/sh\nexit 1\n");

    unpack(&root(&container), &into);
    assert_arrived(&into);
}

#[test]
fn a_helper_is_only_asked_once_per_container() {
    let _lock = tools_locked();
    let dir = tempfile::TempDir::new().expect("temp dir");
    let container = dir.path().join("tree.zip");
    as_zip(&container, &tree());
    let counter = dir.path().join("calls");
    let path = OnlyPath::new(&[]);
    path.fake(
        "unzip",
        &format!("#!/bin/sh\necho x >> '{}'\nexit 1\n", counter.display()),
    );

    unpack(&root(&container), &dir.path().join("out"));
    let calls = std::fs::read_to_string(&counter).unwrap_or_default();
    assert_eq!(
        calls.lines().count(),
        1,
        "a helper that failed is not asked again for the same container"
    );
}

#[test]
fn a_compressed_tar_arrives_in_the_destination() {
    let _lock = tools_locked();
    if !installed("gzip") {
        return;
    }
    let dir = tempfile::TempDir::new().expect("temp dir");
    let plain = dir.path().join("tree.tar");
    as_tar(&plain, &tree());
    let container = dir.path().join("tree.tar.gz");
    gzip_file(&plain, &container);

    unpack(&root(&container), &dir.path().join("out"));
    assert_arrived(&dir.path().join("out"));
}

#[test]
fn a_single_compressed_file_becomes_one_named_member() {
    let _lock = tools_locked();
    if !installed("gzip") {
        return;
    }
    let dir = tempfile::TempDir::new().expect("temp dir");
    let plain = dir.path().join("notes.txt");
    std::fs::write(&plain, b"line one\nline two\n").expect("written");
    let container = dir.path().join("notes.txt.gz");
    gzip_file(&plain, &container);

    let report = unpack(&root(&container), &dir.path().join("out"));
    // The member is the file without its suffix, which is what the listing offers
    // and what a user typing `gunzip -c` would expect to find.
    assert_eq!(
        paths(&dir.path().join("out")),
        BTreeSet::from(["notes.txt".to_string()])
    );
    assert_eq!(report.written, 1);
    assert_eq!(
        report.bytes, 18,
        "what arrived is counted, not what the index claimed about the compressed size"
    );
}

#[test]
fn one_folder_inside_a_container_unpacks_on_its_own() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let container = dir.path().join("tree.zip");
    let members = tree();
    as_zip(&container, &members);
    let into = dir.path().join("out");

    let report = unpack(&inside(&container, &["docs"]), &into);
    assert_eq!(
        paths(&into),
        ["docs", "docs/prázdninový seznam!.txt", "docs/readme.md"]
            .iter()
            .map(|name| (*name).to_string())
            .collect(),
        "the selected folder arrives with its members inside it"
    );
    assert_eq!(
        report.bytes,
        members
            .iter()
            .filter(|member| member.name.starts_with("docs/"))
            .map(|member| member.body.len() as u64)
            .sum::<u64>(),
        "the report counts what that folder holds"
    );
    assert!(
        !into.join("notes.txt").exists(),
        "a sibling of the selection is not written"
    );
}

#[test]
fn one_file_inside_a_container_unpacks_under_its_own_name() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let container = dir.path().join("tree.zip");
    as_zip(&container, &tree());
    let into = dir.path().join("out");

    let report = unpack(&inside(&container, &["notes.txt"]), &into);
    assert_eq!(
        paths(&into),
        BTreeSet::from(["notes.txt".to_string()]),
        "a single member is written as itself, not inside a folder"
    );
    assert_eq!(report.written, 1);
    assert!(
        report.summary().starts_with("Extracted 1 item ("),
        "{}",
        report.summary()
    );
}

#[test]
fn a_link_member_arrives_as_a_link_and_a_device_is_left_out() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let container = dir.path().join("tree.tar");
    let mut members = tree();
    members.push(Member::special("null"));
    as_tar(&container, &members);
    let into = dir.path().join("out");

    let report = unpack(&root(&container), &into);
    let link = into.join("latest.txt");
    let target = std::fs::read_link(&link).unwrap_or_else(|e| panic!("{link:?}: {e}"));
    assert_eq!(target, Path::new("notes.txt"), "stored as it was written");
    assert_eq!(mode_of(&into, "latest.txt"), 0o777);
    assert!(
        !into.join("null").exists(),
        "a device node has no bytes to write"
    );
    assert!(
        report
            .skipped
            .iter()
            .any(|line| line == "null: not a file or a folder."),
        "{:?}: the skip is reported rather than hidden",
        report.skipped
    );
    assert_eq!(report.written, ARRIVED.len(), "the device is not counted");
}

#[test]
fn a_parent_the_container_never_lists_arrives_as_a_folder() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let container = dir.path().join("deep.zip");
    as_zip(
        &container,
        &[Member::file("a/b/c/deep.txt", "x\n").mode(0o100_600)],
    );
    let into = dir.path().join("out");

    unpack(&root(&container), &into);
    assert_eq!(
        paths(&into),
        ["a", "a/b", "a/b/c", "a/b/c/deep.txt"]
            .iter()
            .map(|name| (*name).to_string())
            .collect(),
        "the folders a member's name implies are created for it"
    );
    assert_eq!(
        mode_of(&into, "a/b"),
        0o755,
        "a synthesised folder is usable"
    );
    assert_eq!(mode_of(&into, "a/b/c/deep.txt"), 0o600);
}

#[test]
fn the_report_counts_what_arrived_and_says_where() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let container = dir.path().join("tree.tar");
    let members = tree();
    as_tar(&container, &members);
    let into = dir.path().join("out");

    let report = unpack(&root(&container), &into);
    assert_eq!(
        report.written,
        ARRIVED.len(),
        "files and folders both count"
    );
    // The total is what landed on disk, which is the number that explains why a
    // small container took a while to unpack.
    let mut on_disk = 0u64;
    for name in &ARRIVED {
        let path = into.join(name);
        if path.is_symlink() || path.is_dir() {
            continue;
        }
        on_disk += std::fs::metadata(&path).expect("stat").len();
    }
    assert_eq!(report.bytes, on_disk, "the byte count is the one on disk");
    assert_eq!(
        report.bytes,
        body_bytes(&members),
        "which is what was stored"
    );
    assert!(
        report.summary().contains(&into.display().to_string()),
        "{}: the summary names the folder",
        report.summary()
    );
    assert!(report.skipped.is_empty(), "{:?}", report.skipped);
}

#[test]
fn every_committed_fixture_unpacks_into_the_folder_named_for_it() {
    let _lock = tools_locked();
    for name in [
        "sample.zip",
        "sample.tar.gz",
        "sample.tar.xz",
        "nested.tar",
        "sample.7z",
    ] {
        if name == "sample.7z" && !sevenz_available() {
            continue;
        }
        let path = fixture(name);
        let dir = tempfile::TempDir::new().expect("temp dir");
        let into = dir.path().join("out");
        let report = unpack(&root(&path), &into);
        assert!(report.written > 5, "{name}: {} members", report.written);
        assert!(
            into.join("notes.txt").is_file(),
            "{name}: the tree's own file is there"
        );
        assert!(
            into.join("docs/prázdninový seznam!.txt").is_file(),
            "{name}: the awkward name is there"
        );
        assert_eq!(mode_of(&into, "bin/run.sh"), 0o755, "{name}: mode survives");
        assert!(
            !paths(&into).iter().any(|name| is_part(name)),
            "{name}: no temporary file left behind"
        );
    }
}

#[test]
fn a_container_inside_a_container_arrives_as_a_file_that_can_be_opened() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let outer = fixture("nested.tar");
    let into = dir.path().join("out");

    // This is what `ufm_core::paths::extracted_dir()` exists for: the member
    // holding the inner container, written out so it becomes a container again.
    unpack(&inside(&outer, &["outer.zip"]), &into);
    let inner = into.join("outer.zip");
    let entries = read_all(
        &ArchiveReader::new(),
        &root(&inner),
        SortKey::default(),
        true,
        None,
    )
    .expect("the extracted member opens as a container");
    let names: BTreeSet<&str> = entries.iter().map(|entry| entry.name.as_str()).collect();
    assert_eq!(
        names,
        ["README", "deep"].iter().copied().collect(),
        "the second level is reachable once the first is on disk"
    );
}

#[test]
fn a_cancelled_extract_reports_cancelled_and_leaves_the_folder_it_made() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let container = dir.path().join("tree.zip");
    as_zip(&container, &tree());
    let into = dir.path().join("out");

    let cancel = Cancel::new();
    cancel.cancel();
    let error =
        extract(&root(&container), &into, &cancel, None).expect_err("a cancelled job fails");
    assert!(error.is_cancelled(), "{error}");
    assert!(
        !into.exists(),
        "a folder the extraction made does not survive it"
    );
}

#[test]
fn a_cancelled_extract_keeps_a_destination_the_caller_already_had() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let container = dir.path().join("tree.zip");
    as_zip(&container, &tree());
    let into = dir.path().join("out");
    std::fs::create_dir(&into).expect("destination made first");
    std::fs::write(into.join("mine.txt"), b"mine\n").expect("caller's own file");

    let cancel = Cancel::new();
    cancel.cancel();
    extract(&root(&container), &into, &cancel, None).expect_err("a cancelled job fails");
    assert!(
        into.join("mine.txt").is_file(),
        "a folder the caller pointed at is not this call's to remove"
    );
}

#[test]
fn a_member_that_fails_leaves_nothing_under_its_own_name() {
    let _lock = tools_locked();
    let dir = tempfile::TempDir::new().expect("temp dir");
    let container = dir.path().join("broken.zip");
    // Named so the whole member is walked before the damaged one: the listing is
    // sorted, and a failure that came first would prove nothing about what a
    // completed member leaves behind.
    as_zip(
        &container,
        &[
            Member::file("a-good.txt", "whole\n"),
            Member::file("z-bad.txt", "the member the container mis-states\n").stored(),
        ],
    );
    corrupt(&container, b"the member the container");
    let into = dir.path().join("out");
    // Made beforehand, so the destination survives the failure and what is left of
    // it can be looked at.
    std::fs::create_dir(&into).expect("destination made first");
    let _path = OnlyPath::new(&[]);

    let error =
        extract(&root(&container), &into, &Cancel::new(), None).expect_err("a bad member fails");
    let message = error.to_string();
    assert!(message.contains("z-bad.txt"), "{message}: names the member");
    assert!(
        message.contains("broken.zip"),
        "{message}: names the container"
    );
    assert!(
        into.join("a-good.txt").is_file(),
        "a member written before the failure stays"
    );
    assert!(
        !into.join("z-bad.txt").exists(),
        "the bad member never appears"
    );
    assert!(
        !paths(&into).iter().any(|name| is_part(name)),
        "{:?}: and its temporary name goes too",
        paths(&into)
    );
}

#[test]
fn a_location_that_is_not_inside_a_container_is_refused() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let into = dir.path().join("out");
    let error = extract(&Location::file(dir.path()), &into, &Cancel::new(), None)
        .expect_err("a plain folder is not a container");
    assert!(matches!(error, Error::InvalidLocation(_)), "{error}");
    assert!(!into.exists(), "a refusal does not create the destination");
}

#[test]
fn a_member_the_container_does_not_hold_is_named() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let container = dir.path().join("tree.zip");
    as_zip(&container, &tree());
    let error = extract(
        &inside(&container, &["nowhere.txt"]),
        &dir.path().join("out"),
        &Cancel::new(),
        None,
    )
    .expect_err("no such member");
    let message = error.to_string();
    assert!(message.contains("nowhere.txt"), "{message}");
    assert!(message.contains("tree.zip"), "{message}");
}

/// Flip a byte of the stored body at `marker`, which turns a member's checksum
/// into a lie without disturbing the index that lists it.
fn corrupt(container: &Path, marker: &[u8]) {
    let mut bytes = std::fs::read(container).expect("container readable");
    let at = bytes
        .windows(marker.len())
        .position(|window| window == marker)
        .expect("the member's bytes are in the container");
    bytes[at] ^= 0xff;
    std::fs::write(container, bytes).expect("written back");
}
