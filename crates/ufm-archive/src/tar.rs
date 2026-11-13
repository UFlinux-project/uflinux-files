//! tar, parsed from the bytes rather than from `tar -tv` output.
//!
//! Parsing a listing would mean accepting three different grammars — GNU tar,
//! busybox tar and bsdtar disagree about quoting, about the owner field and about
//! whether dates are localized — and would still leave the mode, uid and gid
//! approximated. The on-disk format is one 512-byte header with octal ASCII
//! fields, so reading it directly is both less code and exact.
//!
//! The layout is regular: a header, that member's bytes, enough NUL padding to
//! return to a 512-byte boundary, then the next header, with the archive closed by
//! two zero blocks. Long names arrive either as a GNU `L` header or as a pax
//! `path` record immediately before the header they modify, so both are read as a
//! preamble to the header that follows.
//!
//! An uncompressed tar is seekable, so [`scan`] records where each member's bytes
//! start and reading one later is a seek. A compressed tar is a pipe, so reading a
//! member means running the decoder again and walking to it by name.

use std::borrow::Cow;
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use ufm_core::error::{Error, Result};

use crate::command;
use crate::format::{Codec, Format};
use crate::toc::{NO_HANDLE, NodeKind, Toc, TocBuilder};
use crate::util::{epoch, epoch_secs, full_mode};
use crate::{Container, copy_out};

/// Refuse a container over this many members, the same ceiling zip applies.
const MAX_MEMBERS: usize = 200_000;
const BLOCK: usize = 512;

/// A tar container, with or without a codec around it.
pub(crate) struct TarContainer {
    path: PathBuf,
    codec: Codec,
}

const _: fn() = || {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<TarContainer>();
};

impl TarContainer {
    /// Open a tar, checking that the decompressor exists if there is one to run.
    /// Detection already decided this is a tar, so a missing codec is reported
    /// here rather than pretending the file is not an archive at all.
    pub(crate) fn new(path: PathBuf, codec: Codec) -> Result<Self> {
        if let Some(program) = codec.program() {
            if !command::available(program) {
                return Err(command::missing(program, &path));
            }
        }
        Ok(Self { path, codec })
    }

    /// Where the bytes come from: the file itself, or a decompressor's stdout.
    fn stream(&self) -> Result<Source> {
        match self.codec {
            Codec::None => Ok(Source::File(BufReader::new(File::open(&self.path)?))),
            codec => {
                let command = codec.spawn(&self.path)?;
                Ok(Source::Pipe(command::spawn(command, &self.path)?))
            }
        }
    }
}

impl Container for TarContainer {
    fn path(&self) -> &Path {
        &self.path
    }

    fn format(&self) -> Format {
        Format::Tar(self.codec)
    }

    fn scan(&self) -> Result<Arc<Toc>> {
        let mut source = self.stream()?;
        // Offsets are only worth recording when something can seek back to them.
        let toc = scan(&mut source, self.codec == Codec::None)?;
        if toc.is_empty() {
            return Err(Error::Archive(format!(
                "{}: no members found",
                self.path.display()
            )));
        }
        Ok(Arc::new(toc))
    }

    fn read(&self, toc: &Toc, node: usize, out: &mut dyn FnMut(&[u8]) -> Result<()>) -> Result<()> {
        let Some(entry) = toc.node(node) else {
            return Err(Error::Archive(format!(
                "{}: no such member",
                self.path.display()
            )));
        };
        // The header's own size is what is in the stream; a pax `size` record can
        // disagree with it, and only the stream decides how far to walk.
        let size = entry.size;
        if self.codec == Codec::None {
            let mut file = File::open(&self.path)?;
            file.seek(SeekFrom::Start(entry.handle))?;
            let mut source = Source::File(BufReader::new(file));
            return copy_out(&mut source, size, out);
        }
        let mut source = self.stream()?;
        let name = toc.path_of(node);
        skip_to_member(&mut source, &name, &self.path)?;
        copy_out(&mut source, size, out)
    }
}

/// Where the tar stream is coming from.
pub(crate) enum Source {
    /// Seekable, which is what lets a member be read at its recorded offset.
    File(BufReader<File>),
    /// A decompressor's stdout: forward only, so a member has to be walked to.
    Pipe(command::Child),
}

impl Read for Source {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Self::File(file) => file.read(buf),
            Self::Pipe(child) => child.read(buf),
        }
    }
}

/// Read the whole container into a table of contents.
///
/// `record_offsets` stores each member's data offset in its handle, which only
/// means anything for a stream the reader could seek back to.
pub(crate) fn scan(stream: &mut Source, record_offsets: bool) -> Result<Toc> {
    let mut builder = TocBuilder::with_limit(MAX_MEMBERS);
    let mut reader = BlockReader::new(stream);
    while let Some(header) = next_member(&mut reader)? {
        let handle = if record_offsets {
            reader.position()
        } else {
            NO_HANDLE
        };
        let mode = header.mode.map(|m| full_mode(header.kind, m));
        builder.push(
            &header.name,
            header.kind,
            header.size,
            header.modified,
            mode,
            header.uid,
            header.gid,
            handle,
        )?;
        reader.skip(header.data)?;
    }
    Ok(builder.finish())
}

/// Walk to a member by name, leaving the stream positioned on its bytes.
///
/// This is the compressed case, where there is no offset to seek to. The walk
/// stops at the member rather than reading the whole archive, so extracting one
/// file out of a large `.tar.gz` decompresses up to that file and no further.
fn skip_to_member(stream: &mut Source, name: &str, archive: &Path) -> Result<()> {
    let mut reader = BlockReader::new(stream);
    while let Some(header) = next_member(&mut reader)? {
        if header.name == name {
            return Ok(());
        }
        reader.skip(header.data)?;
    }
    Err(Error::Archive(format!(
        "{}: {name} is not in the archive",
        archive.display()
    )))
}

/// What a header block is, once its typeflag has been read.
#[derive(Debug, PartialEq, Eq)]
enum Role {
    /// Something to list.
    Member,
    /// A GNU `L` header: the name of the member that follows.
    LongName,
    /// A pax `x`/`X` header: records that amend the member that follows.
    Pax,
    /// A hard link, which shares its target's bytes and would list as an empty
    /// file; a volume label; a type from a later revision of the format.
    Ignore,
}

/// A header with any `L`/`x`/`X` preamble merged in.
struct Header {
    name: String,
    role: Role,
    kind: NodeKind,
    /// Size to display, which a pax `size` record may have overridden.
    size: u64,
    /// Bytes of data actually in the stream, which is what has to be skipped.
    data: u64,
    modified: Option<SystemTime>,
    mode: Option<u32>,
    uid: Option<u32>,
    gid: Option<u32>,
}

/// Read one member, consuming the preamble headers and ignorable entries that
/// belong to it. `None` at the end of the archive.
fn next_member(reader: &mut BlockReader) -> Result<Option<Header>> {
    let mut long: Option<String> = None;
    let mut pax: Option<Pax> = None;
    loop {
        let Some(mut header) = reader.header()? else {
            return Ok(None);
        };
        match header.role {
            Role::LongName => {
                let mut body = Vec::new();
                reader.read_into(header.data, &mut body)?;
                long = Some(String::from_utf8_lossy(&body).trim_end().to_string());
            }
            Role::Pax => {
                let mut body = Vec::new();
                reader.read_into(header.data, &mut body)?;
                pax = Some(parse_pax(&body));
            }
            Role::Ignore => reader.skip(header.data)?,
            Role::Member => {
                if let Some(pax) = pax.take() {
                    pax.apply_to(&mut header);
                }
                if let Some(name) = long.take() {
                    header.name = name;
                }
                return Ok(Some(header));
            }
        }
    }
}

/// pax records that can change the header that follows them.
struct Pax {
    path: Option<String>,
    size: Option<u64>,
    mtime: Option<f64>,
    uid: Option<u32>,
    gid: Option<u32>,
    mode: Option<u32>,
}

impl Pax {
    fn apply_to(self, header: &mut Header) {
        if let Some(path) = self.path {
            header.name = path;
        }
        if let Some(size) = self.size {
            header.size = size;
        }
        if let Some(at) = self.mtime.and_then(seconds) {
            header.modified = Some(at);
        }
        if let Some(uid) = self.uid {
            header.uid = Some(uid);
        }
        if let Some(gid) = self.gid {
            header.gid = Some(gid);
        }
        if let Some(mode) = self.mode {
            header.mode = Some(mode);
        }
    }
}

/// A pax timestamp, which is allowed to be fractional.
fn seconds(value: f64) -> Option<SystemTime> {
    let secs = value.floor();
    if !(0.0..=18_446_744_073_709_551_615.0).contains(&secs) {
        return None;
    }
    let nanos = ((value - secs) * 1e9).round() as u32;
    epoch(secs as u64, nanos.min(999_999_999))
}

/// Parse `<len> <key>=<value>\n` records, keeping only the ones that change what
/// is displayed. A record whose length does not match its text ends the header,
/// which is what GNU tar does with a damaged pax block rather than failing the
/// whole archive.
fn parse_pax(body: &[u8]) -> Pax {
    let mut pax = Pax {
        path: None,
        size: None,
        mtime: None,
        uid: None,
        gid: None,
        mode: None,
    };
    let text = String::from_utf8_lossy(body);
    let mut rest: &str = &text;
    while !rest.is_empty() {
        let Some(space) = rest.find(' ') else { break };
        let Ok(len) = rest[..space].parse::<usize>() else {
            break;
        };
        if len == 0 || len > rest.len() || space + 1 >= len {
            break;
        }
        let (record, tail) = rest.split_at(len);
        rest = tail;
        let record = record.trim_end_matches('\n');
        let entry = &record[space + 1..];
        let Some((key, value)) = entry.split_once('=') else {
            continue;
        };
        // A namespaced key (`SUN.mtime`, `LIBARCHIVE.xattr.compression`) is read
        // by its last component, which is how GNU, BSD and libarchive end up
        // agreeing on the same record.
        let key = key.rsplit('.').next().unwrap_or(key);
        match key {
            "path" => pax.path = Some(value.to_string()),
            "size" => pax.size = value.parse().ok(),
            "mtime" => pax.mtime = value.parse().ok(),
            "uid" => pax.uid = value.parse().ok(),
            "gid" => pax.gid = value.parse().ok(),
            "mode" => pax.mode = u32::from_str_radix(value, 8).ok(),
            _ => {}
        }
    }
    pax
}

/// Read and write a tar stream a block at a time, tracking how far along it is so
/// a member's data offset can be recorded.
struct BlockReader<'a> {
    inner: &'a mut Source,
    buffer: Box<[u8; BLOCK]>,
    /// Bytes consumed so far, which is the offset of the next byte to be read.
    position: u64,
}

impl<'a> BlockReader<'a> {
    fn new(inner: &'a mut Source) -> Self {
        Self {
            inner,
            buffer: Box::new([0; BLOCK]),
            position: 0,
        }
    }

    /// Offset of the next byte to be read, which is where the member just
    /// returned by [`header`](Self::header) has its data.
    fn position(&self) -> u64 {
        self.position
    }

    /// The next header block, `None` at the end of the archive.
    ///
    /// A zero block is ambiguous: it closes an archive, and it also pads a
    /// member's data. Only a second zero block, or the end of the stream, ends the
    /// archive, so one zero block is skipped and whatever follows is taken for a
    /// header.
    fn header(&mut self) -> Result<Option<Header>> {
        if !self.fill()? {
            return Ok(None);
        }
        if self.buffer.iter().all(|&byte| byte == 0) {
            // One zero block is a member's padding; only a second one, or the end
            // of the stream, closes the archive.
            if !self.fill()? || self.buffer.iter().all(|&byte| byte == 0) {
                return Ok(None);
            }
        }
        parse_header(&self.buffer).map(Some)
    }

    /// Step over `size` bytes of member data plus its padding.
    fn skip(&mut self, size: u64) -> Result<()> {
        self.consume(size, &mut |_| Ok(()))
    }

    /// Read `size` bytes of member data into `out`, then its padding.
    fn read_into(&mut self, size: u64, out: &mut Vec<u8>) -> Result<()> {
        self.consume(size, &mut |chunk| {
            out.extend_from_slice(chunk);
            Ok(())
        })
    }

    /// Hand `size` bytes to `emit` in block-sized pieces and consume the padding.
    ///
    /// The checksum field is deliberately not verified. A tar with one bad member
    /// is still browsable, and the byte cost of hashing every header on a
    /// 100 000-member archive buys nothing a listing needs.
    fn consume(&mut self, size: u64, emit: &mut dyn FnMut(&[u8]) -> Result<()>) -> Result<()> {
        let mut left = size;
        while left >= BLOCK as u64 {
            if !self.fill()? {
                return Err(Self::truncated());
            }
            emit(&self.buffer[..])?;
            left -= BLOCK as u64;
        }
        if left > 0 {
            if !self.fill()? {
                return Err(Self::truncated());
            }
            let take = usize::try_from(left).unwrap_or(0);
            emit(&self.buffer[..take])?;
            // The rest of the block is padding, already read and discarded.
        }
        Ok(())
    }

    /// Read one block, `false` at a clean end of stream. A partial block is a
    /// truncated archive, which is worth saying out loud: half a tar still lists.
    fn fill(&mut self) -> Result<bool> {
        let mut filled = 0;
        while filled < BLOCK {
            match self.inner.read(&mut self.buffer[filled..]) {
                Ok(0) => break,
                Ok(read) => filled += read,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e.into()),
            }
        }
        if filled == 0 {
            return Ok(false);
        }
        if filled < BLOCK {
            return Err(Self::truncated());
        }
        self.position += BLOCK as u64;
        Ok(true)
    }

    fn truncated() -> Error {
        Error::Archive(format!(
            "the archive ends in the middle of a {BLOCK}-byte block"
        ))
    }
}

/// One 512-byte header block.
fn parse_header(block: &[u8; BLOCK]) -> Result<Header> {
    // `ustar` in the magic field is what makes this a tar header rather than a
    // block of member data that happens to be block-aligned.
    if !matches!(&block[257..263], b"ustar\0" | b"ustar ") {
        return Err(Error::Archive("not a tar archive".to_string()));
    }
    let name = {
        let base = field(&block[0..100]);
        let prefix = field(&block[345..500]);
        if prefix.is_empty() {
            base.into_owned()
        } else {
            format!("{prefix}/{base}")
        }
    };
    let role = match block[156] {
        b'0' | 0 | b'7' | b'5' | b'2' | b'3' | b'4' | b'6' => Role::Member,
        b'L' => Role::LongName,
        b'x' | b'X' => Role::Pax,
        // `1` is a hard link, `V` a volume label, `D` a GNU dumpdir, `A` an AIX
        // continuation. None of them is a file the user could open, and anything
        // from a later revision of the format is skipped for the same reason.
        _ => Role::Ignore,
    };
    let kind = match block[156] {
        b'5' => NodeKind::Dir,
        b'2' => NodeKind::Symlink,
        // Character device, block device, FIFO.
        b'3' | b'4' | b'6' => NodeKind::Special,
        _ => NodeKind::File,
    };
    let size = octal(&block[124..136]).unwrap_or(0);
    Ok(Header {
        name,
        role,
        kind,
        size,
        data: size,
        modified: octal(&block[136..148])
            .and_then(|s| i64::try_from(s).ok())
            .and_then(epoch_secs),
        mode: octal(&block[100..108]).map(|m| m as u32 & 0o7777),
        uid: octal(&block[108..116]).map(|v| v as u32),
        gid: octal(&block[116..124]).map(|v| v as u32),
    })
}

/// A tar text field, which is NUL-padded rather than length-prefixed, and may not
/// be valid UTF-8 because tar stores whatever bytes the name had.
fn field(bytes: &[u8]) -> Cow<'_, str> {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end])
}

/// Read a tar numeric field: ASCII octal, space- or NUL-padded, or GNU's base-256
/// encoding for values that do not fit in eleven digits.
///
/// In base-256 the high bit of the first byte marks the encoding and the next bit
/// the sign, so a negative value — a file dated before 1970 — has no
/// [`SystemTime`] and is reported as unknown rather than clamped to the epoch.
fn octal(raw: &[u8]) -> Option<u64> {
    if raw.is_empty() {
        return None;
    }
    if raw[0] & 0x80 != 0 {
        if raw[0] & 0x40 != 0 {
            return None;
        }
        // The magnitude is big-endian across the rest of the field; only the low
        // eight bytes can fit in a u64, and anything wider is beyond what this
        // reader can list anyway.
        let bytes = &raw[1..];
        let keep = bytes.len().min(8);
        let mut value: u64 = 0;
        for &byte in &bytes[bytes.len() - keep..] {
            value = (value << 8) | u64::from(byte);
        }
        return Some(value);
    }
    let raw = field(raw);
    let text = raw.trim_matches(|c: char| c == ' ' || c == '\0');
    if text.is_empty() {
        return None;
    }
    u64::from_str_radix(text, 8).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::toc::Node;

    /// Build tar blocks by hand, so the tests do not depend on a `tar` binary and
    /// can produce the awkward cases — long names, pax headers, base-256 sizes —
    /// that are fiddly to create with a real archiver.
    #[derive(Default)]
    struct Builder {
        bytes: Vec<u8>,
    }

    impl Builder {
        fn member(&mut self, typeflag: u8, name: &str, body: &[u8], mtime: u64, mode: &str) {
            let mut block = [0u8; BLOCK];
            write_field(&mut block, 0, name.as_bytes());
            write_field(&mut block, 100, mode.as_bytes());
            // mode, uid and gid are eight-byte fields — seven digits and a
            // terminator — unlike size and mtime, which have twelve.
            write_field(&mut block, 108, format!("{:07o}", 1000u32).as_bytes());
            write_field(&mut block, 116, format!("{:07o}", 1000u32).as_bytes());
            write_field(&mut block, 124, format!("{:011o}", body.len()).as_bytes());
            write_field(&mut block, 136, format!("{mtime:011o}").as_bytes());
            block[156] = typeflag;
            write_field(&mut block, 257, b"ustar\0");
            write_field(&mut block, 263, b"00");
            self.block(&block, body);
        }

        /// A header whose optional numeric fields are left blank, for the fields
        /// that should then read as unknown.
        fn bare(&mut self, typeflag: u8, name: &str, size: usize) {
            let mut block = [0u8; BLOCK];
            write_field(&mut block, 0, name.as_bytes());
            write_field(&mut block, 124, format!("{size:011o}").as_bytes());
            block[156] = typeflag;
            write_field(&mut block, 257, b"ustar\0");
            self.block(&block, &[]);
        }

        /// A ustar header using the prefix field, the older answer to long names.
        fn prefix(&mut self, name: &str, prefix: &str, body: &[u8]) {
            let mut block = [0u8; BLOCK];
            write_field(&mut block, 0, name.as_bytes());
            write_field(&mut block, 100, b"0000644");
            write_field(&mut block, 124, format!("{:011o}", body.len()).as_bytes());
            block[156] = b'0';
            write_field(&mut block, 257, b"ustar\0");
            write_field(&mut block, 345, prefix.as_bytes());
            self.block(&block, body);
        }

        fn block(&mut self, block: &[u8; BLOCK], body: &[u8]) {
            self.bytes.extend_from_slice(block);
            self.bytes.extend_from_slice(body);
            self.bytes
                .resize(self.bytes.len().div_ceil(BLOCK) * BLOCK, 0);
        }

        fn zeros(&mut self) {
            self.bytes.extend_from_slice(&[0; BLOCK]);
        }

        fn end(&mut self) {
            self.bytes.extend_from_slice(&[0; BLOCK * 2]);
        }

        fn toc(&self) -> Result<Toc> {
            scan_file(&self.bytes)
        }

        fn handle(toc: &Toc, inner: &str) -> u64 {
            toc.node(toc.find(inner).expect(inner))
                .expect("node")
                .handle
        }
    }

    fn write_field(block: &mut [u8], at: usize, value: &[u8]) {
        let len = value.len().min(BLOCK - at);
        block[at..at + len].copy_from_slice(&value[..len]);
    }

    /// Scan a byte string by way of a real file, since offsets are recorded
    /// against a seekable stream.
    fn scan_file(bytes: &[u8]) -> Result<Toc> {
        let file = temp_tar(bytes);
        let mut source = Source::File(BufReader::new(File::open(file.path())?));
        scan(&mut source, true)
    }

    fn source_of(bytes: &[u8]) -> (tempfile::NamedTempFile, Source) {
        let file = temp_tar(bytes);
        let file_handle = File::open(file.path()).expect("open temp tar");
        (file, Source::File(BufReader::new(file_handle)))
    }

    fn temp_tar(bytes: &[u8]) -> tempfile::NamedTempFile {
        use std::io::Write;
        let mut file = tempfile::NamedTempFile::new().expect("temp file");
        file.write_all(bytes).expect("write");
        file.flush().expect("flush");
        file
    }

    fn node<'a>(toc: &'a Toc, inner: &str) -> &'a Node {
        toc.node(
            toc.find(inner)
                .unwrap_or_else(|| panic!("{inner} is listed")),
        )
        .expect("node")
    }

    #[test]
    fn a_plain_member_lists_its_metadata() {
        let mut tar = Builder::default();
        tar.member(b'0', "notes.txt", b"hello", 1_700_000_000, "0000644");
        tar.end();
        let toc = tar.toc().expect("scanned");
        let entry = node(&toc, "notes.txt");
        assert_eq!(entry.kind, NodeKind::File);
        assert_eq!(entry.size, 5);
        assert_eq!(entry.mode, Some(0o100_644));
        assert_eq!(entry.uid, Some(1000));
        assert_eq!(entry.gid, Some(1000));
        assert_eq!(
            entry
                .modified
                .expect("dated")
                .duration_since(std::time::UNIX_EPOCH)
                .expect("after epoch")
                .as_secs(),
            1_700_000_000
        );
        // The first header is at 0, so its member's bytes start one block in.
        assert_eq!(entry.handle, BLOCK as u64);
    }

    #[test]
    fn directories_and_the_parents_a_file_implies_both_appear() {
        let mut tar = Builder::default();
        tar.member(b'5', "docs/", b"", 1_700_000_000, "0000755");
        tar.member(b'0', "docs/a/b/deep.txt", b"x", 1_700_000_000, "0000644");
        tar.end();
        let toc = tar.toc().expect("scanned");
        let docs = node(&toc, "docs");
        assert_eq!(docs.kind, NodeKind::Dir);
        assert_eq!(
            docs.mode,
            Some(0o040_755),
            "the type bits come from the kind"
        );
        assert_eq!(node(&toc, "docs/a").kind, NodeKind::Dir, "implied");
        assert_eq!(node(&toc, "docs/a/b").kind, NodeKind::Dir, "implied");
        assert_eq!(node(&toc, "docs/a/b/deep.txt").kind, NodeKind::File);
        assert_eq!(toc.child_count(toc.find("docs").expect("docs")), 1);
    }

    #[test]
    fn a_gnu_long_name_replaces_the_header_name() {
        let long = format!("deep/nested/{}/file.txt", "dir".repeat(40));
        let mut tar = Builder::default();
        tar.member(b'L', "././@LongLink", long.as_bytes(), 0, "0000000");
        tar.member(b'0', &long[..99], b"data", 1_700_000_000, "0000644");
        tar.end();
        let toc = tar.toc().expect("scanned");
        assert_eq!(node(&toc, &long).size, 4, "the full name is listed once");
        assert_eq!(toc.len(), 5, "root, deep, nested, the long dir, the file");
    }

    #[test]
    fn a_ustar_prefix_is_joined_onto_the_name() {
        let mut tar = Builder::default();
        tar.prefix("leaf.txt", "top/middle", b"hi");
        tar.end();
        let toc = tar.toc().expect("scanned");
        assert_eq!(node(&toc, "top/middle/leaf.txt").size, 2);
    }

    #[test]
    fn pax_records_override_the_header_they_precede() {
        // Each record is `<len> <key>=<value>\n` with the length in bytes counting
        // the length prefix itself, and the two non-ASCII characters in the path
        // counting as two bytes each.
        let body = b"24 path=caf\xc3\xa9/\xc3\xbcber.txt\n9 size=9\n22 mtime=1600000000.5\n";
        let mut tar = Builder::default();
        tar.member(b'x', "./PaxHeaders.0/x", body, 1_700_000_000, "0000644");
        tar.member(b'0', "short.txt", b"0123456789", 1_700_000_000, "0000600");
        tar.end();
        let toc = tar.toc().expect("scanned");
        let entry = node(&toc, "café/über.txt");
        assert_eq!(entry.size, 9, "the pax size wins over the header");
        let at = entry
            .modified
            .expect("dated")
            .duration_since(std::time::UNIX_EPOCH)
            .expect("after epoch");
        assert_eq!(at.as_secs(), 1_600_000_000);
        assert_eq!(
            at.subsec_nanos(),
            500_000_000,
            "a fractional mtime keeps it"
        );
        assert_eq!(entry.mode, Some(0o100_600), "the header mode still applies");
        assert!(toc.find("short.txt").is_none(), "replaced by the pax path");
        assert_eq!(toc.len(), 3, "root, the directory, the file");
    }

    #[test]
    fn a_namespaced_pax_key_is_read_by_its_last_component() {
        let pax = parse_pax(b"16 SCHILY.dev=1\n17 SUN.mtime=123\n");
        assert_eq!(pax.mtime, Some(123.0));
        assert_eq!(pax.path, None, "nothing else was claimed");
    }

    #[test]
    fn a_damaged_pax_block_ends_the_header_rather_than_the_archive() {
        let pax = parse_pax(b"99 nonsense\n");
        assert_eq!(pax.mtime, None);
        assert_eq!(pax.mode, None);
    }

    #[test]
    fn a_hard_link_is_stepped_over_and_a_symlink_is_listed() {
        let mut tar = Builder::default();
        tar.member(b'0', "real.txt", b"abc", 1_700_000_000, "0000644");
        // A hard link names its target and carries no bytes of its own.
        tar.member(b'1', "link.txt", b"", 1_700_000_000, "0000644");
        tar.member(b'2', "symlink", b"real.txt", 1_700_000_000, "0000777");
        tar.member(b'3', "null", b"", 1_700_000_000, "0000666");
        tar.end();
        let toc = tar.toc().expect("scanned");
        assert!(toc.find("link.txt").is_none(), "a hard link is not a file");
        assert_eq!(node(&toc, "symlink").kind, NodeKind::Symlink);
        assert_eq!(node(&toc, "symlink").mode, Some(0o120_777));
        assert_eq!(node(&toc, "null").kind, NodeKind::Special);
        assert_eq!(node(&toc, "null").mode, Some(0o020_666));
    }

    #[test]
    fn an_unknown_typeflag_is_ignored_without_confusing_the_layout() {
        let mut tar = Builder::default();
        // `V` is a GNU volume label, `D` a GNU dumpdir. Neither is browsable.
        tar.member(b'V', "Volumeid", b"id", 0, "0000000");
        tar.member(b'D', "dumpdir/", b"", 0, "0000755");
        tar.member(b'0', "after.txt", b"ok", 1_700_000_000, "0000644");
        tar.end();
        let toc = tar.toc().expect("scanned");
        assert!(toc.find("Volumeid").is_none() && toc.find("dumpdir").is_none());
        assert_eq!(node(&toc, "after.txt").size, 2, "the walk stayed aligned");
    }

    #[test]
    fn blank_numeric_fields_read_as_unknown() {
        let mut tar = Builder::default();
        tar.bare(b'0', "odd.txt", 0);
        tar.end();
        let toc = tar.toc().expect("scanned");
        let entry = node(&toc, "odd.txt");
        assert_eq!(entry.size, 0);
        assert_eq!(entry.modified, None, "a blank mtime is not the epoch");
        assert_eq!(entry.mode, None, "a blank mode is not 0000");
        assert_eq!(entry.uid, None);
    }

    #[test]
    fn a_base_256_size_and_a_padded_octal_both_parse() {
        // 8 GiB does not fit in eleven octal digits, so tar switches encoding. The
        // magnitude is big-endian across the eleven bytes after the marker.
        let base256 = [0x80u8, 0, 0, 0, 0, 0, 0, 0x02, 0, 0, 0, 0];
        assert_eq!(octal(&base256), Some(8_589_934_592));

        let negative = [0xC0u8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x01];
        assert_eq!(octal(&negative), None, "a date before 1970 is unknown");

        assert_eq!(octal(b"  0000644\0\0"), Some(0o644));
        assert_eq!(octal(b"00000000000\0"), Some(0));
        assert_eq!(octal(b"           \0"), None, "blank is unknown");
        assert_eq!(octal(b"not-octal\0\0"), None);
    }

    #[test]
    fn a_truncated_member_is_reported_rather_than_listed_short() {
        let mut tar = Builder::default();
        tar.member(b'0', "big.txt", b"0123456789", 1_700_000_000, "0000644");
        // Drop the block holding the member's bytes.
        tar.bytes.truncate(tar.bytes.len() - BLOCK);
        let error = scan_file(&tar.bytes).unwrap_err();
        assert!(error.to_string().contains("middle of a"), "{error}");
    }

    #[test]
    fn a_partial_header_block_is_a_truncated_archive() {
        let mut tar = Builder::default();
        tar.member(b'0', "a.txt", b"x", 0, "0000644");
        tar.bytes.extend_from_slice(b"half a block");
        let error = scan_file(&tar.bytes).unwrap_err();
        assert!(error.to_string().contains("middle of a"), "{error}");
    }

    #[test]
    fn a_file_that_is_not_a_tar_is_reported_as_such() {
        let mut bytes = vec![0u8; BLOCK];
        bytes[..4].copy_from_slice(b"PK\x03\x04");
        let error = scan_file(&bytes).unwrap_err();
        assert!(error.to_string().contains("not a tar"), "{error}");
    }

    #[test]
    fn an_archive_with_only_the_end_marker_has_no_members() {
        let toc = scan_file(&[0u8; BLOCK * 2]).expect("an empty tar is still a tar");
        assert!(toc.is_empty(), "nothing in it");
    }

    #[test]
    fn a_single_zero_block_between_members_is_padding_not_the_end() {
        let mut tar = Builder::default();
        tar.member(b'0', "first.txt", b"a", 0, "0000644");
        tar.zeros();
        tar.member(b'0', "second.txt", b"b", 0, "0000644");
        tar.zeros();
        tar.end();
        let toc = tar.toc().expect("scanned");
        assert!(toc.find("first.txt").is_some() && toc.find("second.txt").is_some());
    }

    #[test]
    fn offsets_are_recorded_only_for_a_seekable_stream() {
        let mut tar = Builder::default();
        tar.member(b'0', "a.txt", b"abc", 0, "0000644");
        tar.member(b'0', "b.txt", b"de", 0, "0000644");
        tar.end();

        let toc = tar.toc().expect("scanned");
        assert_eq!(Builder::handle(&toc, "a.txt"), BLOCK as u64);
        assert_eq!(Builder::handle(&toc, "b.txt"), BLOCK as u64 * 3);

        let (_file, mut source) = source_of(&tar.bytes);
        let streamed = scan(&mut source, false).expect("scanned");
        assert_eq!(Builder::handle(&streamed, "a.txt"), NO_HANDLE);
    }

    #[test]
    fn a_member_is_found_by_name_in_a_stream_that_cannot_seek() {
        let mut tar = Builder::default();
        tar.member(b'0', "first.txt", b"aaaa", 0, "0000644");
        tar.member(b'5', "dir/", b"", 0, "0000755");
        tar.member(b'0', "dir/second.txt", b"bb", 0, "0000644");
        tar.end();

        let (_file, mut source) = source_of(&tar.bytes);
        skip_to_member(&mut source, "dir/second.txt", Path::new("/x/a.tar")).expect("found");
        let mut body = Vec::new();
        source.read_to_end(&mut body).expect("read");
        assert!(body.starts_with(b"bb"), "positioned on the member's bytes");

        let (_file, mut source) = source_of(&tar.bytes);
        let error = skip_to_member(&mut source, "nope.txt", Path::new("/x/a.tar")).unwrap_err();
        assert!(error.to_string().contains("not in the archive"), "{error}");
    }

    #[test]
    fn a_zero_block_is_not_mistaken_for_a_header() {
        let error = match parse_header(&[0; BLOCK]) {
            Ok(header) => panic!("a zero block is not a header: {:?}", header.role),
            Err(error) => error,
        };
        assert!(error.to_string().contains("not a tar"), "{error}");
    }
}
