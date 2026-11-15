//! Locations: the one abstraction every other crate is written against.
//!
//! A [`Location`] is a URI in one of four shapes:
//!
//! * `file:///abs/path` — a local path, and also every mounted network share,
//!   because shares are mounted with a kernel or FUSE helper rather than
//!   emulated in process.
//! * `archive:///home/u/a.zip!/inner/dir` — the only virtual filesystem. `!`
//!   separates the container from the path inside it.
//! * `computer://`, `recent://`, `starred://` — synthesized collections.
//! * `search://<query-id>` — a saved result set.
//!
//! Everything that is not an archive is therefore a plain path, which is what
//! lets thumbnails, drag-and-drop and "Open With" work unchanged over SMB, NFS,
//! SFTP, `WebDAV` and FTP.

use std::ffi::OsStr;
use std::fmt;
use std::path::{Component, Path, PathBuf};

use percent_encoding::{AsciiSet, CONTROLS};

use crate::error::{Error, Result};

/// Characters escaped inside a single path component. `/` splits components and
/// `#`/`?` terminate a URI, so all three are escaped; `%` is escaped so that
/// decoding stays unambiguous; `!` is escaped because it separates an archive
/// container from the path inside it and a container name may contain one.
const SET: &AsciiSet = &CONTROLS.add(b'/').add(b'%').add(b'#').add(b'?').add(b'!');

/// The separator between an archive path and the path inside the archive.
pub const SEPARATOR: char = '!';

/// The synthesized collections that are not backed by a directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum VirtualDir {
    /// The "My Computer" overview: drives, mounts, devices, network.
    Computer,
    /// Recently used files.
    Recent,
    /// User-starred locations.
    Starred,
}

impl VirtualDir {
    #[must_use]
    pub const fn scheme(self) -> &'static str {
        match self {
            Self::Computer => "computer",
            Self::Recent => "recent",
            Self::Starred => "starred",
        }
    }

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Computer => "Computer",
            Self::Recent => "Recent",
            Self::Starred => "Starred",
        }
    }
}

/// A saved search result set, addressed by the id the indexer returned.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SearchId(pub String);

/// Any place the file manager can show.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Location {
    /// A real file or directory on a local or mounted filesystem.
    File { path: PathBuf },
    /// A directory inside an archive container. `inner` is `/`-separated,
    /// normalized, and empty at the root of the archive.
    Archive { archive: PathBuf, inner: String },
    /// One of the synthesized collections.
    Virtual(VirtualDir),
    /// A saved search result set.
    Search(SearchId),
}

impl Location {
    /// Parse a location. Accepts the URI forms above plus a bare absolute path,
    /// which is what the shell, drag-and-drop and `uflinux-files ~/Docs` hand us.
    pub fn parse(input: &str) -> Result<Self> {
        let trimmed = input.trim();
        if trimmed.is_empty() {
            return Err(Error::InvalidLocation(input.to_string()));
        }

        // No scheme means a bare path.
        if trimmed.starts_with('/') {
            return Ok(Self::File {
                path: normalize_absolute(Path::new(trimmed))?,
            });
        }

        let Some((scheme, rest)) = trimmed.split_once("://") else {
            return Err(Error::InvalidLocation(input.to_string()));
        };

        match scheme {
            // Only the empty-authority form. `file://host/` would imply a remote
            // URI, and we mount those instead of emulating them.
            "file" => {
                let path = percent_decode_path(rest);
                if path.is_empty() {
                    return Err(Error::InvalidLocation(input.to_string()));
                }
                Ok(Self::File {
                    path: normalize_absolute(Path::new(&path))?,
                })
            }
            "archive" => Self::parse_archive(rest, input),
            "computer" => Ok(Self::Virtual(VirtualDir::Computer)),
            "recent" => Ok(Self::Virtual(VirtualDir::Recent)),
            "starred" => Ok(Self::Virtual(VirtualDir::Starred)),
            "search" => {
                let id = rest.trim_end_matches('/');
                if id.is_empty() {
                    return Err(Error::InvalidLocation(input.to_string()));
                }
                Ok(Self::Search(SearchId(id.to_string())))
            }
            _ => Err(Error::InvalidLocation(input.to_string())),
        }
    }

    /// `archive:///home/u/a.zip!/inner/dir`. The inner path may be empty, which
    /// is the root of the archive, and may itself contain `!` when escaped as
    /// `\!`. The separator is mandatory: without it `archive:///t/a.zip` would
    /// be a second spelling of a location that already has one, and the views
    /// would have to guess whether they are looking at a container or inside it.
    fn parse_archive(rest: &str, original: &str) -> Result<Self> {
        let invalid = || Error::InvalidLocation(original.to_string());
        let archive_end = find_separator(rest).ok_or_else(invalid)?;
        let archive_raw = &rest[..archive_end];
        let inner_raw = &rest[archive_end + SEPARATOR.len_utf8()..];

        let archive = percent_decode_path(archive_raw);
        if archive.is_empty() {
            return Err(invalid());
        }
        let archive = normalize_absolute(Path::new(&archive))?;

        let inner = unescape_inner(inner_raw.trim_end_matches('/'));
        if !inner.is_empty() {
            for component in inner.split('/') {
                if component.is_empty()
                    || component == "."
                    || component == ".."
                    || component.contains('\0')
                {
                    return Err(Error::UnsafeComponent(component.to_string()));
                }
            }
        }

        Ok(Self::Archive { archive, inner })
    }

    /// Build a location for a path on disk.
    #[must_use]
    pub fn file(path: impl Into<PathBuf>) -> Self {
        Self::File { path: path.into() }
    }

    /// Build a location for a directory inside an archive. Every component is
    /// validated the same way a parsed one is.
    pub fn archive<P: Into<PathBuf>, I: AsRef<str>>(archive: P, inner: &[I]) -> Result<Self> {
        let mut joined = String::new();
        for component in inner {
            let c = component.as_ref();
            if c.is_empty() || c == "." || c == ".." || c.contains('/') || c.contains('\0') {
                return Err(Error::UnsafeComponent(c.to_string()));
            }
            if !joined.is_empty() {
                joined.push('/');
            }
            joined.push_str(c);
        }
        Ok(Self::Archive {
            archive: archive.into(),
            inner: joined,
        })
    }

    #[must_use]
    pub fn computer() -> Self {
        Self::Virtual(VirtualDir::Computer)
    }

    /// Which synthesized collection this location names, if it names one.
    /// A saved search is a result set rather than a collection, so it is not
    /// answered here; a reader deciding what to do with a location that is not
    /// a directory needs the one and not the other.
    #[must_use]
    pub fn virtual_dir(&self) -> Option<VirtualDir> {
        match self {
            Self::Virtual(dir) => Some(*dir),
            Self::File { .. } | Self::Archive { .. } | Self::Search(_) => None,
        }
    }

    /// The archive container, when this location is inside one.
    #[must_use]
    pub fn archive_path(&self) -> Option<&Path> {
        match self {
            Self::Archive { archive, .. } => Some(archive),
            _ => None,
        }
    }

    /// The path inside the archive, `/`-separated and normalized. Empty at the
    /// root of the archive.
    #[must_use]
    pub fn archive_inner(&self) -> Option<&str> {
        match self {
            Self::Archive { inner, .. } => Some(inner),
            _ => None,
        }
    }

    /// The real on-disk path for a `file` location, or for an archive location
    /// the container itself. Views use this to decide whether a location can be
    /// handed to an external program.
    #[must_use]
    pub fn as_path(&self) -> Option<&Path> {
        match self {
            Self::File { path } | Self::Archive { archive: path, .. } => Some(path),
            Self::Virtual(_) | Self::Search(_) => None,
        }
    }

    /// Whether this location is a directory the kernel can see, and so whether
    /// GIO operations, thumbnails and "Open With" all work on it directly.
    #[must_use]
    pub fn is_real(&self) -> bool {
        matches!(self, Self::File { .. })
    }

    #[must_use]
    pub fn is_archive(&self) -> bool {
        matches!(self, Self::Archive { .. })
    }

    #[must_use]
    pub fn is_virtual(&self) -> bool {
        matches!(self, Self::Virtual(_) | Self::Search(_))
    }

    /// The location one level up. `None` at a filesystem root, at the root of an
    /// archive, or for a virtual location.
    #[must_use]
    pub fn parent(&self) -> Option<Self> {
        match self {
            Self::File { path } => {
                let mut names: Vec<&OsStr> = path
                    .components()
                    .filter_map(|c| match c {
                        Component::Normal(s) => Some(s),
                        _ => None,
                    })
                    .collect();
                if names.is_empty() {
                    return None;
                }
                names.pop();
                Some(Self::File {
                    path: join_absolute(&names),
                })
            }
            Self::Archive { archive, inner } => {
                if let Some(i) = inner.rfind('/') {
                    return Some(Self::Archive {
                        archive: archive.clone(),
                        inner: inner[..i].to_string(),
                    });
                }
                if !inner.is_empty() {
                    // One component deep: up is the root of the archive.
                    return Some(Self::Archive {
                        archive: archive.clone(),
                        inner: String::new(),
                    });
                }
                // At the root of an archive, up leaves it and shows the folder
                // that holds the container.
                archive.parent().map(Self::file)
            }
            Self::Virtual(_) | Self::Search(_) => None,
        }
    }

    /// Descend into a single child name, as it appears in a directory listing.
    pub fn join(&self, name: &str) -> Result<Self> {
        if name.is_empty() || name.contains('/') || name.contains('\0') {
            return Err(Error::UnsafeComponent(name.to_string()));
        }
        match self {
            Self::File { path } => Ok(Self::File {
                path: path.join(name),
            }),
            Self::Archive { archive, inner } => {
                let mut parts = split_inner(inner);
                parts.push(name);
                Self::archive(archive.clone(), &parts)
            }
            Self::Virtual(_) | Self::Search(_) => Err(Error::InvalidLocation(format!(
                "{self}: cannot enter {name}"
            ))),
        }
    }

    /// Descend into a child given as a `/`-separated relative path, as it appears
    /// inside an archive table of contents.
    pub fn join_inner(&self, relative: &str) -> Result<Self> {
        for component in split_relative(relative) {
            if component == ".." || component.contains('\0') {
                return Err(Error::UnsafeComponent(component.to_string()));
            }
        }
        match self {
            Self::File { path } => {
                let mut out = path.clone();
                for component in split_relative(relative) {
                    out.push(component);
                }
                Ok(Self::File { path: out })
            }
            Self::Archive { archive, inner } => {
                let mut parts = split_inner(inner);
                parts.extend(split_relative(relative));
                Self::archive(archive.clone(), &parts)
            }
            Self::Virtual(_) | Self::Search(_) => Err(Error::InvalidLocation(self.to_string())),
        }
    }

    /// The final component, for a tab label or a breadcrumb leaf.
    #[must_use]
    pub fn file_name(&self) -> Option<&str> {
        match self {
            Self::File { path } => path.file_name().and_then(OsStr::to_str),
            Self::Archive { inner, archive } => {
                let leaf = inner.rsplit('/').next().unwrap_or_default();
                if leaf.is_empty() {
                    archive.file_name().and_then(OsStr::to_str)
                } else {
                    Some(leaf)
                }
            }
            Self::Virtual(dir) => Some(dir.label()),
            Self::Search(_) => Some("Search"),
        }
    }

    /// Whether `self` is `ancestor` or lives inside it. Compares normalized
    /// forms, so this is safe for a "may this window navigate here" check.
    #[must_use]
    pub fn is_within(&self, ancestor: &Self) -> bool {
        match (self, ancestor) {
            (Self::File { path }, Self::File { path: a }) => path == a || path.starts_with(a),
            (
                Self::Archive { archive, inner },
                Self::Archive {
                    archive: a,
                    inner: ai,
                },
            ) => {
                archive == a
                    && (ai.is_empty()
                        || inner == ai
                        || inner
                            .strip_prefix(ai.as_str())
                            .is_some_and(|rest| rest.starts_with('/')))
            }
            (Self::Search(mine), Self::Search(theirs)) => mine == theirs,
            (Self::Virtual(mine), Self::Virtual(theirs)) => mine == theirs,
            _ => false,
        }
    }

    /// A short label with `$HOME` replaced by `~`, for the breadcrumb and title.
    #[must_use]
    pub fn display(&self) -> String {
        match self {
            Self::File { path } => shorten_home(path),
            Self::Archive { archive, inner } => {
                let mut s = shorten_home(archive);
                s.push_str(" \u{203a} ");
                if inner.is_empty() {
                    s.push('/');
                } else {
                    s.push_str(inner);
                }
                s
            }
            Self::Virtual(dir) => dir.label().to_string(),
            Self::Search(id) => format!("Search {}", id.0),
        }
    }
}

impl fmt::Display for Location {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::File { path } => {
                f.write_str("file://")?;
                write_encoded_path(f, path)
            }
            Self::Archive { archive, inner } => {
                f.write_str("archive://")?;
                write_encoded_path(f, archive)?;
                write!(f, "{SEPARATOR}")?;
                let escaped = inner
                    .split('/')
                    .filter(|c| !c.is_empty())
                    .map(|c| c.replace('\\', "\\\\").replace('!', "\\!"))
                    .collect::<Vec<_>>()
                    .join("/");
                if escaped.is_empty() {
                    f.write_str("/")
                } else {
                    write!(f, "/{escaped}")
                }
            }
            Self::Virtual(dir) => write!(f, "{}://", dir.scheme()),
            Self::Search(id) => write!(f, "search://{}", id.0),
        }
    }
}

impl std::str::FromStr for Location {
    type Err = Error;
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        Self::parse(s)
    }
}

impl serde::Serialize for Location {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> serde::Deserialize<'de> for Location {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Self::parse(&s).map_err(serde::de::Error::custom)
    }
}

/// Emit `/a/b` for `/a/b` and `/` for the root. The caller has already written
/// the `scheme://` prefix.
fn write_encoded_path(f: &mut fmt::Formatter<'_>, path: &Path) -> fmt::Result {
    let mut wrote = false;
    for component in path.components() {
        if let Component::Normal(s) = component {
            let text = s.to_string_lossy();
            f.write_str("/")?;
            f.write_str(&percent_encoding::utf8_percent_encode(&text, SET).to_string())?;
            wrote = true;
        }
    }
    if !wrote {
        f.write_str("/")?;
    }
    Ok(())
}

/// Collapse `//` and `.`, resolve `..` lexically, keep the result absolute.
/// Deliberately lexical rather than symlink-resolving: the breadcrumb shows the
/// path the user actually navigated, which is what Nautilus does too.
fn normalize_absolute(path: &Path) -> Result<PathBuf> {
    let mut names: Vec<&OsStr> = Vec::new();
    for component in path.components() {
        match component {
            Component::RootDir | Component::CurDir => {}
            Component::ParentDir => {
                names.pop();
            }
            Component::Normal(s) => names.push(s),
            Component::Prefix(_) => return Err(Error::InvalidLocation(path.display().to_string())),
        }
    }
    Ok(join_absolute(&names))
}

fn join_absolute(names: &[&OsStr]) -> PathBuf {
    let mut out = PathBuf::from("/");
    for name in names {
        out.push(name);
    }
    out
}

fn split_inner(inner: &str) -> Vec<&str> {
    if inner.is_empty() {
        Vec::new()
    } else {
        inner.split('/').collect()
    }
}

/// Split a relative path, dropping empty and `.` components.
fn split_relative(relative: &str) -> Vec<&str> {
    relative
        .split('/')
        .filter(|c| !c.is_empty() && *c != ".")
        .collect()
}

/// Index of the first unescaped `!`, or `None` when the URI has no separator.
fn find_separator(raw: &str) -> Option<usize> {
    let mut escaped = false;
    for (i, b) in raw.as_bytes().iter().enumerate() {
        if escaped {
            escaped = false;
            continue;
        }
        match b {
            b'\\' => escaped = true,
            b'!' => return Some(i),
            _ => {}
        }
    }
    None
}

fn unescape_inner(raw: &str) -> String {
    let raw = raw.strip_prefix('/').unwrap_or(raw);
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            // `\!` is the escape; `\\` keeps a literal backslash readable.
            Some('!') => out.push('!'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

fn percent_decode_path(raw: &str) -> String {
    // A malformed escape is left alone rather than dropped, so a file literally
    // named `%zz` still resolves.
    percent_encoding::percent_decode_str(raw)
        .decode_utf8_lossy()
        .into_owned()
}

fn shorten_home(path: &Path) -> String {
    static HOME: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    let home = HOME.get_or_init(|| std::env::var("HOME").unwrap_or_default());
    let s = path.display().to_string();
    if home.is_empty() {
        return s;
    }
    let Some(rest) = s.strip_prefix(home.as_str()) else {
        return s;
    };
    // Only a real boundary counts, so `/home/user2` is not shown as `~2`.
    if rest.is_empty() {
        return "~".to_string();
    }
    let Some(rest) = rest.strip_prefix('/') else {
        return s;
    };
    if rest.is_empty() {
        "~".to_string()
    } else {
        format!("~/{rest}")
    }
}

#[cfg(test)]
mod tests;
