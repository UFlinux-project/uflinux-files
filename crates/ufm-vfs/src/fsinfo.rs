//! Filesystem capabilities, per mount point.
//!
//! Two things vary by filesystem and cannot be assumed: whether a trash can
//! exists, and whether the filesystem reports usage at all. Getting these wrong
//! is worse than not showing them — offering Trash on an SMB share silently
//! loses a file, and drawing a usage bar for a FUSE mount that returns zeroes
//! looks like a broken disk.

// `statvfs` and the mountinfo probe are the syscall boundary of this crate.
#![allow(unsafe_code)]

use std::collections::HashMap;
use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// What a filesystem can do, and its capacity, as one small copyable value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FsHints {
    /// Total bytes, `None` when the filesystem does not report a size.
    pub total: Option<u64>,
    /// Bytes available to an unprivileged user.
    pub free: Option<u64>,
    /// A per-user trash can can exist here: a local filesystem, not read-only.
    pub trash: bool,
    /// The mount is mounted read-only.
    pub read_only: bool,
    /// Network or FUSE, where a round trip costs milliseconds and a listing
    /// should not be re-read speculatively.
    pub remote: bool,
}

/// Filesystems that have a real `.Trash-$UID` we can create. Anything else,
/// including FUSE mounts whose owner is unknown, may refuse trashing. Such a refusal
/// must leave the source intact; permanent deletion is a separate operation.
const TRASH_TYPES: &[&str] = &[
    "ext2", "ext3", "ext4", "btrfs", "xfs", "f2fs", "jfs", "reiserfs", "tmpfs",
];

/// Filesystem types where one `stat` is a network packet.
const REMOTE_TYPES: &[&str] = &[
    "cifs",
    "smb",
    "smb2",
    "nfs",
    "nfs4",
    "sftp",
    "sshfs",
    "fuse.sshfs",
    "davfs",
    "fuse.davfs",
    "fuse.davfs2",
    "fuse.lftp",
    "fuse.curlftpfs",
    "webdav",
    "rclone",
    "fuse.rclone",
    "9p",
    "glusterfs",
    "ceph",
    "cephfs",
    "fuse.s3fs",
];

impl FsHints {
    #[must_use]
    pub fn supports_trash(&self) -> bool {
        self.trash
    }

    /// Fraction used, `None` when unknown. Clamped, so a filesystem reporting
    /// more free than total draws an empty bar rather than a negative one.
    #[must_use]
    pub fn used_fraction(&self) -> Option<f64> {
        let (total, free) = (self.total?, self.free?);
        if total == 0 {
            return None;
        }
        Some((total.saturating_sub(free)) as f64 / total as f64)
    }
}

/// One line of `/proc/self/mountinfo`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Mount {
    /// The mount point itself. Named `path` rather than `mount_point` so it does
    /// not repeat the struct name at every use site.
    path: PathBuf,
    fstype: String,
    read_only: bool,
}

/// Look up the capabilities of the filesystem holding a path.
///
/// Results are cached per mount point and invalidated on a mount or unmount,
/// because `statvfs` on a dead NFS mount can block for tens of seconds while the
/// answer changes only slowly.
#[derive(Default)]
pub struct FsInfo {
    cache: Mutex<HashMap<PathBuf, FsHints>>,
}

impl FsInfo {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn hints(&self, path: &Path) -> FsHints {
        let key = mount_for(path).map_or_else(|| PathBuf::from("/"), |m| m.path);
        let mut cache = self.lock();
        if let Some(h) = cache.get(&key) {
            return *h;
        }
        let h = probe(path);
        // Bounded: one entry is a path and four integers, and a session that
        // touches more than 64 devices is unusual enough that dropping the whole
        // table is cheaper than tracking ages.
        if cache.len() >= 64 {
            cache.clear();
        }
        cache.insert(key, h);
        h
    }

    pub fn invalidate(&self, path: &Path) {
        let key = mount_for(path).map_or_else(|| PathBuf::from("/"), |m| m.path);
        self.lock().remove(&key);
    }

    /// Forget everything, after a mount or unmount.
    pub fn clear(&self) {
        self.lock().clear();
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<PathBuf, FsHints>> {
        // A panic while probing one mount must not make every later lookup fail.
        self.cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// Ask the running kernel about `path`.
fn probe(path: &Path) -> FsHints {
    let mut hints = FsHints::default();
    if let Some(mount) = mount_for(path) {
        hints.remote = REMOTE_TYPES.contains(&mount.fstype.as_str());
        hints.read_only = mount.read_only;
        hints.trash = TRASH_TYPES.contains(&mount.fstype.as_str()) && !mount.read_only;
    }

    let Ok(c_path) = CString::new(path.as_os_str().as_bytes()) else {
        return hints;
    };
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: c_path points at a valid NUL-terminated path and statvfs only
    // writes through the second pointer.
    let ok = unsafe { libc::statvfs(c_path.as_ptr(), &raw mut stat) } == 0;
    if ok {
        // The statvfs counters are `u64` on a 64-bit target and `usize` elsewhere,
        // so the conversion is kept rather than removed; the saturating multiply
        // covers a filesystem reporting a count large enough to overflow.
        #[allow(clippy::useless_conversion)]
        let (block, blocks, avail) = (
            u64::try_from(stat.f_frsize).unwrap_or(0),
            u64::try_from(stat.f_blocks).unwrap_or(0),
            u64::try_from(stat.f_bavail).unwrap_or(0),
        );
        if block > 0 && blocks > 0 {
            hints.total = Some(block.saturating_mul(blocks));
            hints.free = Some(block.saturating_mul(avail));
        }
    }
    hints
}

/// Check the mount table without accessing the target filesystem.
#[must_use]
pub fn is_remote_mount(path: &Path) -> bool {
    deepest(mounts(), path).is_some_and(|mount| REMOTE_TYPES.contains(&mount.fstype.as_str()))
}

/// The longest mount point that is a prefix of `path`, so a nested bind mount or
/// a `~/mnt` overlay resolves to itself rather than to `/`.
fn mount_for(path: &Path) -> Option<Mount> {
    let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    deepest(mounts(), &canonical)
}

/// The longest mount point that is a prefix of `path`, taken from a parsed
/// table rather than the live one so the rule can be checked against a
/// fixture.
fn deepest(mounts: Vec<Mount>, path: &Path) -> Option<Mount> {
    mounts
        .into_iter()
        .filter(|m| path.starts_with(&m.path))
        .max_by_key(|m| m.path.as_os_str().len())
}

/// Parse `/proc/self/mountinfo`.
fn mounts() -> Vec<Mount> {
    let Ok(text) = std::fs::read_to_string("/proc/self/mountinfo") else {
        return Vec::new();
    };
    parse_mounts(&text)
}

fn parse_mounts(text: &str) -> Vec<Mount> {
    text.lines().filter_map(parse_mountinfo_line).collect()
}

/// `36 35 98:1 /mnt /home/u/data rw,relatime shared:2 - ext4 /dev/sda1 rw`
///
/// The mount point is field 5 of the head; the filesystem type is field 1 after
/// the `-` separator, because the number of optional fields varies.
fn parse_mountinfo_line(line: &str) -> Option<Mount> {
    let fields: Vec<&str> = line.split_whitespace().collect();
    let sep = fields.iter().position(|f| *f == "-")?;
    if sep < 5 || fields.len() < sep + 3 {
        return None;
    }
    let path = PathBuf::from(unescape(fields[4]));
    let fstype = unescape(fields[sep + 1]);
    let super_options = fields[sep + 2];
    let head_options = fields[5];
    Some(Mount {
        path,
        fstype,
        read_only: head_options
            .split(',')
            .chain(super_options.split(','))
            .any(|o| o == "ro"),
    })
}

/// mountinfo escapes a space, tab, newline and backslash as three octal digits,
/// which would otherwise hide a mount point under `~/My Documents`. Anything that
/// is not three octal digits is left as written, so a path that really does
/// contain a backslash survives the round trip.
fn unescape(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        // The kernel escapes every byte above 0x7E too, so anything left over is
        // UTF-8 continuation and is copied verbatim.
        if bytes[i] == b'\\' && i + 3 < bytes.len() {
            match octal(&bytes[i + 1..i + 4]) {
                Some(byte) => {
                    out.push(byte);
                    i += 4;
                    continue;
                }
                None => out.push(b'\\'),
            }
        } else {
            out.push(bytes[i]);
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn octal(digits: &[u8]) -> Option<u8> {
    if digits.len() != 3 {
        return None;
    }
    let value = digits.iter().try_fold(0u16, |acc, &d| {
        let digit = d.checked_sub(b'0')?;
        (digit < 8).then(|| (acc << 3) | u16::from(digit))
    })?;
    Some(value as u8)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn used_fraction_is_honest() {
        let h = FsHints {
            total: Some(1000),
            free: Some(250),
            ..FsHints::default()
        };
        assert_eq!(h.used_fraction(), Some(0.75));
        assert_eq!(
            FsHints::default().used_fraction(),
            None,
            "unknown must not draw a bar"
        );
        let zero = FsHints {
            total: Some(0),
            free: Some(0),
            ..FsHints::default()
        };
        assert_eq!(zero.used_fraction(), None);
        let weird = FsHints {
            total: Some(100),
            free: Some(400),
            ..FsHints::default()
        };
        assert_eq!(weird.used_fraction(), Some(0.0), "a negative bar is a bug");
    }

    #[test]
    fn trash_only_where_a_trash_can_can_exist() {
        assert!(
            FsHints {
                trash: true,
                ..Default::default()
            }
            .supports_trash()
        );
        assert!(
            !FsHints {
                trash: false,
                remote: true,
                ..Default::default()
            }
            .supports_trash()
        );
    }

    #[test]
    fn mountinfo_lines_parse() {
        let line = "36 35 98:1 /mnt /home/u/data rw,relatime shared:2 - ext4 /dev/sda1 rw";
        let m = parse_mountinfo_line(line).unwrap();
        assert_eq!(m.path, PathBuf::from("/home/u/data"));
        assert_eq!(m.fstype, "ext4");
        assert!(!m.read_only);

        // No optional fields at all.
        let line = "18 1 8:1 / / ro,noatime - xfs /dev/sda1 ro,attr2";
        let m = parse_mountinfo_line(line).unwrap();
        assert_eq!(m.path, PathBuf::from("/"));
        assert_eq!(m.fstype, "xfs");
        assert!(m.read_only);

        // A CIFS mount, which is what a network share looks like once mounted.
        let line = "40 1 0:38 / /run/user/1000/ufm/fileserver-share rw,nosuid,nodev - cifs //fs/share rw,seal";
        let m = parse_mountinfo_line(line).unwrap();
        assert_eq!(m.fstype, "cifs");
        assert!(!m.read_only);
        assert!(REMOTE_TYPES.contains(&m.fstype.as_str()));
    }

    #[test]
    fn junk_lines_are_skipped() {
        assert!(parse_mountinfo_line("").is_none());
        assert!(
            parse_mountinfo_line("1 2 3:4 / /mnt rw").is_none(),
            "no separator"
        );
        assert!(
            parse_mountinfo_line("1 2 - ext4 x").is_none(),
            "too few head fields"
        );
    }

    #[test]
    fn octal_escapes_are_decoded() {
        assert_eq!(unescape("/home/u/My\\040Documents"), "/home/u/My Documents");
        assert_eq!(unescape("/a\\011b"), "/a\tb");
        assert_eq!(unescape("/a\\012b"), "/a\nb");
        // \\134 is how the kernel spells a backslash that is part of the name.
        assert_eq!(unescape("/a\\134b"), "/a\\b");
        assert_eq!(unescape("/plain"), "/plain");
        assert_eq!(unescape("/a\\099b"), "/a\\099b", "not octal, left alone");
        assert_eq!(
            unescape("/trailing\\"),
            "/trailing\\",
            "too short to decode"
        );
        // The kernel escapes every byte above 0x7E, so a UTF-8 mount point arrives
        // as a run of escapes that has to be reassembled before decoding.
        assert_eq!(unescape("/caf\\303\\251"), "/caf\u{e9}");
    }

    #[test]
    fn the_real_root_resolves_and_caches() {
        let info = FsInfo::new();
        let h = info.hints(Path::new("/"));
        if let (Some(total), Some(free)) = (h.total, h.free) {
            assert!(free <= total || h.remote, "{free} > {total}");
        }
        assert_eq!(
            info.hints(Path::new("/")),
            h,
            "the second call is served from cache"
        );
        info.invalidate(Path::new("/"));
        assert!(info.lock().is_empty());
    }

    #[test]
    fn a_missing_path_probes_without_panicking() {
        let info = FsInfo::new();
        let h = info.hints(Path::new("/nonexistent-path-for-ufm-tests"));
        assert_eq!(h.total, None, "a path that does not exist reports no usage");
    }

    #[test]
    fn nested_mounts_resolve_to_the_deeper_one() {
        // The rule is checked against a fixture rather than the live table: a
        // build chroot has no mount namespace of its own, so the kernel lists
        // the host's mount points, and a chroot cannot arrange for a nested
        // pair to exist exactly where the test wants them.
        let fixture = "\
17 25 8:1 / / rw,relatime shared:1 - ext4 /dev/sda1 rw
22 17 8:3 / /home/u/data rw,relatime shared:2 - ext4 /dev/sda3 rw
31 22 253:0 / /home/u/data/deeper ro,relatime shared:3 - btrfs /dev/mapper/v-root ro
";
        let parsed = parse_mounts(fixture);
        assert_eq!(parsed.len(), 3, "the fixture itself parses");

        let under = deepest(parsed.clone(), Path::new("/home/u/data/deeper/report.txt"));
        assert_eq!(
            under.map(|m| m.path),
            Some(PathBuf::from("/home/u/data/deeper")),
            "a nested mount wins over its parents"
        );
        let between = deepest(parsed.clone(), Path::new("/home/u/data/report.txt"));
        assert_eq!(
            between.map(|m| m.path),
            Some(PathBuf::from("/home/u/data")),
            "one level up resolves to the middle mount"
        );
        let outside = deepest(parsed, Path::new("/etc/passwd"));
        assert_eq!(
            outside.map(|m| m.path),
            Some(PathBuf::from("/")),
            "a path under nothing else still resolves to the root mount"
        );

        // The live table only has to survive parsing, and only where /proc is
        // mounted at all. Whether `/` is in it is a fact about the mount
        // namespace, not about this parser.
        if Path::new("/proc/self/mountinfo").exists() {
            let live = mounts();
            assert!(!live.is_empty(), "/proc/self/mountinfo should be readable");
            assert!(live.iter().all(|m| m.path.is_absolute()));
        }
    }
}
