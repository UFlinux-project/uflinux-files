//! The archive fixtures under `tests/archive-fixtures/`, read through the
//! public API.
//!
//! The unit tests inside the crate build their containers with the `zip` crate
//! or from a hand-made [`Toc`], which proves the readers but says nothing about
//! whether a real `zip`, a real GNU `tar`, or a real `7z` can be opened. Those
//! three programs disagree about padding, about how a directory is recorded, and
//! about whether a leading `./` appears, and every one of those differences is a
//! bug that only shows up when a user double-clicks an archive somebody else made.
//!
//! So the containers here are committed as bytes, produced by
//! `tests/archive-fixtures/make.sh`, and this file only ever uses what the crate
//! re-exports: no private module is reachable from `tests/`, which is a feature —
//! a reader that can only be driven from inside the crate cannot drive the UI
//! either.
//!
//! The truncated copies matter more than the whole ones. An archive is the only
//! input in this program that is both untrusted and structurally parsed, and a
//! panic there costs the user a window.
//!
//! What a truncated container has to do depends on how it is indexed, and the
//! distinction is not this reader's to make: `unzip`, Python's `zipfile` and GNU
//! `tar` all behave the same way as the tests below assert. A compressed stream
//! has to be decoded from the beginning, so cutting it short is a failure and
//! `Err` is the only honest answer. A zip or a plain tar is indexed by its own
//! headers, so the part that survives is still a well-formed container and both
//! `unzip -l` and `tar tvf` list it without complaint. What is never acceptable
//! either way is a panic, or a listing that offers a member whose bytes are not
//! there — which is the invariant the second test checks across four sizes.

use std::collections::BTreeSet;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;

use ufm_archive::{ArchiveReader, Format, NodeKind};
use ufm_core::{Location, SortKey};
use ufm_vfs::read_all;

/// Where the committed fixtures live, relative to this crate.
const FIXTURES: &str = "../../tests/archive-fixtures";

/// Every container holds this set of names at its root, plus the root itself.
/// The dot-prefixed one is a member like any other; a listing must report it and
/// mark it hidden, which a separate test checks.
const ROOT_NAMES: [&str; 6] = [".hidden", "bin", "data", "docs", "notes.txt", "outer.zip"];

/// The five members inside `docs`, one of which carries a space, a `!` and
/// non-ASCII in the same name.
const DOC_NAMES: [&str; 3] = ["notes copy.txt", "prázdninový seznam!.txt", "readme.md"];

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

/// The names a listing contains, as a set: the assertions here are about which
/// members are visible, and byte-order sorting is the crate's own business.
fn names(entries: &[ufm_core::Entry]) -> BTreeSet<&str> {
    entries.iter().map(|entry| entry.name.as_str()).collect()
}

/// Read a whole listing, failing with the error rather than a panic inside the
/// reader — which is the distinction the truncated tests are built on.
fn listing(reader: &ArchiveReader, location: &Location) -> Vec<ufm_core::Entry> {
    read_all(reader, location, SortKey::default(), true, None).unwrap_or_else(|e| {
        panic!("{location} should list: {e}");
    })
}

/// Copy a fixture with its last bytes removed, the way a download that stopped
/// early or a half-written USB stick leaves one.
fn truncate(name: &str, dir: &Path, keep: f64) -> PathBuf {
    let bytes = fs::read(fixture(name)).expect("fixture readable");
    // Round down to a whole fraction and then refuse a copy that is still whole,
    // so a mistake in `keep` cannot silently test the intact file.
    let cut = (bytes.len() as f64 * keep) as usize;
    assert!(
        cut > 0 && cut < bytes.len(),
        "{name}: {cut} of {}",
        bytes.len()
    );
    let path = dir.join(name);
    fs::write(&path, &bytes[..cut]).expect("truncated copy written");
    path
}

/// Whether the `7z` CLI is here. The reader for `.7z` shells out to it, so on a
/// machine without it the container cannot be opened at all — the same guard the
/// in-crate tests use, and a skip rather than a failure, because the absence of
/// a decoder says nothing about this code.
fn sevenz_available() -> bool {
    Command::new("7z")
        .arg("i")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// The tree every container holds, asserted against each format in turn. Written
/// once as a function so a format that disagrees fails with the fixture named.
fn assert_same_tree(name: &str) {
    let path = fixture(name);
    let reader = ArchiveReader::new();
    let entries = listing(&reader, &root(&path));
    assert_eq!(
        names(&entries),
        ROOT_NAMES.into_iter().collect(),
        "{name}: root listing"
    );

    let docs = listing(&reader, &inside(&path, &["docs"]));
    assert_eq!(
        names(&docs),
        DOC_NAMES.into_iter().collect(),
        "{name}: docs listing"
    );

    // The script keeps its executable bit where the format records a mode at
    // all; a reader that synthesized 0644 for everything would pass the listing
    // tests and show the wrong permissions in the properties dialog.
    let script = listing(&reader, &inside(&path, &["bin"]))
        .into_iter()
        .find(|entry| entry.name == "run.sh")
        .expect("bin/run.sh listed");
    assert_eq!(script.mode.map(|mode| mode & 0o111), Some(0o111));

    let hidden = entries
        .iter()
        .find(|entry| entry.name == ".hidden")
        .expect("a dotfile is listed, not dropped");
    assert!(hidden.hidden(), "{name}: .hidden marked hidden");
    assert!(
        entries.iter().all(|entry| !entry.writable()),
        "{name}: nothing inside a container is writable"
    );

    // The counts a listing shows for a directory come from the table of
    // contents, because there is no readdir to ask inside a container.
    let docs_entry = entries
        .iter()
        .find(|entry| entry.name == "docs")
        .expect("docs listed");
    assert_eq!(docs_entry.child_count, Some(3), "{name}: child count");
}

#[test]
fn a_zip_from_the_zip_cli_browses() {
    assert_same_tree("sample.zip");
}

#[test]
fn a_tar_gz_from_gnu_tar_browses() {
    assert_same_tree("sample.tar.gz");
}

#[test]
fn a_tar_xz_from_gnu_tar_browses() {
    assert_same_tree("sample.tar.xz");
}

#[test]
fn a_7z_from_the_7z_cli_browses() {
    if !sevenz_available() {
        return;
    }
    assert_same_tree("sample.7z");
}

#[test]
fn each_fixture_is_detected_from_its_bytes() {
    for name in ["sample.zip", "sample.tar.gz", "sample.tar.xz", "nested.tar"] {
        let path = fixture(name);
        let head = fs::read(&path).expect("readable");
        let detected = Format::detect(&path, &head).unwrap_or_else(|| panic!("{name}: detected"));
        assert_eq!(
            Some(detected),
            Format::from_name(&path),
            "{name}: the bytes and the extension agree"
        );
    }
    // `.7z` is detected from its magic too, and detection does not need the
    // decoder to be installed — only opening it does.
    let sevenz = fixture("sample.7z");
    let head = fs::read(&sevenz).expect("readable");
    assert_eq!(
        Format::detect(&sevenz, &head),
        Some(Format::Sevenz),
        "the 7z magic is recognised"
    );
}

#[test]
fn one_member_extracts_byte_for_byte() {
    let reader = ArchiveReader::new();
    // The same content in every container, so one expectation covers all four.
    let expected = "line 01: the quick brown fox jumps over the lazy dog\n";
    for name in ["sample.zip", "sample.tar.gz", "sample.tar.xz", "sample.7z"] {
        if name == "sample.7z" && !sevenz_available() {
            continue;
        }
        let path = fixture(name);
        let location = inside(&path, &["notes.txt"]);
        let mut out = Vec::new();
        let mut collect = |chunk: &[u8]| -> ufm_core::Result<()> {
            out.extend_from_slice(chunk);
            Ok(())
        };
        reader
            .stream(&location, &mut collect)
            .unwrap_or_else(|e| panic!("{name}: {location} readable: {e}"));
        let text = String::from_utf8(out).expect("utf-8 body");
        assert!(
            text.starts_with(expected),
            "{name}: first line of the extracted member"
        );
        assert!(
            text.lines().count() > 30,
            "{name}: the whole member arrived, not a prefix"
        );
    }
}

#[test]
fn the_uncompressed_total_is_reported() {
    let reader = ArchiveReader::new();
    for name in ["sample.zip", "sample.tar.gz", "sample.tar.xz"] {
        let opened = reader
            .contents(&fixture(name))
            .unwrap_or_else(|e| panic!("{name}: opened: {e}"));
        // The containers are all under 2 KiB of real content; a total in the
        // terabytes would mean a size field was read from the wrong offset.
        assert!(
            opened.total_bytes() > 1000 && opened.total_bytes() < 100_000,
            "{name}: plausible uncompressed total, got {}",
            opened.total_bytes()
        );
        assert!(
            opened.toc().len() > 8,
            "{name}: the table of contents is populated"
        );
    }
}

/// Two levels: a zip inside a tar. The outer container lists the inner one as an
/// archive, and the location that entry carries is what a second read opens, so
/// this is the test for the handover between the two levels rather than for
/// either reader on its own.
#[test]
fn a_zip_inside_a_tar_navigates_two_levels() {
    let path = fixture("nested.tar");
    let dir = tempfile::TempDir::new().expect("temp dir");
    let extracted = dir.path().join("outer.zip");
    let mut file = fs::File::create(&extracted).expect("extract target");
    let reader = ArchiveReader::new();
    let entries = listing(&reader, &root(&path));
    let inner = entries
        .iter()
        .find(|entry| entry.name == "outer.zip")
        .expect("the container is listed");
    assert_eq!(inner.kind, ufm_core::EntryKind::Archive);
    assert!(inner.navigable(), "double-clicking it goes in");
    assert_eq!(inner.icon_key, "package-x-generic");
    assert_eq!(inner.mime, None, "a container is keyed by icon, not MIME");

    // What makes a second level reachable is the location the entry carries. One
    // `Location` names one container, so it points at the member holding the
    // inner container's bytes rather than inside that container, and listing it
    // directly is a mistake the reader has to report rather than paper over.
    assert_eq!(
        inner.location,
        inside(&path, &["outer.zip"]),
        "the handover location names the container and the member"
    );
    let error = read_all(&reader, &inner.location, SortKey::default(), true, None)
        .expect_err("a member is a file, not a directory to list");
    assert!(
        error.to_string().contains("outer.zip"),
        "{error}: the message names the member"
    );

    // The extracted member is a container in its own right, and reading it that
    // way is the whole second level. This is the shape of the call the UI will
    // make once archive members can be written out, which is what
    // `ufm_core::paths::extracted_dir()` exists for.
    reader
        .stream(&inner.location, &mut |chunk| {
            file.write_all(chunk).map_err(ufm_core::Error::from)
        })
        .expect("the inner container streams out");
    drop(file);
    let second = listing(&reader, &root(&extracted));
    assert_eq!(
        names(&second),
        ["README", "deep"].into_iter().collect(),
        "the inner container lists its own members"
    );
    let deep = listing(&reader, &inside(&extracted, &["deep"]));
    assert_eq!(
        names(&deep),
        ["inside.txt"].into_iter().collect(),
        "the third level reads through the extracted container"
    );
}

/// A compressed stream has to be decoded from its first byte, so a container
/// missing its tail cannot be listed at all — there is no index to fall back on.
/// This is the group where `Err` is the only honest answer, and the answer has to
/// arrive at open time, where the view can turn it into one error row.
#[test]
fn a_truncated_compressed_container_is_an_error_and_not_a_panic() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    for name in ["sample.tar.gz", "sample.tar.xz", "sample.7z"] {
        if name == "sample.7z" && !sevenz_available() {
            continue;
        }
        for keep in [0.1, 0.3, 0.6, 0.9] {
            let path = truncate(name, dir.path(), keep);
            let reader = ArchiveReader::new();
            // Destructured rather than `expect_err` because the success arm is
            // an `Arc<Opened>`, which is deliberately not `Debug`: a table of
            // contents is a large thing to dump into a panic message.
            let Err(error) = reader.contents(&path) else {
                panic!("{name} at {keep}: opened without its tail");
            };
            assert!(
                !error.to_string().is_empty(),
                "{name} at {keep}: the message names what went wrong"
            );
            // And the failure has to be repeatable: a reader that half-opened a
            // broken container would cache it and then panic on the second visit.
            assert!(
                reader.contents(&path).is_err(),
                "{name} at {keep}: the failure is not a one-off"
            );
        }
    }
}

/// A zip keeps its index in a central directory at the end and a plain tar in
/// header blocks as it goes, so the surviving head of either is still a
/// well-formed container and `unzip -l`/`tar tvf` list it without complaint.
/// Cutting a plain tar part-way through a member's body is the awkward case: the
/// header is whole, so the member is listed with the size it was written with,
/// and GNU tar lists it too and only complains when asked to extract it.
///
/// What is never acceptable is a member that reads short and reports success —
/// that is how a truncated archive silently loses the tail of a file — so the
/// promise checked here is that every listed member either yields exactly the
/// number of bytes the listing advertised, or says so with an error.
#[test]
fn a_truncated_indexed_container_never_reads_a_member_short_in_silence() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    for name in ["sample.zip", "nested.tar"] {
        for keep in [0.1, 0.3, 0.5, 0.6, 0.9] {
            let path = truncate(name, dir.path(), keep);
            let reader = ArchiveReader::new();
            if reader.contents(&path).is_err() {
                continue;
            }
            for entry in listing(&reader, &root(&path)) {
                // A directory is a row in the index, not a body of bytes, so it
                // is always fair to list and there is nothing to read.
                if entry.kind == ufm_core::EntryKind::Directory {
                    continue;
                }
                let member = inside(&path, &[entry.name.as_str()]);
                let mut read = 0u64;
                let outcome = reader.stream(&member, &mut |chunk| {
                    read += u64::try_from(chunk.len()).unwrap_or(0);
                    Ok(())
                });
                assert!(
                    outcome.is_err() || read == entry.size,
                    "{name} at {keep}: {member} was listed at {} bytes and read \
                     {read} without an error",
                    entry.size
                );
            }
        }
    }
}

/// The same containers at the sizes where every member's bytes did survive — a
/// cut on a block boundary that falls between two members — and at those sizes a
/// listing that offers a member has to be able to hand it over whole. Half of a
/// plain tar is the one case that falls out: the cut lands inside `notes.txt`,
/// whose header is complete, so it is listed and reading it reports an error,
/// which is what the test above allows and GNU tar does too.
#[test]
fn a_plain_tar_cut_between_members_still_reads_everything_it_lists() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    for keep in [0.1, 0.3, 0.6] {
        let path = truncate("nested.tar", dir.path(), keep);
        let reader = ArchiveReader::new();
        let opened = reader
            .contents(&path)
            .unwrap_or_else(|e| panic!("at {keep}: opened: {e}"));
        assert_eq!(
            opened.format(),
            Format::Tar(ufm_archive::Codec::None),
            "a head of a tar is still a tar"
        );
        for entry in listing(&reader, &root(&path)) {
            if entry.kind == ufm_core::EntryKind::Directory {
                continue;
            }
            let member = inside(&path, &[entry.name.as_str()]);
            let mut read = 0u64;
            reader
                .stream(&member, &mut |chunk| {
                    read += u64::try_from(chunk.len()).unwrap_or(0);
                    Ok(())
                })
                .unwrap_or_else(|e| panic!("at {keep}: {member}: {e}"));
            assert_eq!(read, entry.size, "at {keep}: {member} read whole");
        }
    }
}

/// The specific shape the truncated zip takes, asserted rather than excused.
/// `sample.zip` carries `outer.zip`, a container of its own, uncompressed near
/// the front — so a copy missing its tail still holds a complete end-of-central-
/// directory record, belonging to the inner archive. Both `unzip -l` and
/// Python's `zipfile` read such a file as the inner container, and so does this
/// one. It is the one case where a half-archive opens as a *different* archive,
/// which is worth pinning down because it is so easy to mistake for a bug in the
/// reader.
#[test]
fn a_truncated_zip_falls_back_to_a_container_it_holds() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let path = truncate("sample.zip", dir.path(), 0.6);
    let reader = ArchiveReader::new();
    let entries = listing(&reader, &root(&path));
    assert_eq!(
        names(&entries),
        ["README", "deep"].into_iter().collect(),
        "the surviving index is the embedded container's"
    );
    // The whole container is 1760 bytes and the embedded one ends around 683, so
    // the copy is genuinely short and still parses.
    assert!(
        fs::metadata(&path).expect("truncated copy").len() < 1200,
        "the copy really is missing most of the container"
    );
}

/// A container that is not a container: the reader is asked about a file whose
/// bytes match nothing, which is what happens when a `.zip` turns out to be a
/// text file. The answer must be an error naming the file, not a guess.
#[test]
fn a_file_that_only_lies_about_being_an_archive_is_refused() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let path = dir.path().join("lie.zip");
    fs::write(&path, b"not an archive at all\n").expect("written");
    let reader = ArchiveReader::new();
    let Err(error) = reader.contents(&path) else {
        panic!("the bytes were accepted over the extension");
    };
    assert!(
        error.to_string().contains("lie.zip"),
        "{error}: the message names the file"
    );
}

/// Reading the same container twice must not re-parse it, and must not hand back
/// a table of contents that has drifted. The cache is what makes navigating back
/// into an archive instant, and a cache keyed wrongly shows a stale listing
/// after the file on disk changed.
#[test]
fn a_container_is_parsed_once_and_reopened_after_a_change() {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let path = dir.path().join("copy.zip");
    fs::copy(fixture("sample.zip"), &path).expect("fixture copied");
    let reader = ArchiveReader::new();

    let first = reader.contents(&path).expect("opened");
    let again = reader.contents(&path).expect("opened twice");
    assert!(
        Arc::ptr_eq(&first, &again),
        "the second open is the cached one"
    );
    assert_eq!(first.toc().len(), again.toc().len());

    // Replace the container with a different one of the same size and check the
    // reader notices. The cache key carries the modification time and the size,
    // so a rewrite that changes neither is the one case it cannot see, and this
    // asserts the case it can.
    fs::copy(fixture("nested.tar"), &path).expect("replaced");
    let third = reader.contents(&path).expect("reopened");
    assert!(!Arc::ptr_eq(&first, &third), "a changed file is re-read");
    assert_eq!(
        third.format(),
        Format::Tar(ufm_archive::Codec::None),
        "the replacement is detected as what it is"
    );
    assert_eq!(
        third.toc().node(0).map(|node| node.kind),
        Some(NodeKind::Dir),
        "node 0 is the root of the container"
    );
}
