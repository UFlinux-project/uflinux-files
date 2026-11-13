//! zip, through the `zip` crate, which reads the central directory directly.
//!
//! This is the one format with random access: the central directory at the end of
//! the file says where every member's header is, so listing a container does not
//! touch a single byte of member data and opening a folder inside it costs one
//! seek. That is why a 4 GiB `.zip` opens as fast as a 4 KiB one, and why this
//! backend holds a `ZipArchive` open instead of rescanning.
//!
//! The archive handle is behind a mutex rather than cloned per read, because
//! `ZipArchive` owns the file and its cached directory; two columns of the same
//! container being listed at once is normal navigation, and serialising on one
//! handle is cheaper than parsing the directory twice.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use ufm_core::error::{Error, Result};
use zip::ZipArchive;

use crate::format::Format;
use crate::toc::{DIR_MODE, FILE_MODE, NO_HANDLE, NodeKind, Toc, TocBuilder};
use crate::util::days_from_civil;
use crate::{CHUNK, Container};

/// Refuse a container over this many members. A directory listing that big is
/// past useful anyway, and the cap is what keeps a pathological archive from
/// turning a browse into an out-of-memory.
const MAX_MEMBERS: usize = 200_000;

/// An open zip, ready to list and to read members from.
pub(crate) struct ZipContainer {
    path: PathBuf,
    /// Held open so listing is a seek rather than a reparse. Every read of member
    /// data goes through here too, which is why it is a mutex and not a snapshot.
    archive: Mutex<ZipArchive<File>>,
}

const _: fn() = || {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<ZipContainer>();
};

impl ZipContainer {
    /// Read the central directory. The cost is proportional to the number of
    /// members, not to the size of the archive.
    pub(crate) fn new(path: PathBuf) -> Result<Self> {
        let file = File::open(&path)?;
        let archive = ZipArchive::new(file)
            .map_err(|e| Error::Archive(format!("{}: {e}", path.display())))?;
        Ok(Self {
            path,
            archive: Mutex::new(archive),
        })
    }
}

impl Container for ZipContainer {
    fn path(&self) -> &Path {
        &self.path
    }

    fn format(&self) -> Format {
        Format::Zip
    }

    fn scan(&self) -> Result<Arc<Toc>> {
        // The lock is held for the whole scan: another thread scanning the same
        // container would parse the same directory twice for the same answer.
        let archive = self
            .archive
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let toc = scan(&archive, &self.path)?;
        Ok(Arc::new(toc))
    }

    fn read(&self, toc: &Toc, node: usize, out: &mut dyn FnMut(&[u8]) -> Result<()>) -> Result<()> {
        let index = usize::try_from(toc.node(node).map_or(NO_HANDLE, |node| node.handle))
            .map_err(|_| Error::Archive(format!("{}: no such member", self.path.display())))?;
        let mut archive = self
            .archive
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // `by_index` decompresses on demand, so a 2 GiB member costs a buffer and
        // not a vector.
        let mut entry = archive
            .by_index(index)
            .map_err(|e| Error::Archive(format!("{}: {e}", self.path.display())))?;
        let mut buffer = vec![0u8; CHUNK];
        loop {
            let read = std::io::Read::read(&mut entry, &mut buffer)?;
            if read == 0 {
                break;
            }
            out(&buffer[..read])?;
        }
        Ok(())
    }
}

/// Build the table of contents from the central directory.
///
/// Metadata only: `zip` exposes the directory without decompressing anything, so
/// this is the whole cost of opening the archive.
fn scan(archive: &ZipArchive<File>, path: &Path) -> Result<Toc> {
    let broken = |e: zip::result::ZipError| Error::Archive(format!("{}: {e}", path.display()));
    let metadata = archive.metadata();
    let mut builder = TocBuilder::with_limit(MAX_MEMBERS);
    for index in 0..metadata.len() {
        let entry = metadata.entry(index).map_err(broken)?;
        // `enclosed_name` rejects absolute paths, embedded NULs, `..` and
        // backslash-separated names, which is exactly the set of names that would
        // otherwise let a member escape the archive root.
        let Some(inner) = entry.enclosed_name() else {
            continue;
        };
        let Some(path) = inner.to_str() else {
            continue;
        };
        let dir = entry.is_dir();
        let kind = if dir {
            NodeKind::Dir
        } else if entry.is_symlink() {
            NodeKind::Symlink
        } else {
            NodeKind::File
        };
        // A zip written by a tool that does not store Unix modes has no
        // permissions at all; synthesising them keeps the properties dialog from
        // showing nothing rather than showing something wrong.
        let mode = entry
            .unix_mode()
            .or(if dir { Some(DIR_MODE) } else { Some(FILE_MODE) });
        builder.push(
            path,
            kind,
            entry.size(),
            system_time(entry.last_modified()),
            mode,
            None,
            None,
            u64::try_from(index).unwrap_or(u64::MAX),
        )?;
    }
    Ok(builder.finish())
}

/// zip stores a local date, without a zone, in a packed form. The spec's epoch is
/// 1980, so anything the crate hands back is at least that; it is read as UTC,
/// which is what every other file manager does with a zip timestamp and the only
/// answer that does not change when the user crosses a timezone.
fn system_time(at: Option<zip::DateTime>) -> Option<SystemTime> {
    let at = at?;
    let days = days_from_civil(
        u32::from(at.year()),
        i64::from(at.month()),
        i64::from(at.day()),
    )?;
    let secs = days * 86_400
        + u64::from(at.hour()) * 3_600
        + u64::from(at.minute()) * 60
        + u64::from(at.second());
    UNIX_EPOCH.checked_add(std::time::Duration::from_secs(secs))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_zip_timestamp_becomes_the_right_instant() {
        let at = zip::DateTime::from_date_and_time(2026, 11, 13, 16, 25, 48).expect("in range");
        let at = system_time(Some(at)).expect("convertible");
        assert_eq!(
            at.duration_since(UNIX_EPOCH)
                .expect("after epoch")
                .as_secs(),
            1_794_587_148,
            "checked against date -u -d 2026-11-13T16:25:48"
        );
    }

    #[test]
    fn the_zip_epoch_is_1980() {
        let at = zip::DateTime::from_date_and_time(1980, 1, 1, 0, 0, 0).expect("in range");
        let at = system_time(Some(at)).expect("convertible");
        assert_eq!(
            at.duration_since(UNIX_EPOCH)
                .expect("after epoch")
                .as_secs(),
            315_532_800
        );
    }

    #[test]
    fn a_missing_or_out_of_range_date_is_unknown() {
        assert_eq!(system_time(None), None);
    }

    #[test]
    fn a_real_container_round_trips_when_the_writer_is_available() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let path = dir.path().join("mixed.zip");
        write(&path);

        let container = ZipContainer::new(path.clone()).expect("opened");
        assert_eq!(container.format(), Format::Zip);
        let toc = container.scan().expect("scanned");
        assert_eq!(toc.len(), 4, "root, docs/, docs/a.txt, note.txt");
        assert_eq!(container.path(), path.as_path());
        let at = toc
            .node(toc.find("docs/a.txt").expect("listed"))
            .expect("node");
        assert_eq!(at.size, 6);
        assert_eq!(at.handle, 1, "the central-directory index is the handle");
        assert_eq!(toc.total_bytes, 6 + 4);

        let mut body = Vec::new();
        container
            .read(
                &toc,
                toc.find("docs/a.txt").expect("listed"),
                &mut |chunk| {
                    body.extend_from_slice(chunk);
                    Ok(())
                },
            )
            .expect("read");
        assert_eq!(body, b"inside");
    }

    #[test]
    fn a_file_that_is_not_a_zip_is_reported() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let path = dir.path().join("junk.zip");
        std::fs::write(&path, b"not an archive at all").expect("write");
        let error = match ZipContainer::new(path) {
            Ok(_) => panic!("a text file is not a zip"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("junk.zip"), "{error}");
    }

    fn write(path: &Path) {
        use zip::write::SimpleFileOptions;
        let file = File::create(path).expect("create");
        let mut writer = zip::ZipWriter::new(file);
        let options = SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated)
            .last_modified_time(
                zip::DateTime::from_date_and_time(2026, 11, 13, 16, 25, 48).expect("in range"),
            );
        writer.add_directory("docs/", options).expect("directory");
        writer.start_file("docs/a.txt", options).expect("file");
        std::io::Write::write_all(&mut writer, b"inside").expect("write");
        writer.start_file("note.txt", options).expect("file");
        std::io::Write::write_all(&mut writer, b"note").expect("write");
        writer.finish().expect("finish");
    }
}
