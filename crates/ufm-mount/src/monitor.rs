//! Whether a mount the table remembers is still a mount.
//!
//! The table records intent; the kernel holds the truth, and the two disagree
//! whenever a FUSE daemon dies, a laptop sleeps for a day, or an administrator
//! unmounts something by hand. This module reads `/proc/self/mountinfo` and
//! answers one question per mount point: is it mounted, is it a dead mount that
//! has to be detached, or is it gone.
//!
//! The mountinfo parser here is a second copy of the one in `ufm-vfs`'s
//! `fsinfo`, deliberately. `ufm-mount` has to stay usable by the privileged
//! helper and by `--sweep-mounts`, neither of which should pull in the directory
//! cache, the inotify watcher, or anything else a browsing session needs.
//!
//! Probing is limited to FUSE. A dead `sshfs` or `rclone` makes its mount point
//! answer `ENOTCONN` immediately, which is a cheap and honest answer. A dead NFS
//! server makes `stat()` wait for an RPC timeout, and a sweep that hangs for a
//! minute holding a lock is worse than one that reports a live mount a few
//! seconds too optimistically.

use std::path::{Path, PathBuf};

use ufm_core::error::Error;

/// Where the kernel publishes the mount table. Injectable so a test can hand
/// [`Observer`] a synthetic table instead of needing a real mount.
const MOUNTINFO: &str = "/proc/self/mountinfo";

/// One entry of `/proc/self/mountinfo`, as far as this module cares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountPoint {
    pub path: PathBuf,
    pub fstype: String,
    /// What the mount was made from: `//srv/share`, `host:/export`, a URL. Used
    /// to build an unmount plan for a mount the table does not know about.
    pub source: String,
    pub read_only: bool,
}

impl MountPoint {
    /// Whether this filesystem is served by a userspace daemon, which is what
    /// makes a liveness probe both possible and safe. See the module comment.
    #[must_use]
    pub fn fuse(&self) -> bool {
        self.fstype.starts_with("fuse")
    }
}

/// The answer about one mount point.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MountState {
    /// The kernel has it and, for FUSE, the daemon answers.
    Mounted,
    /// The kernel has an entry but nothing is behind it: a FUSE daemon that
    /// died, or a mount point whose directory was removed while mounted. Browsing
    /// it yields "Transport endpoint is not connected", so it has to be detached
    /// before it can be used again.
    Stale,
    /// Nothing is mounted there. The directory may or may not exist.
    Absent,
}

impl MountState {
    /// Whether browsing this mount point should work.
    #[must_use]
    pub const fn alive(self) -> bool {
        matches!(self, Self::Mounted)
    }

    /// Whether something has to be detached before the mount point is usable.
    #[must_use]
    pub const fn needs_detach(self) -> bool {
        matches!(self, Self::Stale)
    }

    /// For the badge next to a share in the Network group.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Mounted => "Connected",
            Self::Stale => "Disconnected",
            Self::Absent => "Not connected",
        }
    }
}

/// The mount table, read once and queried many times. A refresh costs one file
/// read, so the Network group can rebuild its badges on a timer without asking
/// the kernel anything else.
#[derive(Debug, Clone)]
pub struct Observer {
    mountinfo: PathBuf,
    points: Vec<MountPoint>,
}

impl Default for Observer {
    fn default() -> Self {
        Self::new()
    }
}

impl Observer {
    /// Read the real mount table. A system whose `/proc` is not there yields an
    /// observer that reports everything absent, which is the honest answer.
    #[must_use]
    pub fn new() -> Self {
        Self::at(MOUNTINFO)
    }

    /// Read the mount table from somewhere else: a test fixture, or a container
    /// where the real one is mounted elsewhere.
    #[must_use]
    pub fn at(mountinfo: impl Into<PathBuf>) -> Self {
        let mountinfo = mountinfo.into();
        let points = read(&mountinfo);
        Self { mountinfo, points }
    }

    /// Re-read, after a mount or an unmount.
    pub fn reload(&mut self) {
        self.points = read(&self.mountinfo);
    }

    /// Every entry, for diagnostics.
    #[must_use]
    pub fn points(&self) -> &[MountPoint] {
        &self.points
    }

    /// The entry for exactly this mount point, after resolving symlinks: a
    /// mount root reached through a symlink is still the same filesystem.
    #[must_use]
    pub fn find(&self, mountpoint: &Path) -> Option<&MountPoint> {
        let resolved =
            std::fs::canonicalize(mountpoint).unwrap_or_else(|_| mountpoint.to_path_buf());
        self.points
            .iter()
            .find(|point| point.path == mountpoint || point.path == resolved)
    }

    /// The state of one mount point.
    #[must_use]
    pub fn state(&self, mountpoint: &Path) -> MountState {
        match self.find(mountpoint) {
            None => MountState::Absent,
            Some(point) if !point.fuse() => MountState::Mounted,
            Some(_) => probe(mountpoint),
        }
    }

    /// Whether browsing this mount point should work.
    #[must_use]
    pub fn mounted(&self, mountpoint: &Path) -> bool {
        self.state(mountpoint).alive()
    }
}

/// The state of one mount point, reading the mount table fresh. Use an
/// [`Observer`] when checking more than one.
#[must_use]
pub fn observe(mountpoint: &Path) -> MountState {
    Observer::new().state(mountpoint)
}

/// The states of several mount points for one read of the mount table, in the
/// order asked for.
#[must_use]
pub fn observe_all(mountpoints: &[&Path]) -> Vec<MountState> {
    let observer = Observer::new();
    mountpoints
        .iter()
        .map(|mountpoint| observer.state(mountpoint))
        .collect()
}

fn read(mountinfo: &Path) -> Vec<MountPoint> {
    let Ok(text) = std::fs::read_to_string(mountinfo) else {
        return Vec::new();
    };
    parse(&text)
}

/// `36 35 0:34 / /run/user/1000/ufm/srv-share rw,nosuid - fuse.sshfs host:/data rw,user_id=1000`
///
/// The mount point is field 5 of the head; the type and source are the three
/// fields after the `-` separator, because the number of optional fields before
/// it varies with namespaces and propagation.
#[must_use]
pub fn parse(text: &str) -> Vec<MountPoint> {
    text.lines().filter_map(parse_line).collect()
}

fn parse_line(line: &str) -> Option<MountPoint> {
    let fields: Vec<&str> = line.split_whitespace().collect();
    let sep = fields.iter().position(|f| *f == "-")?;
    if sep < 5 || fields.len() < sep + 4 {
        return None;
    }
    let head_options = fields[5];
    let super_options = fields[sep + 3];
    Some(MountPoint {
        path: PathBuf::from(unescape(fields[4])),
        fstype: unescape(fields[sep + 1]),
        source: unescape(fields[sep + 2]),
        read_only: head_options
            .split(',')
            .chain(super_options.split(','))
            .any(|option| option == "ro"),
    })
}

/// A FUSE mount point that answers is alive; one that errors is a leftover.
/// Anything else — a permission error, say, from a mount another user made — is
/// treated as alive, because the mount clearly exists and only our view of it is
/// restricted.
fn probe(mountpoint: &Path) -> MountState {
    match std::fs::symlink_metadata(mountpoint) {
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::NotConnected | std::io::ErrorKind::NotFound
            ) =>
        {
            MountState::Stale
        }
        // Including every other error: a permission error means the mount is
        // there and belongs to someone else.
        _ => MountState::Mounted,
    }
}

/// mountinfo escapes a space, tab, newline, backslash and anything above `0x7E`
/// as three octal digits, which would otherwise hide a share mounted under
/// `~/My Documents`. A backslash not followed by three octal digits is left as
/// written, so a path that really contains one survives the round trip.
fn unescape(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
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
    // Mountinfo escapes are always exactly three octal digits; a shorter run is
    // a truncated line, and reading it as a value would invent a character.
    let [a, b, c] = *digits else {
        return None;
    };
    let mut value: u16 = 0;
    for digit in [a, b, c] {
        let d = digit.checked_sub(b'0')?;
        if d > 7 {
            return None;
        }
        value = value * 8 + u16::from(d);
    }
    u8::try_from(value).ok()
}

/// The mount point a source was mounted at, if any — used by the sweep to tell
/// a share that is already connected from one that needs connecting.
pub fn mounted_from(text: &str, source: &str) -> Option<PathBuf> {
    parse(text)
        .into_iter()
        .find(|point| point.source == source)
        .map(|point| point.path)
}

/// Fail with the message a UI can show, when a mount root cannot be created
/// or a mount point cannot be detached.
// Used by the mount runner in `uflinux-files`, which has no test coverage yet.
#[allow(dead_code)]
pub(crate) fn cannot(path: &Path, reason: &std::io::Error) -> Error {
    Error::Mount(format!("cannot use {}: {reason}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SSHFS: &str = "36 35 0:34 / /run/user/1000/ufm/host-data rw,nosuid,nodev shared:2 - \
                         fuse.sshfs host:/data rw,user_id=1000";
    const NFS: &str = "41 22 0:41 / /run/user/1000/ufm/srv-export rw,relatime - nfs \
                       srv:/export rw,vers=4.2";

    #[test]
    fn a_fuse_entry_is_parsed_with_its_source() {
        let points = parse(SSHFS);
        assert_eq!(points.len(), 1);
        let point = &points[0];
        assert_eq!(point.path, PathBuf::from("/run/user/1000/ufm/host-data"));
        assert_eq!(point.fstype, "fuse.sshfs");
        assert_eq!(point.source, "host:/data");
        assert!(point.fuse());
        assert!(!point.read_only);
    }

    #[test]
    fn a_kernel_entry_is_parsed_and_read_only_is_seen() {
        let points = parse(NFS);
        assert_eq!(points.len(), 1);
        assert_eq!(points[0].fstype, "nfs");
        assert_eq!(points[0].source, "srv:/export");
        assert!(!points[0].fuse());
        let ro = format!("{} ro", NFS.replace(" rw,vers=4.2", " ro,vers=4.2"));
        assert!(parse(&ro)[0].read_only);
    }

    #[test]
    fn a_line_without_a_separator_or_too_few_fields_is_ignored() {
        assert!(parse("36 35 0:34 / /mnt rw").is_empty());
        assert!(parse("0 0 0:0 / - x").is_empty());
        assert!(parse("").is_empty());
        assert!(parse("not a mount line at all").is_empty());
    }

    #[test]
    fn optional_fields_of_any_length_are_skipped() {
        let long = "36 35 0:34 / /mnt rw shared:2 master:3 - fuse.rclone webdav:files rw";
        let points = parse(long);
        assert_eq!(points.len(), 1);
        assert_eq!(points[0].fstype, "fuse.rclone");
        assert_eq!(points[0].source, "webdav:files");
    }

    #[test]
    fn an_escaped_mount_point_is_decoded() {
        let line = "36 35 0:34 / /run/user/1000/ufm/My\\040Documents rw - fuse.sshfs h:/d rw";
        let points = parse(line);
        assert_eq!(
            points[0].path,
            PathBuf::from("/run/user/1000/ufm/My Documents")
        );
        // A stray backslash is not an escape, and is left alone.
        let odd = "36 35 0:34 / /mnt\\x rw - fuse.sshfs h:/d rw";
        assert_eq!(parse(odd)[0].path, PathBuf::from("/mnt\\x"));
    }

    #[test]
    fn an_observer_reports_what_the_mount_table_says() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mounted = dir.path().join("host-data");
        std::fs::create_dir(&mounted).expect("mkdir");
        let mut points = parse(&format!("{SSHFS}\n{NFS}\n"));
        // Point the entries at directories inside the temporary directory, so the
        // probe runs against something that exists here.
        points[0].path = mounted.clone();
        points[1].path = dir.path().join("srv-export");
        let observer = Observer {
            mountinfo: dir.path().join("absent"),
            points,
        };
        assert_eq!(observer.points().len(), 2);
        // The FUSE mount point exists, so the probe is satisfied; the kernel entry
        // is trusted without probing at all.
        assert_eq!(observer.state(&mounted), MountState::Mounted);
        assert!(observer.mounted(&mounted));
        assert_eq!(
            observer.state(&dir.path().join("nowhere")),
            MountState::Absent
        );
    }

    #[test]
    fn a_fuse_mount_point_that_vanished_is_stale_not_absent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let gone = dir.path().join("dead");
        let fixture = dir.path().join("mountinfo");
        std::fs::write(
            &fixture,
            format!(
                "36 35 0:34 / {} rw - fuse.sshfs host:/data rw\n",
                gone.display()
            ),
        )
        .expect("write");
        let observer = Observer::at(&fixture);
        assert_eq!(observer.state(&gone), MountState::Stale);
        assert!(observer.state(&gone).needs_detach());
        assert_eq!(observer.state(&gone).label(), "Disconnected");
    }

    #[test]
    fn a_kernel_mount_with_no_directory_is_still_reported_mounted() {
        // Probing a dead NFS mount can block for an RPC timeout, so the entry is
        // trusted and the directory's absence is not treated as evidence.
        let dir = tempfile::tempdir().expect("tempdir");
        let gone = dir.path().join("nfs");
        let fixture = dir.path().join("mountinfo");
        std::fs::write(
            &fixture,
            format!("41 22 0:41 / {} rw - nfs srv:/export rw\n", gone.display()),
        )
        .expect("write");
        assert_eq!(Observer::at(&fixture).state(&gone), MountState::Mounted);
    }

    #[test]
    fn a_missing_mountinfo_yields_no_entries_rather_than_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let observer = Observer::at(dir.path().join("absent"));
        assert!(observer.points().is_empty());
        assert_eq!(
            observer.state(Path::new("/run/user/1000/ufm/x")),
            MountState::Absent
        );
    }

    #[test]
    fn the_real_mount_table_parses() {
        if !Path::new(MOUNTINFO).exists() {
            return;
        }
        let text = std::fs::read_to_string(MOUNTINFO).expect("mountinfo");
        let points = parse(&text);
        assert!(!points.is_empty(), "no mount points in a real mountinfo");
        assert!(points.iter().all(|p| p.path.is_absolute()));
        assert!(observe(Path::new("/")).alive(), "/ should be mounted");
    }

    #[test]
    fn a_source_can_be_matched_back_to_its_mount_point() {
        let path = mounted_from(&format!("{NFS}\n"), "srv:/export").expect("found");
        assert_eq!(path, PathBuf::from("/run/user/1000/ufm/srv-export"));
        assert!(mounted_from(NFS, "other:/export").is_none());
    }

    #[test]
    fn octal_escapes_only_accept_three_octal_digits() {
        assert_eq!(octal(b"040"), Some(b' '));
        assert_eq!(octal(b"012"), Some(10));
        assert_eq!(octal(b"400"), None, "0o400 does not fit in a byte");
        assert_eq!(octal(b"08x"), None);
        assert_eq!(octal(b"12"), None, "too few digits");
    }
}
