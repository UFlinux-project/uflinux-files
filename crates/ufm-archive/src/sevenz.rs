//! `.7z`, through the `7z` command-line tool.
//!
//! A native reader would be nicer, but the two crates that could provide one are
//! both unusable here: `sevenz-rust` is unmaintained with an open advisory
//! (RUSTSEC-2026-0246) and its successor needs a newer compiler than the
//! workspace floor. The `7z` binary is already a package dependency for creating
//! archives, so reading through it costs no new dependency and no extra resident
//! memory — the decompressor lives in another process and is gone when the read
//! ends.
//!
//! Two properties of that tool shape this module. Its listing is text, so it is
//! parsed; and its member selection treats `*` and `?` as wildcards with no way to
//! escape them, so a member whose name contains one is reported rather than
//! silently returning the wrong bytes.
//!
//! The child runs with `LC_ALL=C.UTF-8`, because under a plain `C` locale `7z`
//! escapes every non-ASCII byte in a name as `\xNN` and a file called `café.txt`
//! would be listed as `caf\xc3\xa9.txt`. It also runs with `TZ=UTC0`, because
//! `Modified` is printed in local time and the timestamps have to mean the same
//! thing regardless of where the machine is.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::SystemTime;

use ufm_core::error::{Error, Result};

use crate::command;
use crate::format::Format;
use crate::toc::{NO_HANDLE, NodeKind, Toc, TocBuilder};
use crate::util::{days_from_civil, epoch, full_mode};
use crate::{CHUNK, Container};

/// Refuse a container over this many members, as the other backends do.
const MAX_MEMBERS: usize = 200_000;
const PROGRAM: &str = "7z";

/// A `.7z` container, read by running `7z` when asked.
///
/// Nothing is held open: the listing is cached as a [`Toc`] like every other
/// format, and reading a member starts a fresh process. That is slower than a seek
/// and cheaper than a decompressor in the resident set.
pub(crate) struct SevenzContainer {
    path: PathBuf,
}

const _: fn() = || {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<SevenzContainer>();
};

impl SevenzContainer {
    /// Refuse the archive up front if there is nothing to read it with, so the
    /// view can say so instead of failing on the first double-click.
    pub(crate) fn new(path: PathBuf) -> Result<Self> {
        if !command::available(PROGRAM) {
            return Err(command::missing(PROGRAM, &path));
        }
        Ok(Self { path })
    }

    /// `7z` with the environment the parser needs, and the archive at the end.
    fn invoke(&self, args: &[&str]) -> Command {
        let mut command = command::command(PROGRAM);
        command
            .args(args)
            .arg("--")
            .arg(&self.path)
            // Names as stored, not escaped; timestamps in UTC.
            .env("LC_ALL", "C.UTF-8")
            .env("TZ", "UTC0")
            .current_dir(self.path.parent().unwrap_or(Path::new("/")));
        command
    }
}

impl Container for SevenzContainer {
    fn path(&self) -> &Path {
        &self.path
    }

    fn format(&self) -> Format {
        Format::Sevenz
    }

    fn scan(&self) -> Result<Arc<Toc>> {
        let mut listing = self.invoke(&["l", "-slt"]);
        let listing = command::output(&mut listing, &self.path, PROGRAM)?;
        let toc = parse_listing(&listing)?;
        if toc.is_empty() {
            return Err(Error::Archive(format!(
                "{}: no members found",
                self.path.display()
            )));
        }
        Ok(Arc::new(toc))
    }

    fn read(&self, toc: &Toc, node: usize, out: &mut dyn FnMut(&[u8]) -> Result<()>) -> Result<()> {
        let name = toc.path_of(node);
        let expected = toc.node(node).map_or(0, |n| n.size);
        // `-so` writes the member to stdout, `-bso0` and `-bsp0` keep the banner
        // and the progress meter out of it, `-y` answers the overwrite prompt that
        // would otherwise block on a name collision. The member name goes after the
        // archive, so a name starting with `-` is not read as a switch.
        let mut command = self.invoke(&["x", "-so", "-y", "-bso0", "-bsp0"]);
        command.arg(&name);
        let mut child = command::spawn(command, &self.path)?;
        let mut source = std::io::BufReader::new(&mut child);
        let mut written = 0u64;
        let mut buffer = vec![0u8; CHUNK];
        loop {
            use std::io::Read;
            let read = source.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            written += read as u64;
            out(&buffer[..read])?;
        }
        drop(source);
        child.finish()?;
        if written != expected {
            // `7z` exits 0 when it matched nothing, and it matches on patterns, so
            // the byte count is what says whether the right member was read.
            let archive = self.path.display();
            return Err(Error::Archive(if name.contains(['*', '?']) {
                format!("{archive}: {name} cannot be read because 7z treats * and ? as wildcards")
            } else {
                format!("{archive}: {name} is not in the archive")
            }));
        }
        Ok(())
    }
}

/// Parse `7z l -slt` output.
///
/// The output is a banner, a header block about the container, a `----------`
/// line, and then one `KEY = VALUE` block per member separated by blank lines.
/// Only the member blocks are read; the header block is above the separator.
fn parse_listing(bytes: &[u8]) -> Result<Toc> {
    // The separator is a newline-terminated line, except when it is the first
    // thing printed, which happens for a container whose own header block is
    // empty — a tar inside a 7z printed without a banner, as the tests do.
    const SEPARATOR: &[u8] = b"----------\n";
    let start = if bytes.starts_with(SEPARATOR) {
        SEPARATOR.len()
    } else {
        bytes
            .windows(SEPARATOR.len() + 1)
            .position(|window| window == b"\n----------\n")
            .map(|at| at + SEPARATOR.len() + 1)
            .ok_or_else(|| Error::Archive("not a 7z archive".to_string()))?
    };

    let mut builder = TocBuilder::with_limit(MAX_MEMBERS);
    let mut fields: Vec<(&[u8], &[u8])> = Vec::new();
    let mut members = 0usize;
    for line in bytes[start..].split(|&byte| byte == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.is_empty() {
            if !fields.is_empty() {
                members += usize::from(push_member(&mut builder, &fields)?);
                fields.clear();
            }
            continue;
        }
        if let Some((key, value)) = split_field(line) {
            fields.push((key, value));
        }
    }
    if !fields.is_empty() {
        members += usize::from(push_member(&mut builder, &fields)?);
    }
    if members == 0 {
        return Err(Error::Archive("not a 7z archive".to_string()));
    }
    Ok(builder.finish())
}

/// Split a `KEY = VALUE` line on the first ` = `, which is the only place a value
/// can be told from a key: a file name may itself contain ` = `.
fn split_field(line: &[u8]) -> Option<(&[u8], &[u8])> {
    let at = find(line, b" = ")?;
    let key = &line[..at];
    if key.is_empty() || !key.iter().all(|b| b.is_ascii_alphanumeric() || *b == b' ') {
        return None;
    }
    Some((key, &line[at + 3..]))
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// A field's value, `None` when the field is absent or printed empty. `7z` prints
/// `Size = ` for a size it does not know and `CRC = ` for a container it cannot
/// check, and an empty value means the same thing as no field: nothing is known.
fn field<'a>(fields: &[(&'a [u8], &'a [u8])], key: &[u8]) -> Option<&'a [u8]> {
    fields
        .iter()
        .find(|(name, _)| *name == key)
        .map(|(_, value)| *value)
        .filter(|value| !value.is_empty())
}

/// Add one member from its fields. `false` for a block that names no path, which
/// is how a stray blank line or a trailing summary is skipped.
fn push_member(builder: &mut TocBuilder, fields: &[(&[u8], &[u8])]) -> Result<bool> {
    let Some(path) = field(fields, b"Path").map(unescape) else {
        return Ok(false);
    };
    let kind = kind_of(fields);
    let size = field(fields, b"Size").and_then(parse_number).unwrap_or(0);
    let modified = field(fields, b"Modified").and_then(parse_time);
    let mode = mode_of(fields, kind).map(|permissions| full_mode(kind, permissions));
    let uid = field(fields, b"User ID")
        .and_then(parse_number)
        .map(|v| v as u32);
    let gid = field(fields, b"Group ID")
        .and_then(parse_number)
        .map(|v| v as u32);
    builder.push(&path, kind, size, modified, mode, uid, gid, NO_HANDLE)?;
    Ok(true)
}

/// What a member is. `7z` says it three ways depending on what it is reading: a
/// `Folder`/`Symbolic Link` field for a container it understands, the first word
/// of `Attributes` for its own format, and `Characteristics` for a device inside a
/// tar.
fn kind_of(fields: &[(&[u8], &[u8])]) -> NodeKind {
    if field(fields, b"Symbolic Link").is_some_and(|v| v.first() == Some(&b'+')) {
        return NodeKind::Symlink;
    }
    if field(fields, b"Folder").is_some_and(|v| v.first() == Some(&b'+')) {
        return NodeKind::Dir;
    }
    if let Some(characteristics) = field(fields, b"Characteristics") {
        let text = String::from_utf8_lossy(characteristics).to_ascii_lowercase();
        if text.contains("device") || text.contains("fifo") {
            return NodeKind::Special;
        }
    }
    match field(fields, b"Attributes").and_then(|v| v.first()) {
        Some(b'D') => NodeKind::Dir,
        Some(b'L') => NodeKind::Symlink,
        _ => NodeKind::File,
    }
}

/// Permissions, from the symbolic form when `7z` prints one.
fn mode_of(fields: &[(&[u8], &[u8])], kind: NodeKind) -> Option<u32> {
    field(fields, b"Mode")
        .and_then(parse_symbolic)
        .or_else(|| {
            field(fields, b"Attributes")
                .and_then(|v| v.split(|&b| b == b' ').nth(1))
                .and_then(parse_symbolic)
        })
        // A container that stores no permissions at all — a zip inside a 7z, say —
        // gets the type bits and the usual defaults.
        .or(Some(if kind.is_dir() { 0o755 } else { 0o644 }))
}

/// Read a symbolic mode like `drwxr-xr-x`, ignoring the type character, which the
/// node kind already carries. `s` and `t` count as executable, which is what the
/// setuid and sticky bits look like in the middle position.
fn parse_symbolic(field: &[u8]) -> Option<u32> {
    if field.len() != 10 {
        return None;
    }
    let mut mode = 0u32;
    // `ls`-style: the execute slot of each triple carries the special bit as
    // `s` (setuid, setgid) or `t` (sticky), and as the uppercase form when the
    // bit is set without execute.
    for (group, bits, special) in [
        (1usize, 0o400u32, 0o4000u32),
        (4, 0o040, 0o2000),
        (7, 0o004, 0o1000),
    ] {
        if field[group] != b'-' {
            mode |= bits;
        }
        if field[group + 1] != b'-' {
            mode |= bits >> 1;
        }
        match field[group + 2] {
            b'-' => {}
            b'x' => mode |= bits >> 2,
            c if c == b's' || c == b'S' || c == b't' || c == b'T' => {
                if special == 0o1000 && c != b't' && c != b'T' {
                    return None;
                }
                mode |= special;
                // Lowercase means the execute bit is set too.
                if c == b's' || c == b't' {
                    mode |= bits >> 2;
                }
            }
            _ => return None,
        }
    }
    Some(mode)
}

/// A decimal number, `None` for the empty field `7z` prints for an unknown size.
fn parse_number(field: &[u8]) -> Option<u64> {
    let text = std::str::from_utf8(field).ok()?.trim();
    if text.is_empty() {
        return None;
    }
    text.parse().ok()
}

/// Parse `YYYY-MM-DD HH:MM:SS`, optionally with a fraction, as UTC.
fn parse_time(field: &[u8]) -> Option<SystemTime> {
    let text = std::str::from_utf8(field).ok()?;
    let (date, rest) = text.split_once(' ')?;
    let mut parts = date.split('-');
    let year: u32 = parts.next()?.parse().ok()?;
    let month: i64 = parts.next()?.parse().ok()?;
    let day: i64 = parts.next()?.parse().ok()?;
    // The fraction is there for a container that stores one and absent for a
    // format that does not, so the clock is split on `.` only when there is one.
    let (clock, fraction) = match rest.split_once('.') {
        Some((clock, fraction)) => (clock, Some(fraction)),
        None => (rest, None),
    };
    let mut clock = clock.split(':');
    let hour: u64 = clock.next()?.parse().ok()?;
    let minute: u64 = clock.next()?.parse().ok()?;
    let second: u64 = clock.next()?.parse().ok()?;
    let fraction = fraction.unwrap_or("").as_bytes();
    let fraction = fraction
        .iter()
        .copied()
        .take_while(u8::is_ascii_digit)
        .map(|b| u64::from(b - b'0'))
        .fold((0u64, 0usize), |(value, digits), digit| {
            (value * 10 + digit, digits + 1)
        });
    let nanos = match fraction.1 {
        0 => 0,
        digits => (fraction.0 * 10u64.pow(9u32.saturating_sub(digits as u32))) as u32,
    };
    let days = days_from_civil(year, month, day)?;
    epoch(days * 86_400 + hour * 3_600 + minute * 60 + second, nanos)
}

/// A member name, undoing the two escapes `7z` applies when printing: `\NNN` for
/// a byte it would rather not print and `^X` for a control character. A real
/// newline in a name comes out as `_`, which cannot be told from a literal
/// underscore and is left as it is.
/// Undo the escaping `7z` applies to a name it cannot print. `\xNN` names a
/// code point, not a byte: the tool converts the archive's Unicode name through
/// the current code page, so `café.txt` comes back as `caf\xe9.txt` and the two
/// UTF-8 bytes of the original name are gone. Decoding the escape as a character
/// is what puts the name back.
fn unescape(field: &[u8]) -> String {
    let mut out = String::with_capacity(field.len());
    let mut rest = field;
    while let Some(at) = rest.iter().position(|&byte| byte == b'\\' || byte == b'^') {
        // Whatever precedes an escape is printed as-is and is the archive's own
        // bytes, so it is decoded rather than copied.
        out.push_str(&String::from_utf8_lossy(&rest[..at]));
        let tail = &rest[at + 1..];
        match (rest[at], tail.first()) {
            // `\xNN`, which is the form the tool actually prints. The `x` is
            // optional so that a `\NN` form, which older versions used, reads the
            // same way.
            (b'\\', Some(second)) if tail.len() >= 3 => {
                let digits = if *second == b'x' {
                    &tail[1..3]
                } else {
                    &tail[..2]
                };
                let skip = usize::from(*second == b'x');
                let value = u8::from_str_radix(&String::from_utf8_lossy(digits), 16).ok();
                if let Some(byte) = value {
                    out.push(char::from(byte));
                    // `tail` already starts after the backslash, so the escape
                    // is the `x` plus the two digits and nothing more.
                    rest = &tail[skip + 2..];
                } else {
                    out.push('\\');
                    rest = tail;
                }
            }
            (b'^', Some(second)) if second.is_ascii_alphabetic() || *second == b'[' => {
                out.push(char::from(control_byte(*second)));
                rest = &tail[1..];
            }
            // A marker that starts nothing is part of the name.
            _ => {
                out.push(char::from(rest[at]));
                rest = tail;
            }
        }
    }
    out.push_str(&String::from_utf8_lossy(rest));
    out
}

/// `^A` is 0x01, `^I` is a tab: a control character is printed as the letter
/// 0x40 above it, which is the ASCII convention rather than anything `7z` invented.
const fn control_byte(printable: u8) -> u8 {
    printable.wrapping_add(0x40) & 0x1f
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A listing in the shape `7z l -slt` produces for a native `.7z`, taken from
    /// the real tool's output.
    const LISTING: &[u8] = br"
7-Zip [64] 26.04

Listing archive: /home/u/a.7z

--
Path = /home/u/a.7z
Type = 7z
Physical Size = 260
Method = LZMA2:12
Solid = +
Blocks = 1

----------
Path = src
Folder = +
Modified = 2026-11-13 15:41:15.0000000
Attributes = D drwxr-xr-x

Path = src/dir with space/sub/a.txt
Size = 6
Packed Size = 6
Modified = 2026-11-13 15:41:15.0000000
Attributes = A -rw-r--r--
CRC = 
Encrypted = -
Method = LZMA2:12
Block = 0

Path = r=e.txt
Size = 2
Modified = 2026-11-13 15:41:15
Attributes = A -rw-r--r--
";

    #[test]
    fn a_listing_becomes_a_table_of_contents() {
        let toc = parse_listing(LISTING).expect("parsed");
        assert_eq!(
            toc.len(),
            6,
            "root, src, the directory inside it, its sub, the file, r=e.txt"
        );
        let src = toc.node(toc.find("src").expect("directory")).expect("node");
        assert_eq!(src.kind, NodeKind::Dir);
        assert_eq!(src.mode, Some(0o040_755));
        let file = toc
            .node(toc.find("src/dir with space/sub/a.txt").expect("nested"))
            .expect("node");
        assert_eq!(file.kind, NodeKind::File);
        assert_eq!(file.size, 6);
        assert_eq!(file.mode, Some(0o100_644));
        assert_eq!(
            file.modified
                .expect("dated")
                .duration_since(std::time::UNIX_EPOCH)
                .expect("after epoch")
                .as_secs(),
            1_794_584_475,
            "15:41:15 UTC, the fraction ignored"
        );
        assert_eq!(toc.total_bytes, 8, "only file bytes count");
    }

    #[test]
    fn a_value_containing_an_equals_sign_survives() {
        let toc = parse_listing(LISTING).expect("parsed");
        assert!(toc.find("r=e.txt").is_some(), "split on the first one only");
    }

    #[test]
    fn a_listing_with_no_member_blocks_is_not_an_archive() {
        let error = parse_listing(b"7-Zip [64]\n\n--\nPath = x\nType = 7z\n").unwrap_err();
        assert!(error.to_string().contains("not a 7z"), "{error}");
    }

    #[test]
    fn a_tar_inside_a_7z_contributes_owner_and_symlinks() {
        let listing = b"----------\nPath = link\nFolder = -\nMode = lrwxrwxrwx\nSymbolic Link = +\nModified = 2026-11-13 15:41:15\nUser ID = 1000\nGroup ID = 100\n\nPath = dev\nCharacteristics = Block device\nMode = brw-r--r--\n\n";
        let toc = parse_listing(listing).expect("parsed");
        let link = toc.node(toc.find("link").expect("symlink")).expect("node");
        assert_eq!(link.kind, NodeKind::Symlink);
        assert_eq!(link.mode, Some(0o120_777));
        assert_eq!(link.uid, Some(1000));
        assert_eq!(link.gid, Some(100));
        let dev = toc.node(toc.find("dev").expect("device")).expect("node");
        assert_eq!(dev.kind, NodeKind::Special);
        assert_eq!(dev.mode, Some(0o020_644));
    }

    #[test]
    fn a_missing_mode_is_filled_in_from_the_kind() {
        let listing = b"----------\nPath = plain.txt\nSize = 1\n\nPath = adir\nFolder = +\n\n";
        let toc = parse_listing(listing).expect("parsed");
        assert_eq!(
            toc.node(toc.find("plain.txt").expect("f"))
                .expect("node")
                .mode,
            Some(0o100_644)
        );
        assert_eq!(
            toc.node(toc.find("adir").expect("d")).expect("node").mode,
            Some(0o040_755)
        );
    }

    #[test]
    fn times_parse_with_and_without_a_fraction() {
        let at = parse_time(b"2026-11-13 15:41:15").expect("parsed");
        assert_eq!(
            at.duration_since(std::time::UNIX_EPOCH)
                .expect("after")
                .as_secs(),
            1_794_584_475
        );
        let with_fraction = parse_time(b"2026-11-13 15:41:15.2500000").expect("parsed");
        let at = with_fraction
            .duration_since(std::time::UNIX_EPOCH)
            .expect("after");
        assert_eq!(at.as_secs(), 1_794_584_475);
        assert_eq!(at.subsec_nanos(), 250_000_000);
        assert_eq!(parse_time(b"not a time"), None);
    }

    #[test]
    fn escapes_are_undone() {
        assert_eq!(unescape(b"plain"), "plain");
        assert_eq!(unescape(b"caf\\xe9.txt"), "café.txt");
        assert_eq!(unescape(b"a^Ab"), "a\x01b");
        assert_eq!(unescape(b"tab^I"), "tab\t");
        // A backslash that is not an escape stays, so a name containing one is not
        // mangled.
        assert_eq!(unescape(b"back\\slash"), "back\\slash");
        assert_eq!(unescape(b"\\xzz"), "\\xzz");
    }

    #[test]
    fn control_bytes_follow_the_caret_convention() {
        assert_eq!(control_byte(b'A'), 0x01);
        assert_eq!(control_byte(b'I'), b'\t');
        assert_eq!(control_byte(b'['), 0x1b);
    }

    #[test]
    fn symbolic_modes_read_as_permissions() {
        assert_eq!(parse_symbolic(b"drwxr-xr-x"), Some(0o755));
        assert_eq!(parse_symbolic(b"-rw-r--r--"), Some(0o644));
        assert_eq!(parse_symbolic(b"-rwsr-xr-x"), Some(0o4755));
        assert_eq!(parse_symbolic(b"-rwxrwxrwx"), Some(0o777));
        assert_eq!(parse_symbolic(b"short"), None);
    }

    #[test]
    fn a_real_container_round_trips_when_7z_is_installed() {
        if !command::available(PROGRAM) {
            return; // The tool is a package dependency, not a test dependency.
        }
        let dir = tempfile::TempDir::new().expect("temp dir");
        let source = dir.path().join("src");
        std::fs::create_dir_all(source.join("dir with space/sub")).expect("mkdir");
        std::fs::write(source.join("dir with space/sub/a.txt"), b"inside").expect("write");
        std::fs::write(source.join("note.txt"), b"note").expect("write");
        let path = dir.path().join("a.7z");
        let mut create = command::command(PROGRAM);
        create.args(["a", "-t7z"]).arg(&path).arg("src");
        create.current_dir(dir.path()).env("LC_ALL", "C.UTF-8");
        command::status_ok(&mut create, &path, PROGRAM).expect("created");

        let container = SevenzContainer::new(path.clone()).expect("opened");
        assert_eq!(container.format(), Format::Sevenz);
        let toc = container.scan().expect("scanned");
        assert!(
            toc.find("src/dir with space/sub/a.txt").is_some(),
            "the nested file is listed"
        );
        assert!(toc.find("src/note.txt").is_some(), "the sibling is listed");
        assert!(toc.find("src/nope.txt").is_none(), "absent stays absent");

        let at = toc.find("src/dir with space/sub/a.txt").expect("listed");
        let mut body = Vec::new();
        container
            .read(&toc, at, &mut |chunk| {
                body.extend_from_slice(chunk);
                Ok(())
            })
            .expect("read");
        assert_eq!(body, b"inside");
    }

    #[test]
    fn a_file_that_is_not_a_7z_is_reported() {
        if !command::available(PROGRAM) {
            return;
        }
        let dir = tempfile::TempDir::new().expect("temp dir");
        let path = dir.path().join("junk.7z");
        std::fs::write(&path, b"not an archive at all").expect("write");
        let container = SevenzContainer::new(path).expect("the tool exists");
        let error = container.scan().unwrap_err();
        assert!(error.to_string().contains("junk.7z"), "{error}");
    }

    #[test]
    fn the_kind_falls_back_to_the_attributes_word() {
        assert_eq!(kind_of(&[(b"Attributes", b"D drwxr-xr-x")]), NodeKind::Dir);
        assert_eq!(kind_of(&[(b"Attributes", b"A -rw-r--r--")]), NodeKind::File);
        assert_eq!(kind_of(&[]), NodeKind::File, "nothing to go on");
    }

    #[test]
    fn a_field_key_never_looks_like_a_name() {
        assert_eq!(
            split_field(b"Path = a = b"),
            Some((&b"Path"[..], &b"a = b"[..]))
        );
        assert_eq!(split_field(b"not a field"), None);
        assert_eq!(split_field(b"= value"), None);
    }

    #[test]
    fn mode_bits_come_from_the_kind_not_the_listing() {
        use crate::util::mode_bits;
        assert_eq!(mode_bits(NodeKind::Dir), 0o040_000);
    }
}
