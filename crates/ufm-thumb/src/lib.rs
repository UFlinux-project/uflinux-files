//! Thumbnails: the cache key, the staleness rule, and the decode.
//!
//! This lives in a crate rather than in one binary because two components draw
//! the same pictures. The desktop provider paints icons for `uflinux-desktop`
//! and the file manager paints icons for a folder; if they computed different
//! cache paths, a picture the desktop already paid for would be decoded again
//! the moment the same folder was opened, which is the opposite of what a cache
//! is for. The rules are here so that both get the same answer for the same
//! file, and so that there is one place where the bounds on a decode are set.
//!
//! The cache follows the freedesktop thumbnail spec's layout —
//! `$XDG_CACHE_HOME/thumbnails/normal/<md5 of the file URI>.png` — for one
//! reason that is not about tidiness: a file manager window that already
//! thumbnailled a picture puts the same bytes where the desktop, and every other
//! conforming component, will look for them. One thing is deliberately *not* the
//! spec. The spec records the source's size and mtime in a `tEXt` chunk inside
//! the PNG, so honouring it means walking chunks and checking CRCs in a process
//! whose whole budget is 10 MiB. Comparing the cached file's own mtime against
//! the source's is the same staleness rule with no parser, and it fails in the
//! safe direction: touching a picture regenerates its thumbnail, and a thumbnail
//! that is merely too new is rebuilt rather than served wrong.
//!
//! Decoding is the expensive part, and it is bounded twice. The source is refused
//! unless it is small enough to plausibly be an icon-sized picture
//! ([`MAX_SOURCE_BYTES`]), and the decode asks `GdkPixbuf` for the requested size
//! directly, so a 24-megapixel photo is downsampled while it is read rather than
//! decoded at full size and shrunk afterwards.
//!
//! Nothing here knows about the main loop or about widgets. [`bytes`] is a
//! blocking call, and a caller that cannot afford to block calls it from a
//! worker thread and gets back owned bytes to paint from.

use std::io::Write;
use std::path::{Path, PathBuf};

use gdk_pixbuf::Pixbuf;
use gio::prelude::*;

/// Largest source file that will be decoded: 16 MiB.
///
/// The cost of a decode is proportional to the source's pixel area, and this cap
/// is a proxy for that. A file over it is either a very large image or something
/// that is not an image at all, and in both cases the answer a caller wants is a
/// fallback icon rather than a decode that costs a second and a half of RSS.
pub const MAX_SOURCE_BYTES: u64 = 16 * 1024 * 1024;

/// Largest edge that will be decoded. Beyond this a caller is asking for a
/// preview and not an icon; the size a view actually drew is the number that
/// should be in use.
pub const MAX_SIZE: u32 = 512;

/// Smallest edge accepted, so a request for `0` cannot ask for a zero-sized
/// image that every renderer will then reject.
pub const MIN_SIZE: u32 = 16;

/// The PNG signature, and the shortest header worth reading.
const SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];

/// One thumbnail request, already vetted.
///
/// The path is a whole file and not an id: a caller asks about the file it is
/// about to show, and this type is where the size is clamped so that no caller
/// can ask for a picture with no edge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Job {
    /// The file to thumbnail.
    pub path: PathBuf,
    /// Requested edge length, clamped into [`MIN_SIZE`]..=`MAX_SIZE`.
    pub size: u32,
}

impl Job {
    /// A job for `path` at `size`, with the size clamped rather than refused: a
    /// caller that asks for 4 px wants a small picture, not an error.
    #[must_use]
    pub fn new(path: impl Into<PathBuf>, size: u32) -> Self {
        Self {
            path: path.into(),
            size: size.clamp(MIN_SIZE, MAX_SIZE),
        }
    }

    /// Whether `directory` is the folder holding this file.
    ///
    /// Compared as paths and not as text, because a text prefix would accept
    /// `/home/u/Desktopial/file` as being inside `/home/u/Desktop`. The desktop
    /// provider uses this to refuse a request for a file it has no business
    /// decoding; a caller that thumbnails a whole folder ignores it.
    #[must_use]
    pub fn inside(&self, directory: &Path) -> bool {
        self.path.parent() == Some(directory)
    }
}

/// PNG bytes for `job`: the cache first, then a decode, then `None`.
///
/// `None` is the answer for every failure — a file that is not an image, a
/// format with no loader, a source too big to be worth the decode, a cache
/// directory that cannot be written — and not an error, because a caller with no
/// picture to show draws an icon and is not interested in why there is no
/// picture. A decode that succeeds is written into the cache before being
/// returned, so the next caller pays a read.
///
/// Blocking. A caller on a UI thread calls this from a worker.
#[must_use]
pub fn bytes(job: &Job) -> Option<Vec<u8>> {
    if let Some(png) = cached(job) {
        return Some(png);
    }
    let png = make(job)?;
    store(job, &png);
    Some(png)
}

/// Read a cached thumbnail, if one exists and is not older than its source.
#[must_use]
pub fn cached(job: &Job) -> Option<Vec<u8>> {
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

/// Decode and encode a thumbnail, without looking at the cache.
///
/// `None` means "draw an icon", for the reasons listed on [`bytes`].
#[must_use]
pub fn make(job: &Job) -> Option<Vec<u8>> {
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
    // caller no pixels at all.
    if pixbuf.width() < edge / 4 || pixbuf.height() < edge / 4 {
        return None;
    }
    // Level 1 rather than the default: a thumbnail is decoded to be shown once
    // and re-read from disk later, so the CPU spent deflating buys nothing. The
    // option is a zlib level and not a word — the loader rejects `fast` with an
    // error, which is why this is spelled as a number.
    pixbuf.save_to_bufferv("png", &[("compression", "1")]).ok()
}

/// Write a fresh thumbnail into the cache, best effort.
///
/// A failure here is invisible on purpose: the thumbnail is already in memory on
/// its way to being painted, and a cache directory that cannot be written should
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
/// Parsed by hand rather than with `GdkPixbuf`, because the bytes are already in
/// memory and a decode just to learn their size would double the cost of
/// answering. The check is strict about the signature and the chunk type, so
/// bytes that are not a PNG yield `None` instead of a size read out of the middle
/// of a palette.
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

/// Fit an image dimension into the `u16` the desktop protocol uses.
const fn clamp_u16(value: u32) -> u16 {
    if value > u16::MAX as u32 {
        u16::MAX
    } else {
        value as u16
    }
}

/// Whether this build can encode a PNG at all.
///
/// A minimal image can ship `GdkPixbuf` with one loader, and then every thumbnail
/// request has the same answer without anything needing to be decoded to find
/// out. The desktop provider reports this through its `Status`, so a renderer
/// turns the feature off instead of asking twice per icon, and the file manager
/// declines to start a thumbnail worker at all.
#[must_use]
pub fn available() -> bool {
    Pixbuf::formats()
        .iter()
        .any(|f| f.is_writable() && f.name().is_some_and(|n| n == "png"))
}

/// Write PNG bytes to a descriptor for passing over D-Bus.
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
    fn a_path_in_a_subfolder_is_not_directly_inside() {
        let job = Job::new("/home/u/Desktop/img/a.png", 128);
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
    fn a_missing_file_has_no_thumbnail() {
        assert!(bytes(&Job::new("/nonexistent-desktop/a.png", 128)).is_none());
    }

    #[test]
    fn a_text_file_has_no_thumbnail() {
        let dir = tempfile::tempdir().expect("a folder to read");
        let path = dir.path().join("notes.txt");
        std::fs::write(&path, b"not an image at all").expect("a file to thumbnail");
        assert!(bytes(&Job::new(&path, 128)).is_none());
    }

    /// A real PNG, encoded here rather than embedded: the header built by
    /// [`png`] above is enough to test the parser and not enough to decode, and
    /// a decode test that fed it one would pass while proving nothing.
    ///
    /// `None` when this process cannot encode a PNG at all. `available()` reports
    /// what the loader table says, and a loader registered behind a sandbox that
    /// cannot start — `GdkPixbuf` 2.44 hands every format but XPM to a helper
    /// process — is registered and still useless.
    fn real_png(width: u32, height: u32) -> Option<Vec<u8>> {
        // Built from a mutable slice rather than `Pixbuf::new`, whose pixel
        // accessors are read-only: the picture has to be filled in before it is
        // wrapped, and an all-opaque grey is enough for an encoder to compress.
        let mut pixels = vec![0x40u8; width as usize * height as usize * 4];
        for chunk in pixels.chunks_exact_mut(4) {
            chunk[3] = 0xff;
        }
        let pixbuf = Pixbuf::from_mut_slice(
            pixels,
            gdk_pixbuf::Colorspace::Rgb,
            true,
            8,
            width.cast_signed(),
            height.cast_signed(),
            width.cast_signed() * 4,
        );
        pixbuf.save_to_bufferv("png", &[("compression", "1")]).ok()
    }

    #[test]
    fn a_cached_picture_is_served_without_a_decoder() {
        // The cache half of `bytes` needs no image loader at all, so it is
        // asserted unconditionally: a build whose loaders cannot run would
        // otherwise skip most of what this module does.
        let dir = tempfile::tempdir().expect("a folder to read");
        let path = dir.path().join("a.png");
        std::fs::write(&path, b"not an image").expect("a file to thumbnail");
        let cache = cached_path(&path).expect("a cache path");
        std::fs::create_dir_all(cache.parent().expect("a cache folder"))
            .expect("a cache directory");
        let stored = png(48, 24);
        std::fs::write(&cache, &stored).expect("a writable cache");
        assert_eq!(
            bytes(&Job::new(&path, 32)).as_deref(),
            Some(stored.as_slice()),
            "a fresh cache entry must be served instead of decoded again"
        );
    }

    #[test]
    fn a_cache_entry_older_than_its_source_is_not_served() {
        let dir = tempfile::tempdir().expect("a folder to read");
        let path = dir.path().join("a.png");
        std::fs::write(&path, b"not an image").expect("a file to thumbnail");
        let cache = cached_path(&path).expect("a cache path");
        std::fs::create_dir_all(cache.parent().expect("a cache folder"))
            .expect("a cache directory");
        std::fs::write(&cache, png(48, 24)).expect("a writable cache");
        // Ten years in the past, which is older than the source on any clock
        // that is not badly wrong, and the point of the staleness rule.
        let old = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(10);
        std::fs::File::options()
            .write(true)
            .open(&cache)
            .expect("the cache entry")
            .set_modified(old)
            .expect("a filesystem that stores mtimes");
        assert!(cached(&Job::new(&path, 32)).is_none());
        // The source is not an image, so nothing can be decoded either: the
        // answer to the whole request is the icon fallback.
        assert!(bytes(&Job::new(&path, 32)).is_none());
    }

    #[test]
    fn a_real_picture_comes_back_as_png_and_is_cached() {
        let Some(source) = real_png(64, 64) else {
            // The loader table promises PNG and the encoder cannot deliver it:
            // a sandboxed loader whose sandbox cannot start. Real on a locked
            // down system, and the state the icon fallback exists for — so the
            // rest of this module is still exercised, only the decode is not.
            eprintln!("SKIP: this process cannot encode a PNG; no decode to test");
            return;
        };
        let dir = tempfile::tempdir().expect("a folder to read");
        let path = dir.path().join("a.png");
        std::fs::write(&path, &source).expect("a file to thumbnail");
        let got = bytes(&Job::new(&path, 32)).expect("a working encoder must decode");
        assert_eq!(png_size(&got), Some((32, 32)));
        assert!(
            cached_path(&path).is_some_and(|c| c.is_file()),
            "a fresh thumbnail must be written into the cache"
        );
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
