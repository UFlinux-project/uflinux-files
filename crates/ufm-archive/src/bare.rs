//! A single compressed file, shown as a folder holding one member.
//!
//! `notes.txt.gz` is not a container in the sense the other backends mean: there is
//! no member list to read, only a stream. Every other file manager shows it as a
//! folder with one file inside, called `notes.txt`, and so this module synthesises
//! exactly that — a two-node table of contents, and a read that inflates the whole
//! stream.
//!
//! The size on offer is the compressed one. Getting the real length out of a gzip
//! stream means inflating it, which for a 4 GiB `.tar.gz` mistaken for a bare file
//! would be the most expensive way possible to render one row.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use ufm_core::error::{Error, Result};

use crate::Container;
use crate::command;
use crate::format::{Codec, Format};
use crate::toc::{FILE_MODE, NO_HANDLE, NodeKind, Toc, TocBuilder};

/// A compressed file with nothing inside but itself.
pub(crate) struct PlainContainer {
    path: PathBuf,
    codec: Codec,
}

const _: fn() = || {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<PlainContainer>();
};

impl PlainContainer {
    /// Refuse a file whose decompressor is not installed, so the view can say so
    /// before the user double-clicks.
    pub(crate) fn new(path: PathBuf, codec: Codec) -> Result<Self> {
        let Some(program) = codec.program() else {
            return Err(Error::Archive(format!(
                "{} is not a compressed file",
                path.display()
            )));
        };
        if !command::available(program) {
            return Err(command::missing(program, &path));
        }
        Ok(Self { path, codec })
    }
}

impl Container for PlainContainer {
    fn path(&self) -> &Path {
        &self.path
    }

    fn format(&self) -> Format {
        Format::Bare(self.codec)
    }

    /// One member, named after the file without its suffix. Nothing is read from
    /// the file beyond what the caller already stat'ed, so a 4 GiB `.gz` lists as
    /// fast as a 4 KiB one.
    fn scan(&self) -> Result<Arc<Toc>> {
        let meta = std::fs::metadata(&self.path)?;
        let mut builder = TocBuilder::with_limit(2);
        builder.push(
            &Format::bare_member_name(&self.path),
            NodeKind::File,
            meta.len(),
            meta.modified().ok(),
            Some(FILE_MODE),
            None,
            None,
            NO_HANDLE,
        )?;
        Ok(Arc::new(builder.finish()))
    }

    /// Inflate the whole file. There is no length to stop at — the compressed size
    /// says nothing about the uncompressed one — so this reads to end of stream.
    fn read(
        &self,
        _toc: &Toc,
        _node: usize,
        out: &mut dyn FnMut(&[u8]) -> Result<()>,
    ) -> Result<()> {
        let mut child = command::spawn(self.codec.spawn(&self.path)?, &self.path)?;
        let mut source = std::io::BufReader::new(&mut child);
        let mut buffer = vec![0u8; crate::CHUNK];
        loop {
            use std::io::Read;
            let read = match source.read(&mut buffer) {
                Ok(read) => read,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e.into()),
            };
            if read == 0 {
                break;
            }
            out(&buffer[..read])?;
        }
        drop(source);
        child.finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Replace `path`'s contents with the gzip stream of `body`, so the container
    /// under test is a real `.gz` and not a hand-written guess at one.
    fn gzip(path: &Path, body: &[u8]) {
        std::fs::write(path, body).expect("written");
        let mut command = command::command("gzip");
        command.args(["-c", "--"]).arg(path);
        let packed = command::output(&mut command, path, "gzip").expect("gzipped");
        std::fs::write(path, packed).expect("written");
    }

    #[test]
    fn a_compressed_file_lists_one_member_named_after_itself() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let path = dir.path().join("notes.txt.gz");
        gzip(&path, b"body");
        let container = PlainContainer::new(path.clone(), Codec::Gzip).expect("gzip is installed");
        let toc = container.scan().expect("listed");
        assert_eq!(toc.len(), 2, "the root and the one member");
        let at = toc.find("notes.txt").expect("the suffix is dropped");
        assert_eq!(toc.node(at).map(|n| n.kind), Some(NodeKind::File));
        assert_eq!(container.format(), Format::Bare(Codec::Gzip));
    }

    #[test]
    fn reading_inflates_the_stream() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let path = dir.path().join("notes.txt.gz");
        gzip(&path, b"inflate me");
        let container = PlainContainer::new(path.clone(), Codec::Gzip).expect("gzip is installed");
        let toc = container.scan().expect("listed");
        let at = toc.find("notes.txt").expect("member");
        let mut body = Vec::new();
        container
            .read(&toc, at, &mut |chunk| {
                body.extend_from_slice(chunk);
                Ok(())
            })
            .expect("read");
        assert_eq!(body, b"inflate me");
    }

    #[test]
    fn an_uncompressed_file_is_not_a_container_here() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let path = dir.path().join("plain.txt");
        std::fs::write(&path, b"x").expect("write");
        let error = match PlainContainer::new(path, Codec::None) {
            Ok(_) => panic!("a plain file is not a container"),
            Err(error) => error,
        };
        assert!(
            error.to_string().contains("not a compressed file"),
            "{error}"
        );
    }
}
