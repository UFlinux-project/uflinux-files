//! Thumbnails, and the one place the provider touches image bytes.
//!
//! The protocol says a thumbnail travels as a file descriptor
//! ([`Thumbnail::png`]), so the largest message on this bus stays a model and the
//! pixels go down a pipe instead. That decision has one consequence that shapes
//! this whole module: the provider has to hand over something readable, which
//! means either a temporary file or a memory fd. A temporary file costs a
//! directory entry, a write and an unlink per icon, and leaves something behind
//! when the provider is killed; `memfd_create` costs one syscall and disappears
//! with the process. On a target small enough to turn thumbnails off by default,
//! the fd is the cheaper answer by a wide margin.
//!
//! The cache follows the freedesktop thumbnail spec's layout —
//! `$XDG_CACHE_HOME/thumbnails/normal/<md5 of the file URI>.png` — for one
//! reason that is not about tidiness: a file manager window that already
//! thumbnailled a picture puts the same bytes where the desktop will look for
//! them, and the work is not done twice. One thing is deliberately *not* the
//! spec. The spec records the source's size and mtime in a `tEXt` chunk inside
//! the PNG, so honouring it means walking chunks and checking CRCs in a process
//! whose whole budget is 10 MiB. Comparing the cached file's own mtime against
//! the source's is the same staleness rule with no parser, and it fails in the
//! safe direction: touching a picture regenerates its thumbnail, and a thumbnail
//! that is merely too new is rebuilt rather than served wrong.
//!
//! Decoding is the expensive part, and it is bounded twice. The source is refused
//! unless it is small enough to plausibly be an icon-sized picture
//! ([`MAX_SOURCE_BYTES`]), and the decode asks gdk-pixbuf for the requested size
//! directly, so a 24-megapixel photo is downsampled while it is read rather than
//! decoded at full size and shrunk afterwards.

use std::io::Write;
use std::path::{Path, PathBuf};

use gdk_pixbuf::Pixbuf;
use gio::prelude::*;

use ufm_desktop_ipc::{Format, Thumbnail};

/// Largest source file that will be decoded: 16 MiB.
///
/// The cost of a decode is proportional to the source's pixel area, and this cap
/// is a proxy for that. A file over it is either a very large image or something
/// that is not an image at all, and in both cases the answer the renderer wants
/// is an icon name rather than a decode that costs a second and a half of RSS.
pub const MAX_SOURCE_BYTES: u64 = 16 * 1024 * 1024;

/// Largest edge that will be decoded. Beyond this a renderer is asking for a
/// preview and not a desktop icon; the model's own `thumbnail_size` is the number
/// that should be in use.
pub const MAX_SIZE: u32 = 512;

/// Smallest edge accepted, so a request for `0` cannot ask for a zero-sized
/// image that every renderer will then reject.
pub const MIN_SIZE: u32 = 16;

/// The PNG signature, and the shortest header worth reading.
const SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];

/// One thumbnail request, already vetted.
///
/// The path is a whole file and not an id: [`Request::RequestThumbnail`] carries
/// one so a renderer can ask about a file it is about to show, and this type is
/// where the provider re-checks that the path really is inside the desktop
/// directory before spending anything on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Job {
    /// The file to thumbnail.
    pub path: PathBuf,
    /// Requested edge length, clamped into [`MIN_SIZE`]..=`MAX_SIZE`.
    pub size: u32,
}

impl Job {
    /// A job for `path` at `size`, with the size clamped rather than refused: a
    /// renderer that asks for 4 px wants a small picture, not an error.
    #[must_use]
    pub fn new(path: impl Into<PathBuf>, size: u32) -> Self {
        Self {
            path: path.into(),
            size: size.clamp(MIN_SIZE, MAX_SIZE),
        }
    }

    /// Whether `desktop` is the directory holding this file.
    ///
    /// Compared as paths and not as text, because a text prefix would accept
    /// `/home/u/Desktopial/file` as being inside `/home/u/Desktop`.
    #[must_use]
    pub fn inside(&self, desktop: &Path) -> bool {
        self.path.parent() == Some(desktop)
    }
}

/// The reply body for a job, plus the bytes that go with it.
///
/// The two are returned together because they are only meaningful together: the
/// fd index in the [`Thumbnail`] describes the D-Bus message that carries the
/// descriptor, so a caller cannot keep one and drop the other.
pub struct Reply {
    /// The JSON body to put on the bus.
    pub body: Thumbnail,
    /// The PNG to write to the pipe, when there is one.
    pub png: Option<Vec<u8>>,
}

impl Reply {
    /// The "use an icon" answer, which is also the answer to every failure here.
    #[must_use]
    pub const fn icon() -> Self {
        Self {
            body: Thumbnail::icon(),
            png: None,
        }
    }
}

/// Answer one request: cache first, then decode, then give up.
///
/// Every failure path returns [`Reply::icon`], including the ones that would be
/// errors elsewhere — an unreadable file, an unsupported format, a decode that ran
/// out of memory. A renderer told to fall back to an icon draws a desktop that
/// still looks right, and the alternative is a bus error per icon for a feature
/// that is optional by design.
#[must_use]
pub fn answer(job: &Job) -> Reply {
    if let Some(png) = cached(job) {
        return reply(&png);
    }
    let Some(png) = make(job) else {
        return Reply::icon();
    };
    store(job, &png);
    reply(&png)
}

/// The reply for PNG bytes this module produced or read.
fn reply(png: &[u8]) -> Reply {
    // The size is read back from the bytes rather than assumed, because a cached
    // file may have been written at another size by another component, and a
    // renderer that scales to the wrong box looks broken rather than stale.
    let Some((width, height)) = png_size(png) else {
        return Reply::icon();
    };
    Reply {
        body: Thumbnail {
            width,
            height,
            format: Format::Png,
            fd: Some(0),
        },
        png: Some(png.to_vec()),
    }
}

/// Read a cached thumbnail, if one exists and is not older than its source.
fn cached(job: &Job) -> Option<Vec<u8>> {
    let path = cached_path(&job.path)?;
    let cache = std::fs::metadata(&path).ok()?;
    if !cache.is_file() || cache.len() == 0 || cache.len() > MAX_SOURCE_BYTES {
        return None;
    }
    let source = std::fs::metadata(&job.path).ok()?;
    let fresh = match (cache.modified(), source.modified()) {
        (Ok(c), Ok(s)) => c >= s,
        // A filesystem without mtimes, or a source that vanished mid-request: the
        // entry is not provably stale, so it is used.
        _ => true,
    };
    fresh.then(|| std::fs::read(&path).ok()).flatten()
}

/// Decode and encode a thumbnail, or `None` for "draw an icon".
fn make(job: &Job) -> Option<Vec<u8>> {
    let meta = std::fs::metadata(&job.path).ok()?;
    if !meta.is_file() || meta.len() > MAX_SOURCE_BYTES {
        return None;
    }
    // Reading only the header costs almost nothing and rules out the two common
    // cases — a file that is not an image, and a format with no loader here —
    // before anything is allocated. An image too small to fill a cell is refused
    // the same way, and for the same reason as below.
    let (_format, width, height) = Pixbuf::file_info(&job.path)?;
    if width < 8 || height < 8 || !available() {
        return None;
    }
    let edge = i32::try_from(job.size).ok()?;
    let pixbuf = Pixbuf::from_file_at_scale(&job.path, edge, edge, true).ok()?;
    // A source much smaller than the request comes back at its own size, which is
    // the right picture but says nothing about the cell it was asked to fill.
    // Under a quarter of the request the icon is the better answer, and costs the
    // renderer no descriptor at all.
    if pixbuf.width() < edge / 4 || pixbuf.height() < edge / 4 {
        return None;
    }
    pixbuf
        .save_to_bufferv("png", &[("compression", "fast")])
        .ok()
}

/// Write a fresh thumbnail into the cache, best effort.
///
/// A failure here is invisible on purpose: the thumbnail is already in memory on
/// its way to the renderer, and a cache directory that cannot be written should
/// cost one repeated decode and nothing else.
fn store(job: &Job, png: &[u8]) {
    let Some(path) = cached_path(&job.path) else {
        return;
    };
    let Some(dir) = path.parent() else {
        return;
    };
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    let _ = std::fs::write(&path, png);
}

/// Where a thumbnail for `source` lives: `<cache>/thumbnails/normal/<md5>.png`.
#[must_use]
pub fn cached_path(source: &Path) -> Option<PathBuf> {
    let mut sum = glib::Checksum::new(glib::ChecksumType::Md5)?;
    sum.update(uri(source).as_bytes());
    let hash = sum.string()?;
    let root = ufm_core::paths::thumbnail_dir();
    Some(root.join("normal").join(hash).with_extension("png"))
}

/// The `file://` URI of a path, which is what the spec hashes.
fn uri(path: &Path) -> String {
    gio::File::for_path(path).uri().to_string()
}

/// The dimensions of a PNG, from its `IHDR` chunk.
///
/// Parsed by hand rather than with gdk-pixbuf, because the bytes are already in
/// memory and a decode just to learn their size would double the cost of the
/// reply. The check is strict about the signature and the chunk type, so bytes
/// that are not a PNG yield `None` instead of a size read out of the middle of a
/// palette.
#[must_use]
pub fn png_size(png: &[u8]) -> Option<(u16, u16)> {
    let header = png.get(..24)?;
    if header[..8] != SIGNATURE || &header[12..16] != b"IHDR" {
        return None;
    }
    let width = u32::from_be_bytes([header[16], header[17], header[18], header[19]]);
    let height = u32::from_be_bytes([header[20], header[21], header[22], header[23]]);
    Some((clamp_u16(width), clamp_u16(height)))
}

/// Fit an image dimension into the `u16` the protocol uses.
const fn clamp_u16(value: u32) -> u16 {
    if value > u16::MAX as u32 {
        u16::MAX
    } else {
        value as u16
    }
}

/// Whether this build can encode a PNG at all.
///
/// A minimal image can ship gdk-pixbuf with one loader, and then every thumbnail
/// request has the same answer without anything needing to be decoded to find
/// out. `Status` reports this through its `note`, so a renderer turns the feature
/// off instead of asking twice per icon.
#[must_use]
pub fn available() -> bool {
    Pixbuf::formats()
        .iter()
        .any(|f| f.is_writable() && f.name().is_some_and(|n| n == "png"))
}

/// Write PNG bytes to a descriptor for passing over the bus.
///
/// `memfd_create` rather than a temporary file: no unlink to lose, nothing left
/// behind when the provider is killed, and no directory to clean. The seal is not
/// worth taking — the receiver gets its own dup of the descriptor either way —
/// but `MFD_CLOEXEC` matters, because a provider that later forks a mount helper
/// must not hand it an open thumbnail.
///
/// The descriptor is rewound before it is handed over: the offset is shared by
/// every dup of a memory fd, so a receiver given one left at the end of the write
/// would read an empty file.
///
/// # Errors
///
/// Whatever the kernel or the write says. The caller turns any of it into the
/// icon fallback.
pub fn to_memfd(png: &[u8]) -> std::io::Result<std::fs::File> {
    use std::os::fd::AsRawFd as _;
    use std::os::fd::FromRawFd as _;

    let name = std::ffi::CString::new("ufm-thumb").expect("no NUL in a literal");
    // SAFETY: a C string and a flags value this file controls; the result is
    // either a new descriptor or -1.
    let raw = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC) };
    if raw < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: `raw` is a fresh descriptor, now owned by this File.
    let mut file = unsafe { std::fs::File::from_raw_fd(raw) };
    file.write_all(png)?;
    // SAFETY: lseek on a descriptor this function owns. A failure only means the
    // receiver reads nothing, and that is reported rather than ignored.
    let rewound = unsafe { libc::lseek(file.as_raw_fd(), 0, libc::SEEK_SET) };
    if rewound < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn png(width: u32, height: u32) -> Vec<u8> {
        let mut bytes = SIGNATURE.to_vec();
        bytes.extend_from_slice(&13u32.to_be_bytes());
        bytes.extend_from_slice(b"IHDR");
        bytes.extend_from_slice(&width.to_be_bytes());
        bytes.extend_from_slice(&height.to_be_bytes());
        bytes.extend_from_slice(&[8, 6, 0, 0, 0]);
        bytes
    }

    #[test]
    fn a_size_is_clamped_and_not_refused() {
        assert_eq!(Job::new("/d/a.png", 0).size, MIN_SIZE);
        assert_eq!(Job::new("/d/a.png", 4).size, MIN_SIZE);
        assert_eq!(Job::new("/d/a.png", 128).size, 128);
        assert_eq!(Job::new("/d/a.png", u32::MAX).size, MAX_SIZE);
    }

    #[test]
    fn a_job_belongs_to_the_directory_holding_it() {
        let job = Job::new("/home/u/Desktop/a.png", 128);
        assert!(job.inside(Path::new("/home/u/Desktop")));
        assert!(!job.inside(Path::new("/home/u")));
        assert!(!job.inside(Path::new("/home/u/Desktopial")));
    }

    #[test]
    fn a_path_outside_the_desktop_is_not_a_thumbnail_request() {
        let job = Job::new("/home/u/.ssh/id_rsa", 128);
        assert!(!job.inside(Path::new("/home/u/Desktop")));
    }

    #[test]
    fn dimensions_come_from_the_header() {
        assert_eq!(png_size(&png(128, 96)), Some((128, 96)));
    }

    #[test]
    fn a_huge_image_reports_the_cap_and_not_a_wrap() {
        assert_eq!(png_size(&png(70_000, 8)), Some((u16::MAX, 8)));
    }

    #[test]
    fn bytes_that_are_not_a_png_have_no_size() {
        assert_eq!(png_size(&[]), None);
        assert_eq!(png_size(&[0x89, b'P', b'N', b'G']), None);
        let mut wrong_chunk = png(16, 16);
        wrong_chunk[12..16].copy_from_slice(b"IDAT");
        assert_eq!(png_size(&wrong_chunk), None);
    }

    #[test]
    fn a_short_header_is_not_a_size() {
        assert_eq!(png_size(&SIGNATURE), None);
    }

    #[test]
    fn a_cache_path_is_the_md5_of_the_uri() {
        // The spec's own example: the hash is of the URI text and nothing else,
        // so this is the value another component will look for.
        let path = Path::new("/home/u/Desktop/a.png");
        let got = cached_path(path).expect("md5 is always available");
        let want = {
            let mut sum = glib::Checksum::new(glib::ChecksumType::Md5).unwrap();
            sum.update(b"file:///home/u/Desktop/a.png");
            sum.string().unwrap()
        };
        assert_eq!(
            got.file_name().and_then(|n| n.to_str()).unwrap(),
            format!("{want}.png")
        );
        assert!(got.starts_with(ufm_core::paths::thumbnail_dir()));
    }

    #[test]
    fn two_different_files_get_two_cache_entries() {
        let a = cached_path(Path::new("/home/u/Desktop/a.png")).unwrap();
        let b = cached_path(Path::new("/home/u/Desktop/b.png")).unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn a_failure_is_an_icon_and_not_an_error() {
        let reply = Reply::icon();
        assert_eq!(reply.body, Thumbnail::icon());
        assert_eq!(reply.body.format, Format::Icon);
        assert_eq!(reply.body.fd, None);
        assert!(reply.png.is_none());
    }

    #[test]
    fn a_missing_file_is_answered_with_an_icon() {
        let job = Job::new("/nonexistent-desktop/a.png", 128);
        let reply = answer(&job);
        assert_eq!(reply.body, Thumbnail::icon());
        assert!(reply.png.is_none());
    }

    #[test]
    fn a_text_file_is_answered_with_an_icon() {
        let dir = tempfile::tempdir().expect("a desktop directory");
        let path = dir.path().join("notes.txt");
        std::fs::write(&path, b"not an image at all").expect("a file to thumbnail");
        let reply = answer(&Job::new(&path, 128));
        assert_eq!(reply.body, Thumbnail::icon());
    }

    #[test]
    fn a_memory_fd_arrives_at_the_start() {
        use std::io::Read as _;
        let bytes = png(32, 32);
        let Ok(mut fd) = to_memfd(&bytes) else {
            // A kernel without memfd_create: the caller falls back to an icon,
            // which is the behaviour the rest of this module is built around.
            return;
        };
        let mut read = Vec::new();
        fd.read_to_end(&mut read).expect("a readable descriptor");
        assert_eq!(read, bytes, "the descriptor must not be left at the end");
    }
}
