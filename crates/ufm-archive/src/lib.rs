//! Browsing inside archive files.
//!
//! An archive is a folder that cannot be written to, and the design follows from
//! that: the same [`Entry`] values, the same streaming contract, the same sorting
//! and filtering as a real directory, with a read-only badge in the path bar and no
//! create, rename or delete anywhere in the views. Nothing above this crate needs to
//! know that a container is involved, which is what keeps the columns view, the
//! search and the properties dialog from growing archive-specific branches.
//!
//! Three formats, chosen for what a home directory actually contains:
//!
//! - **zip** through the `zip` crate, the one format with random access, so opening
//!   a 4 GiB container costs one seek.
//! - **tar** parsed from its own bytes, uncompressed or behind the system
//!   `gzip`/`bzip2`/`xz`/`zstd`, which keeps four decompressors out of the resident
//!   set.
//! - **7z** through the `7z` command-line tool, because the Rust readers for it are
//!   either unmaintained or need a newer compiler than this workspace targets.
//!
//! A container is scanned once, into a [`Toc`]: a flat array of nodes with the
//! children of each node gathered into contiguous ranges. That layout is what makes
//! listing a folder inside a large archive fast — no reparse, no re-decompress, and
//! a directory with 100 000 siblings is a binary search per lookup. The result is
//! cached, keyed by the container's path, size and mtime, so a browse back to a
//! folder the user just left costs nothing and a container that changed on disk is
//! re-read rather than served stale.
//!
//! Reading a member's bytes always streams: a 2 GiB file inside an archive must not
//! turn into a 2 GiB allocation in a process with a 90 MiB budget.

#![deny(unsafe_code)]

mod bare;
mod command;
mod format;
mod reader;
mod sevenz;
mod tar;
mod toc;
mod util;
mod zip;

pub use format::{Codec, Format};
pub use reader::{ArchiveReader, Opened};
pub use toc::{Cache, DIR_MODE, FILE_MODE, Key, NO_HANDLE, Node, NodeKind, Toc, TocBuilder};

use std::io::Read;
use std::path::Path;
use std::sync::Arc;

use ufm_core::error::Result;

/// How many bytes of member data are handed to a reader at a time.
///
/// The same size the directory listing uses, so one buffer serves both and a
/// large member costs a buffer rather than a copy of itself.
const CHUNK: usize = ufm_vfs::CHUNK;

/// A container this build can list and read.
///
/// Every method is `&self` and the trait is `Send + Sync`: listings happen on
/// worker threads, and two columns of the same archive can be listed at once.
pub(crate) trait Container: Send + Sync {
    /// The file on disk, for error messages and for the cache key.
    fn path(&self) -> &Path;

    /// What kind of container this turned out to be, which the reader reports in
    /// the properties dialog.
    fn format(&self) -> Format;

    /// Read the whole member list. Called once per container, then cached.
    fn scan(&self) -> Result<Arc<Toc>>;

    /// Stream one member's bytes, in chunks of at most [`CHUNK`].
    ///
    /// `out` sees the decompressed data and can abort the read by returning an
    /// error, which is how cancellation reaches a format whose own reading is a
    /// blocking loop.
    fn read(&self, toc: &Toc, node: usize, out: &mut dyn FnMut(&[u8]) -> Result<()>) -> Result<()>;
}

/// Hand `size` bytes of `source` to `out`, a chunk at a time.
///
/// Shared by the formats that end up streaming from a pipe, so the buffer is
/// written once per format rather than once per call site.
fn copy_out<R: Read>(
    source: &mut R,
    size: u64,
    out: &mut dyn FnMut(&[u8]) -> Result<()>,
) -> Result<()> {
    let mut buffer = vec![0u8; CHUNK];
    let mut left = size;
    while left > 0 {
        let want = std::cmp::min(left, CHUNK as u64) as usize;
        let read = match source.read(&mut buffer[..want]) {
            Ok(read) => read,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.into()),
        };
        if read == 0 {
            break;
        }
        left -= read as u64;
        out(&buffer[..read])?;
    }
    Ok(())
}
