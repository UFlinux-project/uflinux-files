//! Which container this file is, decided from its bytes rather than its name.
//!
//! Extension-only detection is wrong often enough to matter: a `.zip` that is
//! really a `tar`, a `.tar.gz` that is only gzipped, a `.jar` that is a zip. So
//! the magic number wins and the extension only decides the ambiguous cases —
//! whether `a.xz` is a compressed tar or a single compressed file, which no
//! magic can tell apart because both are the same bytes.

use std::path::Path;
use std::process::Command;

use ufm_core::error::{Error, Result};

/// A stream that has to be inflated before the container inside it can be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Codec {
    None,
    Gzip,
    Bzip2,
    Xz,
    Zstd,
}

impl Codec {
    /// The command that writes the decompressed bytes to stdout.
    ///
    /// External rather than linked in: every one of these is already installed
    /// because something else in the base system needs it, and linking four more
    /// decompressors into the file manager is memory the resident set would pay
    /// for even when no compressed archive is open.
    pub(crate) fn decoder(self) -> Option<(&'static str, &'static [&'static str])> {
        match self {
            Self::None => None,
            Self::Gzip => Some(("gzip", &["-dc", "--"][..])),
            Self::Bzip2 => Some(("bzip2", &["-dc", "--"][..])),
            Self::Xz => Some(("xz", &["-dc", "--"][..])),
            Self::Zstd => Some(("zstd", &["-dc", "--quiet"][..])),
        }
    }

    /// The decompressor's name, or `None` for an uncompressed stream.
    #[must_use]
    pub(crate) fn program(self) -> Option<&'static str> {
        match self.decoder() {
            Some((program, _)) => Some(program),
            None => None,
        }
    }

    /// Spawn the decompressor for `path`. The child is the caller's to reap.
    pub fn spawn(self, path: &Path) -> Result<Command> {
        let (program, args) = self
            .decoder()
            .ok_or_else(|| Error::Archive(format!("{} is not compressed", path.display())))?;
        let mut command = Command::new(program);
        command.args(args).arg(path).env("LC_ALL", "C");
        Ok(command)
    }

    #[must_use]
    pub fn is_compressed(self) -> bool {
        self != Self::None
    }
}

/// The containers this build can read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Format {
    Zip,
    /// A tar stream, with or without a codec around it.
    Tar(Codec),
    /// A single file behind a codec: `notes.txt.gz` shows one member, the way
    /// every other file manager shows it.
    Bare(Codec),
    Sevenz,
}

impl Format {
    /// Decide from the first bytes of the file, falling back to the name.
    ///
    /// `head` is the beginning of the file, which the caller has already read
    /// for its own purposes; fewer than 512 bytes is fine and only rules out the
    /// formats that need a full block.
    #[must_use]
    pub fn detect(path: &Path, head: &[u8]) -> Option<Self> {
        if head.starts_with(b"PK\x03\x04") || head.starts_with(b"PK\x05\x06") {
            return Some(Self::Zip);
        }
        if head.starts_with(b"7z\xBC\xAF\x27\x1C") {
            return Some(Self::Sevenz);
        }
        if let Some(codec) = codec_of(head) {
            return Some(if tar_named(path, codec) {
                Self::Tar(codec)
            } else {
                Self::Bare(codec)
            });
        }
        if head.len() >= 263 && matches!(&head[257..263], b"ustar\0" | b"ustar ") {
            return Some(Self::Tar(Codec::None));
        }
        // Nothing matched, so trust the name. A `.tar.gz` whose gzip stream is
        // damaged still has to be reported as a tar, or the error the user sees
        // would claim the file is not an archive at all.
        Self::from_name(path)
    }

    /// Name-based detection, used when the bytes say nothing.
    ///
    /// The name is lowercased first, so comparing against lowercase suffixes is
    /// the case-insensitive check, and `ZIP_NAMES`/`SUFFIXES` stay plain data.
    #[must_use]
    #[allow(clippy::case_sensitive_file_extension_comparisons)]
    pub fn from_name(path: &Path) -> Option<Self> {
        let name = path.file_name()?.to_str()?.to_ascii_lowercase();
        if name.ends_with(".7z") {
            return Some(Self::Sevenz);
        }
        if name.ends_with(".tar") {
            return Some(Self::Tar(Codec::None));
        }
        for &(suffix, codec) in SUFFIXES {
            if name.ends_with(suffix) {
                // Every tar suffix starts with `.t` — `.tar.gz` and the short
                // `.tgz` — and no bare-codec suffix does.
                let tar = suffix.starts_with(".t");
                return Some(if tar {
                    Self::Tar(codec)
                } else {
                    Self::Bare(codec)
                });
            }
        }
        if ZIP_NAMES.iter().any(|s| name.ends_with(s)) {
            return Some(Self::Zip);
        }
        None
    }

    /// The codec wrapped around this container, [`Codec::None`] when there is none.
    #[must_use]
    pub const fn codec(self) -> Codec {
        match self {
            Self::Zip | Self::Sevenz => Codec::None,
            Self::Tar(codec) | Self::Bare(codec) => codec,
        }
    }

    /// The name to give the single member of a [`Self::Bare`] container.
    #[must_use]
    pub fn bare_member_name(path: &Path) -> String {
        let name = path
            .file_name()
            .map_or_else(|| "file".to_string(), |n| n.to_string_lossy().into_owned());
        strip_container_suffix(&name).unwrap_or(&name).to_string()
    }

    /// The name to give the folder a container is unpacked into: `photos.tar.gz`
    /// gives `photos`, `app.apk` gives `app`, and a name no container suffix
    /// explains is left as it is.
    ///
    /// The same rule [`Self::bare_member_name`] uses, because the two questions
    /// are the same question — what would this file be called if it were the
    /// thing inside rather than the wrapper — and a folder called `archive.tar`
    /// next to `archive.tar` is what a second rule produces.
    #[must_use]
    pub fn base_name(path: &Path) -> String {
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            return "archive".to_string();
        };
        strip_container_suffix(name).unwrap_or(name).to_string()
    }
}

/// Drop the container suffix that explains the end of `name`, when there is one.
///
/// Every known suffix is tried, not only the one the format turned out to be: a
/// caller with a path and no [`Format`] in hand cannot know which applies, and a
/// file named `notes.zip.gz` is a gzipped zip whose folder should be `notes.zip`
/// rather than `notes.zip.`. The order matters twice over: [`SUFFIXES`] is
/// longest-first so `.tar.gz` wins over `.gz`, and the two suffixes
/// [`Format::from_name`] checks ahead of that table — `.7z` and a plain `.tar` —
/// come last, so that `a.tar.gz` is never read as a tar called `a.gz`.
///
/// A name equal to its suffix is left alone: `.zip` and `.7z` are dotfiles, not
/// containers with nothing in front of them.
fn strip_container_suffix(name: &str) -> Option<&str> {
    const PLAIN: [&str; 2] = [".7z", ".tar"];
    let lower = name.to_ascii_lowercase();
    let suffix = SUFFIXES
        .iter()
        .map(|&(suffix, _)| suffix)
        .chain(ZIP_NAMES.iter().copied())
        .chain(PLAIN)
        .find(|suffix| name.len() > suffix.len() && lower.ends_with(suffix))?;
    Some(&name[..name.len() - suffix.len()])
}

/// `.tgz` is a tar and `.gz` is not, so the suffix itself carries the answer.
///
/// Longest first, which is what lets [`strip_container_suffix`] stop at the
/// first match: `.tar.gz` has to be tried before `.gz`, or a tarball becomes a
/// compressed file called `a.tar`.
const SUFFIXES: &[(&str, Codec)] = &[
    (".tar.gz", Codec::Gzip),
    (".tar.bz2", Codec::Bzip2),
    (".tar.xz", Codec::Xz),
    (".tar.zst", Codec::Zstd),
    (".tbz2", Codec::Bzip2),
    (".txz", Codec::Xz),
    (".tzst", Codec::Zstd),
    (".tgz", Codec::Gzip),
    (".gz", Codec::Gzip),
    (".bz2", Codec::Bzip2),
    (".xz", Codec::Xz),
    (".zst", Codec::Zstd),
];

/// Whether a compressed file is a tar inside a codec rather than one compressed
/// file. Both are the same bytes, so only the name says which.
fn tar_named(path: &Path, codec: Codec) -> bool {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    let Some(ext) = codec.ext() else {
        return false;
    };
    let name = name.to_ascii_lowercase();
    // `.tar.gz` and the short form `.tgz`, which is `.t` plus the same extension.
    name.ends_with(&format!(".tar.{ext}")) || name.ends_with(&format!(".t{ext}"))
}

/// Containers that are a zip under another name, all of them openable as one.
const ZIP_NAMES: &[&str] = &[
    ".zip", ".jar", ".war", ".ear", ".apk", ".whl", ".epub", ".docx", ".xlsx", ".pptx", ".odt",
    ".ods", ".odp", ".rpm",
];

impl Codec {
    pub(crate) const fn ext(self) -> Option<&'static str> {
        match self {
            Self::None => None,
            Self::Gzip => Some("gz"),
            Self::Bzip2 => Some("bz2"),
            Self::Xz => Some("xz"),
            Self::Zstd => Some("zst"),
        }
    }
}

fn codec_of(head: &[u8]) -> Option<Codec> {
    if head.starts_with(b"\x1F\x8B") {
        return Some(Codec::Gzip);
    }
    if head.starts_with(b"BZh") {
        return Some(Codec::Bzip2);
    }
    if head.starts_with(b"\xFD7zXZ\x00") {
        return Some(Codec::Xz);
    }
    if head.starts_with(b"\x28\xB5\x2F\xFD") {
        return Some(Codec::Zstd);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn p(name: &str) -> PathBuf {
        PathBuf::from(name)
    }

    #[test]
    fn a_magic_number_beats_the_extension() {
        // Named .tar, really a zip. Navigating it as a tar would fail; as a zip
        // it works, which is what the user meant.
        assert_eq!(
            Format::detect(&p("/t/a.tar"), b"PK\x03\x04junk"),
            Some(Format::Zip)
        );
        assert_eq!(
            Format::detect(&p("/t/a.zip"), b"7z\xBC\xAF\x27\x1C.."),
            Some(Format::Sevenz)
        );
    }

    #[test]
    fn a_tar_is_found_by_its_header() {
        let mut block = [0u8; 512];
        block[257..263].copy_from_slice(b"ustar\0");
        assert_eq!(
            Format::detect(&p("/t/mystery"), &block),
            Some(Format::Tar(Codec::None))
        );
    }

    #[test]
    fn a_compound_suffix_is_removed_whole() {
        // The order the suffixes are tried in is what makes these agree with
        // `from_name`: `a.tar.gz` is a gzipped tar, so the folder is `a`. Reading
        // the plain `.gz` rule first would give `a.tar`, and a folder called
        // `a.tar` beside `a.tar.gz` is what a second rule produces.
        assert_eq!(Format::base_name(&p("/t/a.tar.gz")), "a");
        assert_eq!(Format::base_name(&p("/t/a.tar.xz")), "a");
        assert_eq!(Format::base_name(&p("/t/a.tar")), "a");
        assert_eq!(Format::base_name(&p("/t/a.tgz")), "a");
        assert_eq!(Format::base_name(&p("/t/photos.TGZ")), "photos");
        assert_eq!(Format::base_name(&p("/t/notes.txt.gz")), "notes.txt");
    }

    #[test]
    fn a_name_with_no_container_suffix_is_kept() {
        // A name that is nothing but a suffix is a dotfile, and a name with no
        // suffix at all is already the folder name a user would expect.
        assert_eq!(Format::base_name(&p("/t/.zip")), ".zip");
        assert_eq!(Format::base_name(&p("/t/README")), "README");
        assert_eq!(Format::base_name(&p("/t/archive.7z")), "archive");
        assert_eq!(Format::base_name(&p("/t/app.apk")), "app");
    }

    #[test]
    fn a_path_with_no_name_still_gets_a_folder_name() {
        // The caller has somewhere to put the result either way, and a folder
        // called `archive` beats refusing to name one.
        assert_eq!(Format::base_name(&p("/")), "archive");
        assert_eq!(Format::base_name(&p("a/b/..")), "archive");
    }

    #[test]
    fn a_compressed_tar_and_a_compressed_file_are_told_apart() {
        let gz = b"\x1F\x8B\x08\x00";
        assert_eq!(
            Format::detect(&p("/t/a.tar.gz"), gz),
            Some(Format::Tar(Codec::Gzip))
        );
        assert_eq!(
            Format::detect(&p("/t/a.tgz"), gz),
            Some(Format::Tar(Codec::Gzip))
        );
        assert_eq!(
            Format::detect(&p("/t/notes.txt.gz"), gz),
            Some(Format::Bare(Codec::Gzip))
        );
        assert_eq!(
            Format::detect(&p("/t/a.txz"), b"\xFD7zXZ\x00"),
            Some(Format::Tar(Codec::Xz))
        );
        assert_eq!(
            Format::detect(&p("/t/a.zst"), b"\x28\xB5\x2F\xFD"),
            Some(Format::Bare(Codec::Zstd))
        );
    }

    #[test]
    fn an_unreadable_file_is_still_classified_by_its_name() {
        assert_eq!(Format::from_name(&p("/t/a.7z")), Some(Format::Sevenz));
        assert_eq!(
            Format::from_name(&p("/t/app.apk")),
            Some(Format::Zip),
            "an APK is a zip"
        );
        assert_eq!(
            Format::from_name(&p("/t/a.tar")),
            Some(Format::Tar(Codec::None))
        );
        assert_eq!(
            Format::from_name(&p("/t/a.tar.bz2")),
            Some(Format::Tar(Codec::Bzip2))
        );
        assert_eq!(Format::from_name(&p("/t/notes.txt")), None);
    }

    #[test]
    fn a_bare_container_is_named_after_the_file_without_its_suffix() {
        assert_eq!(Format::bare_member_name(&p("/t/notes.txt.gz")), "notes.txt");
        assert_eq!(Format::bare_member_name(&p("/t/a.gz")), "a");
        assert_eq!(Format::bare_member_name(&p("/t/noext")), "noext");
    }

    #[test]
    fn an_unknown_extension_is_not_an_archive() {
        assert_eq!(Format::detect(&p("/t/a.bin"), b"\0\0\0\0"), None);
    }
}
