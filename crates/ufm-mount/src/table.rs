//! The table of shares the user has connected to.
//!
//! One JSON file, `<config dir>/mounts.json`, rewritten whole through
//! [`ufm_core::atomic`] so a crash cannot lose the list. It records intent
//! rather than state: whether a mount is actually alive is answered by
//! [`crate::monitor`], because the kernel can drop a mount at any time and a
//! JSON file has no way to notice.
//!
//! A record stores the [`Share`], not the URI that produced it, so a password
//! cannot survive a save. Label, mount point and protocol are all derivable from
//! the share, which keeps the file small and removes any chance of the two
//! disagreeing.

use std::path::{Path, PathBuf};
use std::slice;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use ufm_core::atomic::write_json;
use ufm_core::error::{Error, Result};
use ufm_core::paths;

use crate::uri::{Protocol, Share};

const VERSION: u32 = 1;
const FILE: &str = "mounts.json";

/// One connected share.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MountRecord {
    /// The share's slug: stable, unique, and the same string the mount point
    /// ends with, so a listing of the mount root and the table always agree.
    pub id: String,
    pub share: Share,
    /// Where the filesystem is attached: `<mount root>/<slug>`.
    pub mountpoint: PathBuf,
    /// Connect automatically once the session starts.
    #[serde(default)]
    pub autostart: bool,
    /// Seconds since the epoch of the last successful mount.
    #[serde(default)]
    pub mounted_at: Option<u64>,
    /// Seconds since the epoch of the last time a listing was read through it.
    /// Auto-unmount decides idleness from this rather than `mounted_at`, so a
    /// share being browsed all day stays up.
    #[serde(default)]
    pub last_used: Option<u64>,
    /// Why the last mount attempt failed, so the Network group can show a reason
    /// instead of a card that silently does nothing. Cleared on success.
    #[serde(default)]
    pub last_error: Option<String>,
}

impl MountRecord {
    fn new(share: &Share, mount_root: &Path) -> Self {
        Self {
            id: share.slug(),
            share: share.clone(),
            mountpoint: mount_root.join(share.slug()),
            autostart: false,
            mounted_at: None,
            last_used: None,
            last_error: None,
        }
    }

    #[must_use]
    pub fn protocol(&self) -> Protocol {
        self.share.protocol
    }

    #[must_use]
    pub fn label(&self) -> String {
        self.share.label()
    }

    /// Mark a successful mount, which also clears any earlier failure.
    pub fn record_mount(&mut self, at: u64) {
        self.mounted_at = Some(at);
        self.last_used = Some(at);
        self.last_error = None;
    }

    /// Mark a failed attempt. The message must already be safe to show.
    pub fn record_failure(&mut self, at: u64, message: impl Into<String>) {
        self.last_used = Some(at);
        self.last_error = Some(message.into());
    }

    /// Seconds since `last_used`, or `None` when the share has never been used.
    #[must_use]
    pub fn idle_since(&self, now: u64) -> Option<u64> {
        self.last_used.map(|then| now.saturating_sub(then))
    }
}

/// Every share the user has connected to, in the order they added them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountTable {
    mounts: Vec<MountRecord>,
    /// Where this table is stored. Kept in the value so `save` needs no
    /// arguments and a test can point a table at a temporary directory.
    config: PathBuf,
    /// Where mount points are created. Deliberately not stored: `$XDG_RUNTIME_DIR`
    /// is per boot, so a mount point read from disk is rewritten on load.
    mount_root: PathBuf,
}

/// The table is a list of records first of all, so iterating it directly is the
/// natural thing to do.
impl<'a> IntoIterator for &'a MountTable {
    type Item = &'a MountRecord;
    type IntoIter = slice::Iter<'a, MountRecord>;

    fn into_iter(self) -> Self::IntoIter {
        self.mounts.iter()
    }
}

impl Default for MountTable {
    fn default() -> Self {
        Self::new(paths::config_dir(), paths::mount_root())
    }
}

impl MountTable {
    #[must_use]
    pub fn new(config: impl Into<PathBuf>, mount_root: impl Into<PathBuf>) -> Self {
        Self {
            mounts: Vec::new(),
            config: config.into(),
            mount_root: mount_root.into(),
        }
    }

    /// Read the table from the user's configuration directory. A file that
    /// exists but does not parse yields an empty table; use [`try_load`] to
    /// report it as a banner instead of losing the shares in silence.
    #[must_use]
    pub fn load() -> Self {
        Self::try_load().unwrap_or_default()
    }

    /// Read the table, keeping the parse error.
    pub fn try_load() -> Result<Self> {
        Self::try_load_at(&paths::config_dir(), &paths::mount_root())
    }

    /// Read a table rooted somewhere else, for tests and for `--sweep-mounts`,
    /// which runs from a service unit without a desktop session.
    pub fn try_load_at(config: &Path, mount_root: &Path) -> Result<Self> {
        let path = config.join(FILE);
        let stored: Stored = ufm_core::atomic::read_json(&path)?;
        let mut table = Self::new(config, mount_root);
        table.mounts = stored.mounts;
        for record in &mut table.mounts {
            record.mountpoint = mount_root.join(&record.id);
        }
        Ok(table)
    }

    #[must_use]
    pub fn path() -> PathBuf {
        paths::config_dir().join(FILE)
    }

    #[must_use]
    pub fn path_at(config: &Path) -> PathBuf {
        config.join(FILE)
    }

    /// The file this table will be written to.
    #[must_use]
    pub fn path_to(&self) -> PathBuf {
        self.config.join(FILE)
    }

    /// The directory the table is stored in.
    #[must_use]
    pub fn config(&self) -> &Path {
        &self.config
    }

    #[must_use]
    pub fn mount_root(&self) -> &Path {
        &self.mount_root
    }

    /// The records. `mounts_mut` exists for the sweep and for tests; prefer the
    /// mutating methods below, which keep `id` and `mountpoint` consistent.
    #[must_use]
    pub fn mounts(&self) -> &[MountRecord] {
        &self.mounts
    }

    #[must_use]
    pub fn mounts_mut(&mut self) -> &mut Vec<MountRecord> {
        &mut self.mounts
    }

    /// Iterate the records in display order. `&MountTable` is also `IntoIterator`,
    /// so `for record in &table` works.
    pub fn iter(&self) -> slice::Iter<'_, MountRecord> {
        self.mounts.iter()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.mounts.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.mounts.is_empty()
    }

    #[must_use]
    pub fn get(&self, id: &str) -> Option<&MountRecord> {
        self.mounts.iter().find(|m| m.id == id)
    }

    /// The record for a share, matched on its address without the password.
    #[must_use]
    pub fn find(&self, share: &Share) -> Option<&MountRecord> {
        self.mounts
            .iter()
            .find(|m| m.share.display() == share.display())
    }

    /// Whether a share is already in the table, which lets the connect dialog
    /// say "already added" instead of producing a second card.
    #[must_use]
    pub fn contains(&self, share: &Share) -> bool {
        self.find(share).is_some()
    }

    /// The record whose mount point is `path` or lies under it, deepest first.
    /// Used to badge a folder as remote and to offer Eject in the right places.
    #[must_use]
    pub fn for_path(&self, path: &Path) -> Option<&MountRecord> {
        self.mounts
            .iter()
            .filter(|m| path.starts_with(&m.mountpoint))
            .max_by_key(|m| m.mountpoint.components().count())
    }

    /// Records that should be connected when the session starts.
    #[must_use]
    pub fn autostart(&self) -> Vec<&MountRecord> {
        self.mounts.iter().filter(|m| m.autostart).collect()
    }

    /// Every mount point in the table, for the stale sweep.
    #[must_use]
    pub fn mountpoints(&self) -> Vec<PathBuf> {
        self.mounts.iter().map(|m| m.mountpoint.clone()).collect()
    }

    pub fn save(&self) -> Result<()> {
        write_json(
            &self.path_to(),
            &Snapshot {
                version: VERSION,
                mounts: &self.mounts,
            },
        )
    }

    /// Add or refresh a share, returning its record.
    ///
    /// Adding a share that is already there keeps its position, its `autostart`
    /// flag and its recorded failure, and replaces only what a user editing that
    /// entry would expect to change.
    pub fn add(&mut self, share: &Share) -> &mut MountRecord {
        if let Some(index) = self
            .mounts
            .iter()
            .position(|m| m.share.display() == share.display())
        {
            self.mounts[index].share = share.clone();
            return &mut self.mounts[index];
        }
        self.mounts.push(MountRecord::new(share, &self.mount_root));
        self.mounts.last_mut().expect("just pushed")
    }

    /// Set or clear a record's autostart flag. Returns whether it changed.
    pub fn set_autostart(&mut self, id: &str, autostart: bool) -> bool {
        match self.mounts.iter_mut().find(|m| m.id == id) {
            Some(record) if record.autostart != autostart => {
                record.autostart = autostart;
                true
            }
            _ => false,
        }
    }

    /// Touch the idle clock, so a share being browsed is not unmounted.
    pub fn touch(&mut self, id: &str) -> bool {
        self.with(id, |record| record.last_used = Some(now()))
    }

    pub fn record_mount(&mut self, id: &str) -> bool {
        self.with(id, |record| record.record_mount(now()))
    }

    pub fn record_failure(&mut self, id: &str, message: impl Into<String>) -> bool {
        let message = message.into();
        self.with(id, move |record| record.record_failure(now(), message))
    }

    fn with(&mut self, id: &str, f: impl FnOnce(&mut MountRecord)) -> bool {
        match self.mounts.iter_mut().find(|m| m.id == id) {
            Some(record) => {
                f(record);
                true
            }
            None => false,
        }
    }

    /// Drop a share from the table. The caller unmounts first: a record removed
    /// while still mounted leaves an orphan nobody can eject.
    pub fn remove(&mut self, id: &str) -> Option<MountRecord> {
        let index = self.mounts.iter().position(|m| m.id == id)?;
        Some(self.mounts.remove(index))
    }

    /// Move a record, which is how the Network group is ordered.
    pub fn move_item(&mut self, from: usize, to: usize) -> Result<()> {
        if from >= self.mounts.len() || to >= self.mounts.len() {
            return Err(Error::Config(format!(
                "cannot move mount {from} to {to}: only {} mounts",
                self.mounts.len()
            )));
        }
        let record = self.mounts.remove(from);
        self.mounts.insert(to, record);
        Ok(())
    }
}

/// The on-disk shape. Reading is lenient — `version` is recorded but not
/// enforced, and unknown fields are dropped, so a downgrade keeps whatever it
/// understands. Writing always stamps the running build's version.
#[derive(Debug, Default, Deserialize)]
struct Stored {
    #[serde(default)]
    #[allow(dead_code)]
    version: u32,
    #[serde(default)]
    mounts: Vec<MountRecord>,
}

#[derive(Debug, Serialize)]
struct Snapshot<'a> {
    version: u32,
    mounts: &'a [MountRecord],
}

/// Seconds since the epoch, clamped. A clock set before 1970 reports 0 rather
/// than wrapping into an enormous idle time that unmounts everything at once.
#[must_use]
pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Where a share should be mounted under `mount_root`. The GUI asks for this so
/// the path a card shows matches what the backend will use.
#[must_use]
pub fn mountpoint_for(share: &Share, mount_root: &Path) -> PathBuf {
    mount_root.join(share.slug())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn share(input: &str) -> Share {
        Share::parse(input).unwrap().share
    }

    /// A table whose file and mount root are both inside a temp dir, so no test
    /// touches the real configuration or needs `$XDG_RUNTIME_DIR`. The name only
    /// labels the subtree, to keep a failure readable.
    fn table(name: &str) -> (tempfile::TempDir, MountTable) {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = dir.path().join(name).join("config");
        std::fs::create_dir_all(&config).expect("config dir");
        let root = dir.path().join(name).join("mnt");
        (dir, MountTable::new(config, root))
    }

    #[test]
    fn an_absent_table_is_empty_not_an_error() {
        let (_dir, t) = table("absent");
        assert!(t.is_empty());
        assert_eq!(t.len(), 0);
        assert!(t.mountpoints().is_empty());
    }

    #[test]
    fn adding_assigns_a_stable_id_and_a_mount_point_under_the_root() {
        let (_dir, mut t) = table("add");
        let s = share("smb://filesrv/Users/alice");
        let id = s.slug();
        let root = t.mount_root().join(&id);
        let label = {
            let record = t.add(&s);
            assert_eq!(record.id, id);
            assert_eq!(record.mountpoint, root);
            record.label()
        };
        assert_eq!(label, "alice");
        assert!(!t.mounts()[0].autostart);
        assert!(t.mounts()[0].last_error.is_none());
        assert!(t.mounts()[0].mounted_at.is_none());
    }

    #[test]
    fn adding_the_same_share_twice_edits_it_in_place() {
        let (_dir, mut t) = table("dup");
        t.add(&share("smb://srv/share"));
        let other = share("smb://srv/share?vers=2.0");
        t.add(&other);
        assert_eq!(t.len(), 1);
        assert_eq!(t.mounts()[0].share.option("vers"), Some("2.0"));
        assert!(t.contains(&other));
    }

    #[test]
    fn two_shares_with_the_same_leaf_get_different_mount_points() {
        let (_dir, mut t) = table("collide");
        t.add(&share("smb://a/share"));
        t.add(&share("smb://b/share"));
        assert_eq!(t.len(), 2);
        let points: Vec<_> = t.mounts().iter().map(|m| m.mountpoint.clone()).collect();
        assert_ne!(points[0], points[1], "{points:?}");
        assert!(points.iter().all(|p| p.starts_with(t.mount_root())));
    }

    #[test]
    fn a_round_trip_through_disk_keeps_the_records() {
        let (_dir, mut t) = table("round");
        let s = share("sftp://alice@host:2222/data");
        let id = s.slug();
        t.add(&s);
        t.set_autostart(&id, true);
        t.record_mount(&id);
        t.save().expect("save");
        let meta = std::fs::metadata(t.path_to()).expect("meta");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(meta.permissions().mode() & 0o777, 0o600);
        }
        let back = MountTable::try_load_at(t.config(), t.mount_root()).expect("reload");
        assert_eq!(back.len(), 1);
        assert_eq!(back.mounts()[0].share, s);
        assert!(back.mounts()[0].autostart);
        assert!(back.mounts()[0].mounted_at.is_some());
        assert_eq!(back.find(&s).map(|m| m.id.as_str()), Some(id.as_str()));
    }

    #[test]
    fn a_stored_mount_point_is_replaced_by_the_current_root() {
        let (_dir, t) = table("root");
        std::fs::write(
            MountTable::path_at(t.config()),
            r#"{"version":1,"mounts":[{"id":"srv-share","share":{"protocol":"smb","host":"srv","port":null,"user":null,"tls":false,"resource":"/share","options":[]},"mountpoint":"/run/user/9999/ufm/old","autostart":true}]}"#,
        )
        .expect("write");
        let back = MountTable::try_load_at(t.config(), t.mount_root()).expect("load");
        assert_eq!(
            back.mounts()[0].mountpoint,
            t.mount_root().join("srv-share")
        );
        assert!(back.mounts()[0].autostart);
    }

    #[test]
    fn a_corrupt_table_is_an_error_that_keeps_the_file() {
        let (_dir, t) = table("corrupt");
        std::fs::write(MountTable::path_at(t.config()), "{oops").expect("write");
        let err = MountTable::try_load_at(t.config(), t.mount_root()).unwrap_err();
        assert!(matches!(err, Error::Config(_)), "{err}");
        assert!(std::fs::exists(MountTable::path_at(t.config())).unwrap());
        // The lenient loader still starts, so a bad file cannot block the app.
        assert!(
            MountTable::try_load_at(t.config(), t.mount_root())
                .err()
                .is_some()
        );
    }

    #[test]
    fn looking_up_by_path_picks_the_deepest_mount() {
        let (_dir, mut t) = table("path");
        let outer = t.add(&share("nfs://host/export")).id.clone();
        let inner = t.mount_root().join("nested-share");
        t.mounts_mut().push(MountRecord {
            id: "nested-share".into(),
            share: share("nfs://host/export/inner"),
            mountpoint: inner.clone(),
            autostart: false,
            mounted_at: None,
            last_used: None,
            last_error: None,
        });
        let deep = inner.join("a/b");
        assert_eq!(
            t.for_path(&deep).map(|m| m.id.as_str()),
            Some("nested-share")
        );
        let shallow = t.mount_root().join(&outer).join("x");
        assert_eq!(
            t.for_path(&shallow).map(|m| m.id.as_str()),
            Some(outer.as_str())
        );
        assert!(t.for_path(Path::new("/home/user")).is_none());
    }

    #[test]
    fn autostart_failures_and_idle_are_tracked() {
        let (_dir, mut t) = table("idle");
        let s = share("smb://srv/share");
        let id = s.slug();
        t.add(&s);
        assert!(t.set_autostart(&id, true));
        assert!(!t.set_autostart(&id, true));
        assert_eq!(t.autostart().len(), 1);
        assert!(t.get(&id).unwrap().idle_since(now() + 100).is_none());
        t.record_mount(&id);
        let idle = t.get(&id).unwrap().idle_since(now() + 100).unwrap();
        assert!((100..110).contains(&idle), "{idle}");
        assert!(t.get(&id).unwrap().last_error.is_none());
        t.record_failure(&id, "host not found");
        assert_eq!(
            t.get(&id).unwrap().last_error.as_deref(),
            Some("host not found")
        );
        assert!(t.touch(&id));
        assert!(!t.touch("nope"));
        assert!(!t.set_autostart("nope", true));
    }

    #[test]
    fn removing_and_reordering_are_positional() {
        let (_dir, mut t) = table("order");
        let a = share("smb://a/one");
        let b = share("smb://b/two");
        let c = share("smb://c/three");
        for s in [&a, &b, &c] {
            t.add(s);
        }
        assert_eq!(t.mounts()[0].share, a);
        t.move_item(0, 2).expect("move");
        assert_eq!(t.mounts()[2].share, a);
        assert!(t.move_item(9, 0).is_err());
        assert!(t.remove(&b.slug()).is_some());
        assert_eq!(t.len(), 2);
        assert!(t.remove(&b.slug()).is_none());
        assert_eq!(t.mountpoints().len(), 2);
    }

    #[test]
    fn a_password_never_reaches_the_table() {
        let (_dir, mut t) = table("secret");
        let parsed = Share::parse("webdav://alice:hunter2@host/files").unwrap();
        t.add(&parsed.share);
        t.save().expect("save");
        let raw = std::fs::read_to_string(t.path_to()).expect("read");
        assert!(!raw.contains("hunter2"), "{raw}");
        // The share is stored as its parts, so there is no address form in the
        // file at all; the user is kept, the password has nowhere to be.
        assert!(raw.contains("\"user\": \"alice\""), "{raw}");
        assert!(!raw.contains("password"), "{raw}");
    }

    #[test]
    fn a_mount_point_is_derivable_without_a_table() {
        let s = share("smb://srv/share");
        assert_eq!(
            mountpoint_for(&s, Path::new("/run/user/1000/ufm")),
            PathBuf::from(format!("/run/user/1000/ufm/{}", s.slug()))
        );
    }
}
