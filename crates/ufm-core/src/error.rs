use std::fmt;
use std::io;

/// Anything that can go wrong while resolving a location, reading a directory,
/// or persisting preferences.
#[derive(Debug)]
pub enum Error {
    /// The string is not a location this build understands.
    InvalidLocation(String),
    /// A path component inside an archive URI was `.`, `..`, empty or contained
    /// a NUL, so resolving it would escape the archive.
    UnsafeComponent(String),
    /// The archive container itself is not readable or not a supported format.
    Archive(String),
    /// A mount could not be established or torn down. Carries a message that is
    /// safe to show to the user.
    Mount(String),
    /// The indexer rejected a query or is not reachable.
    Search(String),
    /// A preferences or state file could not be read or parsed.
    Config(String),
    /// A D-Bus reply did not match the expected interface version or shape.
    Protocol(String),
    /// The operation was cancelled, usually because the user navigated away.
    Cancelled,
    Io(io::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidLocation(s) => write!(f, "not a valid location: {s}"),
            Self::UnsafeComponent(c) => write!(f, "unsafe path component: {c}"),
            Self::Archive(m) => write!(f, "archive error: {m}"),
            Self::Mount(m) => write!(f, "mount error: {m}"),
            Self::Search(m) => write!(f, "search error: {m}"),
            Self::Config(m) => write!(f, "configuration error: {m}"),
            Self::Protocol(m) => write!(f, "protocol error: {m}"),
            Self::Cancelled => write!(f, "cancelled"),
            Self::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Self::Config(e.to_string())
    }
}

impl Error {
    /// Whether the caller should report this to the user at all. A cancelled
    /// read is normal when a tab is closed or a directory is left quickly, and
    /// surfacing it would produce spurious error banners.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        matches!(self, Self::Cancelled)
    }
}

/// The result type every fallible function in the workspace returns.
pub type Result<T, E = Error> = std::result::Result<T, E>;
