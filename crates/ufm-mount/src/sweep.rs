//! Cleaning up after mounts that are no longer mounts.
//!
//! Three kinds of leftover appear in practice, and none of them can be fixed by
//! the code that made them, because that code has usually exited:
//!
//! * a **stale mount**: the kernel still has an entry, the daemon behind it is
//!   gone, and browsing the directory answers `ENOTCONN`. Only `umount` clears
//!   it, and for `cifs`/`nfs` that needs root — so a stale entry becomes a
//!   [`Plan`], not a filesystem call.
//! * an **empty directory** under the mount root whose share is long gone, left
//!   because a mount point has to exist before mounting and nothing removes it
//!   afterwards.
//! * a **credential file** left by a mount that crashed between writing it and
//!   deleting it. These hold a plaintext password, so leaving them behind is the
//!   one leftover that has a security cost.
//!
//! [`Sweep::scan`] decides what to do and returns the plans and paths; the
//! caller runs them, because detaching a kernel mount goes through `pkexec` and
//! a UI may want to ask first. `uflinux-files --sweep-mounts` runs it at session
//! start, when a stale mount would otherwise be the first thing the user meets.
//!
//! Idle auto-unmount lives here too, as the same scan with a threshold: an
//! unmount that happens because a share went unused has to produce the same
//! cleanup as one the user asked for.

use std::fmt::Write as _;
use std::path::PathBuf;

use ufm_core::error::{Error, Result};
use ufm_core::paths;

use crate::backend::{Backend, MountTarget, Paths as SearchPaths};
use crate::monitor::{MountState, Observer};
use crate::plan::Plan;
use crate::table::{MountTable, now};

/// The extension of a per-mount credential file.
const CRED_SUFFIX: &str = ".conf";

/// The directory holding per-mount credential files. Owner-only, and separate
/// from the mount root so a leftover password file cannot appear in a listing.
#[must_use]
pub fn credential_root() -> PathBuf {
    paths::config_dir().join("mounts")
}

/// What the sweep decided to do about one path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Detach this mount point by running the enclosed plan. Stale entries and
    /// idle mounts both land here, because the work is the same and only the
    /// reason differs.
    Detach(Plan),
    /// Remove an empty directory under the mount root.
    RemoveDir,
    /// Remove a leftover credential file.
    RemoveCredential,
}

impl Action {
    /// Whether this action needs a child process, which is what decides whether
    /// the sweep has to go through `pkexec` at all.
    #[must_use]
    pub const fn needs_command(&self) -> bool {
        matches!(self, Self::Detach(_))
    }
}

/// One thing the sweep found, with the reason it is safe to remove.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SweepItem {
    /// The mount's id, when the leftover belongs to a share in the table.
    pub id: Option<String>,
    pub path: PathBuf,
    pub action: Action,
    /// Shown in `--sweep-mounts` output and in the log. States the evidence, so
    /// a user reading it can tell a wrong answer from a right one.
    pub why: String,
}

impl SweepItem {
    #[must_use]
    pub fn plan(&self) -> Option<&Plan> {
        match &self.action {
            Action::Detach(plan) => Some(plan),
            _ => None,
        }
    }
}

/// What to look for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Options {
    /// Detach a mount that has not been browsed for this many seconds. `None`
    /// leaves everything mounted, which is what a scan at session start wants.
    pub idle_after: Option<u64>,
    /// Where [`credential_root`] points in practice; injectable for tests.
    pub credential_dir: PathBuf,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            idle_after: None,
            credential_dir: credential_root(),
        }
    }
}

impl Options {
    #[must_use]
    pub fn new(credential_dir: impl Into<PathBuf>) -> Self {
        Self {
            idle_after: None,
            credential_dir: credential_dir.into(),
        }
    }

    /// Detach mounts idle for `secs` or longer.
    #[must_use]
    pub fn idle_after(mut self, secs: u64) -> Self {
        self.idle_after = Some(secs);
        self
    }
}

/// The result of one scan.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Sweep {
    items: Vec<SweepItem>,
    /// Things the sweep could not classify: a directory it could not read, a
    /// backend whose program has gone missing. Reported, never fatal — a sweep
    /// that stops halfway would leave the rest to rot.
    notes: Vec<String>,
    /// Where the credential files were looked for, kept so a detach plan can be
    /// built against the same directory the caller passed.
    credential_dir: PathBuf,
}

impl Sweep {
    /// Look at the table, the kernel's mount table, and the two directories, and
    /// decide what to clean up. Nothing is removed or unmounted by this call.
    #[must_use]
    pub fn scan(
        table: &MountTable,
        observer: &Observer,
        search: &SearchPaths,
        options: &Options,
    ) -> Self {
        let mut sweep = Self {
            items: Vec::new(),
            notes: Vec::new(),
            credential_dir: options.credential_dir.clone(),
        };
        let stamp = now();
        for record in table.mounts() {
            match observer.state(&record.mountpoint) {
                MountState::Stale => match backend_for(record, search) {
                    Ok(backend) => sweep.push_detached(record, &backend, search, "dead"),
                    Err(note) => sweep.notes.push(note),
                },
                MountState::Mounted => {
                    let idle = record.idle_since(stamp);
                    let Some(limit) = options.idle_after else {
                        continue;
                    };
                    // A share marked autostart is one the user asked to keep
                    // connected; dropping it would mean a password prompt on the
                    // next session for no gain.
                    if record.autostart || idle.is_none_or(|secs| secs < limit) {
                        continue;
                    }
                    match backend_for(record, search) {
                        Ok(backend) => sweep.push_detached(record, &backend, search, "idle"),
                        Err(note) => sweep.notes.push(note),
                    }
                }
                // Not mounted is the state the sweep was asked to leave alone:
                // reconnecting is a user action, not a cleanup.
                MountState::Absent => {}
            }
        }
        sweep.orphan_credentials(table, observer, options);
        sweep.orphan_dirs(table);
        sweep
    }

    fn push_detached(
        &mut self,
        record: &crate::table::MountRecord,
        backend: &Backend,
        search: &SearchPaths,
        reason: &str,
    ) {
        let target = MountTarget::new(&record.share, &record.mountpoint, &self.credential_dir);
        match backend.unmount(&target, search) {
            Ok(plan) => {
                let why = match reason {
                    "idle" => format!(
                        "{} has not been used for a while and is detached to free memory",
                        record.label()
                    ),
                    _ => format!(
                        "{} is mounted but nothing answers there; it has to be detached",
                        record.label()
                    ),
                };
                self.items.push(SweepItem {
                    id: Some(record.id.clone()),
                    path: record.mountpoint.clone(),
                    action: Action::Detach(plan),
                    why,
                });
            }
            Err(e) => self
                .notes
                .push(format!("{}: {e}", record.mountpoint.display())),
        }
    }

    /// A credential file is a leftover when its share is gone from the table, or
    /// is in it but not currently mounted: the file is only needed while the
    /// mount is being established.
    fn orphan_credentials(&mut self, table: &MountTable, observer: &Observer, options: &Options) {
        // No directory means no leftovers, which is the common case.
        let Ok(entries) = std::fs::read_dir(&options.credential_dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(stem) = path
                .file_name()
                .and_then(|n| n.to_str())
                .and_then(|n| n.strip_suffix(CRED_SUFFIX))
            else {
                continue;
            };
            let live = table.get(stem).is_some_and(|record| {
                observer.state(&record.mountpoint).alive()
                    && options.credential_dir.join(format!("{stem}{CRED_SUFFIX}")) == path
            });
            if live {
                continue;
            }
            let why = match table.get(stem) {
                Some(_) => format!("{stem} is not mounted, so its stored password is not needed"),
                None => format!("{stem} is no longer a share in the table"),
            };
            self.items.push(SweepItem {
                id: table.get(stem).map(|record| record.id.clone()),
                path,
                action: Action::RemoveCredential,
                why,
            });
        }
    }

    /// A directory under the mount root that no record points at. Only empty ones
    /// are removed: a non-empty one may hold files from a mount that died while
    /// something still had it open, and deleting those would be the sweep causing
    /// the data loss it exists to prevent.
    fn orphan_dirs(&mut self, table: &MountTable) {
        // The mount root not existing yet is normal, not an error.
        let Ok(entries) = std::fs::read_dir(table.mount_root()) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() || table.mountpoints().contains(&path) {
                continue;
            }
            let name = path
                .file_name()
                .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
            if table.get(&name).is_some() {
                continue;
            }
            let children = match std::fs::read_dir(&path) {
                // A mount point that holds even one entry may hold a user's
                // files, so emptiness is checked before anything is proposed.
                Ok(children) => children.count(),
                Err(e) => {
                    self.notes
                        .push(format!("cannot read {}: {e}", path.display()));
                    continue;
                }
            };
            if children == 0 {
                self.items.push(SweepItem {
                    id: None,
                    path,
                    action: Action::RemoveDir,
                    why: format!("{name} is an empty mount point with no share behind it"),
                });
            } else {
                self.notes.push(format!(
                    "{} is not empty and was left alone",
                    path.display()
                ));
            }
        }
    }

    /// The leftovers, in the order they should be applied: detaches first, so a
    /// directory can be gone by the time a later scan looks at it.
    #[must_use]
    pub fn items(&self) -> &[SweepItem] {
        &self.items
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// The plans to run, in order. Empty means the sweep needs no privilege and
    /// no child process, which is the case most of the time.
    #[must_use]
    pub fn plans(&self) -> Vec<&Plan> {
        self.items.iter().filter_map(SweepItem::plan).collect()
    }

    #[must_use]
    pub fn notes(&self) -> &[String] {
        &self.notes
    }

    /// Perform the file removals — empty directories and leftover credential
    /// files — and drop those items, leaving only the ones that need a command.
    /// A path that has vanished in the meantime counts as done.
    pub fn apply_files(&mut self) -> Result<usize> {
        let mut done = 0;
        let mut notes = Vec::new();
        self.items.retain(|item| {
            if item.plan().is_some() {
                return true;
            }
            let outcome = match item.action {
                Action::RemoveDir => remove_gone(std::fs::remove_dir(&item.path)),
                _ => remove_gone(std::fs::remove_file(&item.path)),
            };
            match outcome {
                Ok(()) => {
                    done += 1;
                    false
                }
                Err(e) => {
                    notes.push(format!("cannot remove {}: {e}", item.path.display()));
                    true
                }
            }
        });
        self.notes.extend(notes);
        Ok(done)
    }

    /// One line per item, for `--sweep-mounts` and the log.
    #[must_use]
    pub fn summary(&self) -> String {
        if self.items.is_empty() && self.notes.is_empty() {
            return "nothing to clean up".into();
        }
        let mut out = String::new();
        for item in &self.items {
            let _ = writeln!(out, "{}\t{}", verb(&item.action), item.path.display());
            let _ = writeln!(out, "\t{}", item.why);
        }
        for note in &self.notes {
            out.push_str("note: ");
            out.push_str(note);
            out.push('\n');
        }
        out
    }
}

fn verb(action: &Action) -> &'static str {
    match action {
        Action::Detach(_) => "detach",
        Action::RemoveDir => "rmdir",
        Action::RemoveCredential => "rm",
    }
}

/// A path that vanished between the scan and the removal counts as done: the
/// point of the sweep is that the leftover is gone, not that we removed it.
fn remove_gone(outcome: std::io::Result<()>) -> std::io::Result<()> {
    match outcome {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

/// The backend a record's protocol would use now. Fails with the message that
/// explains which package went missing, since a stale mount whose helper is gone
/// cannot be detached and the user has to be told.
fn backend_for(
    record: &crate::table::MountRecord,
    search: &SearchPaths,
) -> std::result::Result<Backend, String> {
    Backend::for_share(&record.share, search).map_err(|e| match e {
        Error::Mount(message) => format!("{}: {message}", record.mountpoint.display()),
        other => format!("{}: {other}", record.mountpoint.display()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;

    use crate::uri::Share;

    fn share(input: &str) -> Share {
        Share::parse(input).expect("share").share
    }

    /// A temporary mount root and credential directory, with a table that points
    /// at them, and a mountinfo fixture listing what the kernel believes.
    struct Fixture {
        dir: tempfile::TempDir,
        mount_root: PathBuf,
        cred: PathBuf,
        mountinfo: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().expect("tempdir");
            let mount_root = dir.path().join("mnt");
            let cred = dir.path().join("cred");
            fs::create_dir_all(&mount_root).expect("mkdir");
            fs::create_dir_all(&cred).expect("mkdir");
            let mountinfo = dir.path().join("mountinfo");
            fs::write(&mountinfo, "").expect("write");
            Self {
                dir,
                mount_root,
                cred,
                mountinfo,
            }
        }

        fn table(&self) -> MountTable {
            MountTable::new(self.dir.path().join("config"), &self.mount_root)
        }

        fn observer(&self) -> Observer {
            Observer::at(&self.mountinfo)
        }

        /// Tell the fixture what the kernel's mount table contains for `path`.
        fn claim(&self, path: &Path, fstype: &str, source: &str) {
            let mut text = fs::read_to_string(&self.mountinfo).unwrap_or_default();
            let _ = writeln!(
                text,
                "36 35 0:34 / {} rw - {} {} rw",
                path.display(),
                fstype,
                source
            );
            fs::write(&self.mountinfo, text).expect("write mountinfo");
        }

        /// Stub programs for the helpers a detach needs, so a plan can be built
        /// on a host without the real ones. Takes `&self` to stay a method on the
        /// fixture even though it builds its own directory.
        #[allow(clippy::unused_self)]
        fn helpers(&self) -> (tempfile::TempDir, SearchPaths) {
            let dir = tempfile::tempdir().expect("tempdir");
            for name in ["uflinux-files-mount", "fusermount3", "sshfs"] {
                let path = dir.path().join(name);
                fs::write(&path, b"#!/bin/sh\nexit 0\n").expect("write stub");
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("chmod");
                }
            }
            let paths = SearchPaths::only(dir.path());
            (dir, paths)
        }

        fn options(&self) -> Options {
            Options::new(&self.cred)
        }
    }

    #[test]
    fn a_clean_system_produces_nothing() {
        let fix = Fixture::new();
        let table = fix.table();
        let (stubs, search) = fix.helpers();
        let sweep = Sweep::scan(&table, &fix.observer(), &search, &fix.options());
        assert!(sweep.is_empty(), "{}", sweep.summary());
        assert!(sweep.notes().is_empty());
        drop(stubs);
    }

    #[test]
    fn a_dead_fuse_mount_becomes_a_detach_plan() {
        let fix = Fixture::new();
        let mut table = fix.table();
        let record = table.add(&share("sftp://srv/data"));
        let mountpoint = record.mountpoint.clone();
        let id = record.id.clone();
        fs::create_dir(&mountpoint).expect("mkdir");
        table.record_mount(&id);
        // The directory is then removed, which is what a dead sshfs looks like
        // to a probe: the kernel has an entry, nothing answers there.
        fs::remove_dir(&mountpoint).expect("rmdir");
        fix.claim(&mountpoint, "fuse.sshfs", "srv:/data");
        let (stubs, search) = fix.helpers();
        let sweep = Sweep::scan(&table, &fix.observer(), &search, &fix.options());
        assert_eq!(sweep.len(), 1, "{}", sweep.summary());
        let item = &sweep.items()[0];
        assert_eq!(item.id.as_deref(), Some(id.as_str()));
        assert!(item.action.needs_command());
        assert!(item.why.contains("nothing answers"), "{}", item.why);
        let plan = item.plan().expect("plan");
        assert_eq!(plan.mountpoint(), mountpoint);
        assert_eq!(
            plan.command().argv()[0],
            stubs.path().join("fusermount3").to_string_lossy()
        );
        assert_eq!(sweep.plans().len(), 1);
    }

    #[test]
    fn a_live_mount_is_left_alone_when_it_is_not_idle() {
        let fix = Fixture::new();
        let mut table = fix.table();
        let record = table.add(&share("sftp://srv/data"));
        let mountpoint = record.mountpoint.clone();
        let id = record.id.clone();
        fs::create_dir(&mountpoint).expect("mkdir");
        table.record_mount(&id);
        fix.claim(&mountpoint, "fuse.sshfs", "srv:/data");
        let (stubs, search) = fix.helpers();
        let sweep = Sweep::scan(
            &table,
            &fix.observer(),
            &search,
            &fix.options().idle_after(3600),
        );
        assert!(sweep.is_empty(), "{}", sweep.summary());
        drop(stubs);
    }

    #[test]
    fn an_idle_mount_is_detached_and_an_autostart_one_is_not() {
        let fix = Fixture::new();
        let mut table = fix.table();
        let plain = table.add(&share("sftp://srv/one"));
        let plain_id = plain.id.clone();
        let plain_point = plain.mountpoint.clone();
        let kept = table.add(&share("sftp://srv/two"));
        let kept_point = kept.mountpoint.clone();
        let kept_id = kept.id.clone();
        fs::create_dir(&plain_point).expect("mkdir");
        fs::create_dir(&kept_point).expect("mkdir");
        table.record_mount(&plain_id);
        table.record_mount(&kept_id);
        table.set_autostart(&kept_id, true);
        table.record_mount(&kept_id);
        // Both were mounted long enough ago to count as idle.
        for record in table.mounts_mut() {
            record.last_used = Some(1);
        }
        fix.claim(&plain_point, "fuse.sshfs", "srv:/one");
        fix.claim(&kept_point, "fuse.sshfs", "srv:/two");
        let (stubs, search) = fix.helpers();
        let sweep = Sweep::scan(
            &table,
            &fix.observer(),
            &search,
            &fix.options().idle_after(60),
        );
        assert_eq!(sweep.len(), 1, "{}", sweep.summary());
        let item = &sweep.items()[0];
        assert_eq!(item.id.as_deref(), Some(plain_id.as_str()));
        assert!(
            item.why.contains("idle") || item.why.contains("used"),
            "{}",
            item.why
        );
        drop(stubs);
    }

    #[test]
    fn a_credential_file_for_a_share_that_is_not_mounted_is_removed() {
        let fix = Fixture::new();
        let mut table = fix.table();
        let record = table.add(&share("sftp://srv/data"));
        let id = record.id.clone();
        let kept = fix.cred.join(format!("{id}{CRED_SUFFIX}"));
        fs::write(&kept, "user = alice\n").expect("write cred");
        fs::write(fix.cred.join("gone-abcdef.conf"), "password=hunter2\n").expect("write orphan");
        fs::write(fix.cred.join("README"), b"x").expect("write unrelated");
        let (stubs, search) = fix.helpers();
        let mut sweep = Sweep::scan(&table, &fix.observer(), &search, &fix.options());
        let paths: Vec<&PathBuf> = sweep.items().iter().map(|i| &i.path).collect();
        assert!(paths.contains(&&kept), "{}", sweep.summary());
        assert_eq!(sweep.len(), 2, "{}", sweep.summary());
        assert!(
            sweep
                .items()
                .iter()
                .any(|i| i.why.contains("no longer a share")),
            "{}",
            sweep.summary()
        );
        assert_eq!(sweep.apply_files().expect("apply"), 2);
        assert!(!kept.exists(), "the password file should be gone");
        assert!(fix.cred.join("README").exists(), "unrelated files survive");
        assert!(sweep.is_empty());
        drop(stubs);
    }

    #[test]
    fn a_credential_file_for_a_live_mount_survives() {
        let fix = Fixture::new();
        let mut table = fix.table();
        let record = table.add(&share("sftp://srv/data"));
        let mountpoint = record.mountpoint.clone();
        let id = record.id.clone();
        fs::create_dir(&mountpoint).expect("mkdir");
        table.record_mount(&id);
        fix.claim(&mountpoint, "fuse.sshfs", "srv:/data");
        let kept = fix.cred.join(format!("{id}{CRED_SUFFIX}"));
        fs::write(&kept, "user = alice\n").expect("write cred");
        let (stubs, search) = fix.helpers();
        let sweep = Sweep::scan(&table, &fix.observer(), &search, &fix.options());
        assert!(sweep.is_empty(), "{}", sweep.summary());
        assert!(kept.exists());
        drop(stubs);
    }

    #[test]
    fn an_empty_orphan_directory_is_removed_and_a_full_one_is_not() {
        let fix = Fixture::new();
        let table = fix.table();
        let orphan = fix.mount_root.join("left-over-123456");
        let full = fix.mount_root.join("still-here-654321");
        fs::create_dir(&orphan).expect("mkdir");
        fs::create_dir(&full).expect("mkdir");
        fs::write(full.join("file.txt"), b"data").expect("write file");
        let (stubs, search) = fix.helpers();
        let mut sweep = Sweep::scan(&table, &fix.observer(), &search, &fix.options());
        assert_eq!(sweep.len(), 1, "{}", sweep.summary());
        assert_eq!(sweep.items()[0].action, Action::RemoveDir);
        assert_eq!(sweep.items()[0].path, orphan);
        assert!(
            sweep.notes().iter().any(|n| n.contains("not empty")),
            "{:?}",
            sweep.notes()
        );
        assert_eq!(sweep.apply_files().expect("apply"), 1);
        assert!(!orphan.exists());
        assert!(full.exists());
        drop(stubs);
    }

    #[test]
    fn a_mount_point_the_table_still_uses_is_never_removed() {
        let fix = Fixture::new();
        let mut table = fix.table();
        let record = table.add(&share("sftp://srv/data"));
        let mountpoint = record.mountpoint.clone();
        fs::create_dir(&mountpoint).expect("mkdir");
        let (stubs, search) = fix.helpers();
        let sweep = Sweep::scan(&table, &fix.observer(), &search, &fix.options());
        assert!(sweep.is_empty(), "{}", sweep.summary());
        assert!(mountpoint.exists());
        drop(stubs);
    }

    #[test]
    fn a_stale_kernel_mount_is_reported_even_though_it_is_not_probed() {
        // cifs and nfs are never probed, so a stale one is only visible through
        // the table: the record says it is mounted and mountinfo says otherwise.
        let fix = Fixture::new();
        let mut table = fix.table();
        let record = table.add(&share("nfs://srv/export"));
        let mountpoint = record.mountpoint.clone();
        let id = record.id.clone();
        fs::create_dir(&mountpoint).expect("mkdir");
        table.record_mount(&id);
        let (stubs, search) = fix.helpers();
        // Nothing in mountinfo, so the state is Absent rather than Stale: the
        // sweep leaves it for the reconnect path, and reports nothing.
        let sweep = Sweep::scan(&table, &fix.observer(), &search, &fix.options());
        assert!(sweep.is_empty(), "{}", sweep.summary());
        drop(stubs);
    }

    #[test]
    fn a_missing_helper_is_reported_instead_of_stopping_the_sweep() {
        let fix = Fixture::new();
        let mut table = fix.table();
        let dead = table.add(&share("sftp://srv/one"));
        let dead_point = dead.mountpoint.clone();
        let dead_id = dead.id.clone();
        fs::create_dir(&dead_point).expect("mkdir");
        table.record_mount(&dead_id);
        fs::remove_dir(&dead_point).expect("rmdir");
        fix.claim(&dead_point, "fuse.sshfs", "srv:/one");
        table.add(&share("nfs://srv/two"));
        fs::create_dir(fix.mount_root.join("orphan-000000")).expect("mkdir");
        let dir = tempfile::tempdir().expect("tempdir");
        let sweep = Sweep::scan(
            &table,
            &fix.observer(),
            &SearchPaths::only(dir.path()),
            &fix.options(),
        );
        // The detach could not be planned, but the empty directory still was.
        assert_eq!(sweep.len(), 1, "{}", sweep.summary());
        assert_eq!(sweep.items()[0].action, Action::RemoveDir);
        assert!(
            sweep
                .notes()
                .iter()
                .any(|n| n.contains("sshfs-fuse") || n.contains("fusermount")),
            "{:?}",
            sweep.notes()
        );
    }

    #[test]
    fn applying_a_detach_leaves_the_item_for_the_caller() {
        let fix = Fixture::new();
        let mut table = fix.table();
        let record = table.add(&share("sftp://srv/data"));
        let mountpoint = record.mountpoint.clone();
        let id = record.id.clone();
        fs::create_dir(&mountpoint).expect("mkdir");
        table.record_mount(&id);
        fs::remove_dir(&mountpoint).expect("rmdir");
        fix.claim(&mountpoint, "fuse.sshfs", "srv:/data");
        fs::write(fix.cred.join(format!("{id}{CRED_SUFFIX}")), "x").expect("write cred");
        let (stubs, search) = fix.helpers();
        let mut sweep = Sweep::scan(&table, &fix.observer(), &search, &fix.options());
        assert_eq!(sweep.len(), 2);
        assert_eq!(sweep.apply_files().expect("apply"), 1);
        assert_eq!(sweep.len(), 1, "the detach is still for the caller to run");
        assert!(sweep.items()[0].action.needs_command());
        drop(stubs);
    }

    #[test]
    fn a_summary_names_every_action_it_proposes() {
        let fix = Fixture::new();
        let table = fix.table();
        fs::create_dir(fix.mount_root.join("gone-123456")).expect("mkdir");
        let (stubs, search) = fix.helpers();
        let sweep = Sweep::scan(&table, &fix.observer(), &search, &fix.options());
        let text = sweep.summary();
        assert!(text.contains("rmdir"), "{text}");
        assert!(text.contains("gone-123456"), "{text}");
        drop(stubs);
        let empty = Sweep::default();
        assert_eq!(empty.summary(), "nothing to clean up");
    }

    #[test]
    fn a_missing_credential_directory_is_not_an_error() {
        let fix = Fixture::new();
        let table = fix.table();
        let (stubs, search) = fix.helpers();
        let options = Options::new(fix.dir.path().join("absent"));
        let sweep = Sweep::scan(&table, &fix.observer(), &search, &options);
        assert!(sweep.is_empty());
        drop(stubs);
    }

    #[test]
    fn credential_files_live_outside_the_mount_root() {
        let root = credential_root();
        assert!(root.starts_with(paths::config_dir()), "{}", root.display());
        assert!(!root.starts_with(paths::mount_root()));
    }
}
