//! Running the plans that `ufm-mount` builds, and keeping the bookkeeping that
//! goes with them.
//!
//! [`ufm_mount`] decides *what* a mount means: the argv, the credential file,
//! the record, the cleanup. It never runs anything, because running a mount has
//! side effects that reach outside a test — a polkit prompt, a FUSE daemon, a
//! directory under `$XDG_RUNTIME_DIR`. This module is where those side effects
//! happen, and it is deliberately free of GTK: the UI calls these functions from
//! a worker thread and repaints from the result, and the same functions serve
//! `--sweep-mounts`, which runs before there is a window at all.
//!
//! Two rules shape the code below.
//!
//! **The kernel is the truth about state.** Whether a mount happened is answered
//! by re-reading `/proc/self/mountinfo`, not by a child's exit code, because
//! `mount(8)` can succeed and leave nothing usable and a FUSE helper can exit
//! cleanly after daemonising. The table records intent and the last error;
//! [`MountState`] decides what the Network group shows.
//!
//! **A password is on disk only while the mount is being established.** The
//! credential file is written immediately before the plan runs and removed on
//! every path out of [`Runner::mount`], including the error paths. A crash in
//! between leaves an orphan that `--sweep-mounts` removes, which is why the file
//! is named after the share rather than at random.

use std::io::ErrorKind;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use ufm_core::error::{Error, Result};
use ufm_mount::backend::{Backend, MountTarget, Paths as SearchPaths, credential_bytes};
use ufm_mount::credentials::{CredentialStore, FileStore, Secret};
use ufm_mount::monitor::{MountState, Observer};
use ufm_mount::plan::{ACTION, PKEXEC, Plan};
use ufm_mount::sweep::{Action, Options as SweepOptions, Sweep, SweepItem};
use ufm_mount::table::{MountRecord, MountTable};
use ufm_mount::uri::Share;

/// The idle limit for the auto-unmount that runs when the last window closes.
///
/// Long enough that stepping away for a phone call does not drop a share, short
/// enough that a machine left open overnight does not spend the night holding a
/// dozen FUSE daemons and their caches. A share marked autostart is never
/// detached by it — see [`ufm_mount::sweep`].
pub const IDLE_AFTER: u64 = 900;

/// One thing the runner did, ready to show.
///
/// The message is always safe to print: it comes from a plan's argv, a path, or a
/// helper's first line of stderr, and a password never reaches any of those.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    /// What happened, in the user's terms. Never empty.
    pub message: String,
    /// The share this is about, when the answer is about one.
    pub id: Option<String>,
    /// Whether the requested state is now true. `false` is not always a failure
    /// worth a dialog: the UI should repaint and show the message inline.
    pub ok: bool,
}

impl Report {
    fn done(id: &str, message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            id: Some(id.to_string()),
            ok: true,
        }
    }

    fn failed(id: &str, message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            id: Some(id.to_string()),
            ok: false,
        }
    }
}

/// Everything a [`Runner`] touches that a test might want somewhere else.
///
/// A test builds a `Runner` over a temporary directory and a directory of stub
/// programs, so nothing outside those is written.
#[derive(Debug, Clone)]
pub struct Layout {
    /// Where mount points live: `$XDG_RUNTIME_DIR/ufm` in a session.
    pub mount_root: PathBuf,
    /// Where `<slug>.conf` credential files are written and removed.
    pub credential_dir: PathBuf,
    /// Where the symlinks that other applications see live.
    pub network_dir: PathBuf,
    /// Where `mounts.json` and `credentials.json` live.
    pub config_dir: PathBuf,
    /// Where helpers are looked for.
    pub search: SearchPaths,
    /// `/proc/self/mountinfo`, injectable so a test can claim a mount exists.
    pub mountinfo: PathBuf,
    /// `pkexec`, used for the two kernel filesystems. Injectable because a test
    /// cannot write to `/usr/bin`.
    pub pkexec: PathBuf,
}

impl Layout {
    /// The real layout for this session.
    #[must_use]
    pub fn system() -> Self {
        Self {
            mount_root: ufm_core::paths::mount_root(),
            credential_dir: ufm_mount::sweep::credential_root(),
            network_dir: ufm_core::paths::network_dir(),
            config_dir: ufm_core::paths::config_dir(),
            search: SearchPaths::system(),
            mountinfo: PathBuf::from("/proc/self/mountinfo"),
            pkexec: PathBuf::from(PKEXEC),
        }
    }
}

impl Default for Layout {
    fn default() -> Self {
        Self::system()
    }
}

/// Mounts, unmounts, and the cleanup in between.
///
/// One per process, used from a worker thread. Owning the table is the point: the
/// file on disk and the answer the UI shows are then updated under one lock, so a
/// second window cannot catch a state mid-update.
#[derive(Debug)]
pub struct Runner {
    layout: Layout,
    table: MountTable,
    observer: Observer,
    store: FileStore,
}

impl Runner {
    /// Load the table from `layout`. A missing or damaged file yields an empty
    /// table: a file manager that refuses to start because `mounts.json` has a
    /// syntax error is worse than one that forgets the shares, because
    /// reconnecting is one dialog whereas an error at launch is all the user gets.
    #[must_use]
    pub fn open(layout: Layout) -> Self {
        let table = MountTable::try_load_at(&layout.config_dir, &layout.mount_root)
            .unwrap_or_else(|_| MountTable::new(&layout.config_dir, &layout.mount_root));
        Self {
            store: FileStore::at(layout.config_dir.join("credentials.json")),
            observer: Observer::at(&layout.mountinfo),
            layout,
            table,
        }
    }

    #[must_use]
    pub fn layout(&self) -> &Layout {
        &self.layout
    }

    #[must_use]
    pub fn table(&self) -> &MountTable {
        &self.table
    }

    /// The state of one record, against the mount table as last read.
    #[must_use]
    pub fn state(&self, id: &str) -> MountState {
        self.table.get(id).map_or(MountState::Absent, |record| {
            self.observer.state(&record.mountpoint)
        })
    }

    /// Re-read the kernel's mount table, so a badge reflects what happened while a
    /// directory listing was being read.
    pub fn refresh(&mut self) {
        self.observer.reload();
    }

    /// Note that a share carried traffic, which is what stops an idle auto-unmount
    /// from dropping a share someone is using. Called when a listing is read
    /// through a mount point, not when a window is merely open.
    pub fn touch(&mut self, id: &str) {
        if self.table.touch(id) {
            // Best effort: losing the timestamp costs one unmount at the wrong
            // moment, and the next listing sets it again.
            let _ = self.table.save();
        }
    }

    /// Connect to `share`, with `password` when the caller has one.
    ///
    /// A password is looked for in this order: the argument, then the stored one.
    /// A share that needs one and has neither fails with the backend's own
    /// message, which is the one that names the protocol.
    ///
    /// A failed attempt is a [`Report`] with `ok` false rather than an `Err`,
    /// because the record and its reason are both worth keeping: the share stays
    /// in the Network group with the error next to it, and the next attempt starts
    /// from a known state. The record is added before anything runs for the same
    /// reason — a crash mid-mount then leaves a mount point the sweep can account
    /// for instead of an unexplained directory. The signature says the same thing:
    /// there is no error a caller could act on that the record does not carry.
    #[must_use]
    pub fn mount(&mut self, share: &Share, password: Option<&Secret>) -> Report {
        let label = share.label();
        // A password the caller just typed is the one to use and, once the mount
        // works, the one to cache. Otherwise the stored one is read here, into a
        // local that is dropped when this function returns, so it is on disk no
        // longer than the plan that needs it. `Secret` is deliberately not `Clone`,
        // which is why the choice is made by reference and made once.
        let typed = password.filter(|given| !given.is_empty());
        let stored = if typed.is_some() {
            None
        } else {
            self.store.get(share)
        };
        let secret = typed.or(stored.as_ref());
        let (id, mountpoint) = {
            let record = self.table.add(share);
            (record.id.clone(), record.mountpoint.clone())
        };
        match self.mount_with(share, &mountpoint, secret) {
            Ok(()) => {
                if let Some(given) = typed {
                    // A copy, because the caller keeps its own and `Secret` is not
                    // `Clone`. Failing to cache costs a second prompt, not the
                    // mount, so the error is not the user's problem here.
                    let _ = self.remember(share, Some(Secret::from(given.expose())));
                }
                self.table.record_mount(&id);
                let _ = self.table.save();
                Report::done(&id, self.publish(&label, &mountpoint))
            }
            Err(message) => {
                self.table.record_failure(&id, message.clone());
                let _ = self.table.save();
                Report::failed(&id, message)
            }
        }
    }

    /// Detach a share and forget it, which is what Eject means.
    ///
    /// Already-gone counts as success: the state the user asked for is the state
    /// the system is in, and a retry after a lost reply should not look like a
    /// failure. A mount that survives the attempt keeps its record — dropping it
    /// would strand a mount point nobody can eject any more, which is the one
    /// outcome worse than a card that says "still mounted".
    #[must_use]
    pub fn unmount(&mut self, id: &str) -> Report {
        let Some(record) = self.table.get(id).cloned() else {
            self.retire(id);
            return Report::done(id, "that share is not connected");
        };
        let label = record.label();
        let outcome = self.detach(&record).map_err(|e| e.to_string());
        self.observer.reload();
        if self.observer.state(&record.mountpoint) == MountState::Absent {
            // The empty mount point goes too: a later mount recreates it, and
            // leaving it behind is the leftover the sweep exists to clean up.
            let _ = std::fs::remove_dir(&record.mountpoint);
            self.retire(id);
            return Report::done(id, format!("{label} is disconnected"));
        }
        let message = match outcome {
            Ok(()) => format!("{label} is still mounted; something is using it"),
            Err(message) => message,
        };
        self.table.record_failure(id, message.clone());
        let _ = self.table.save();
        Report::failed(id, message)
    }

    /// Reconnect a share the table already knows, using its stored password.
    #[must_use]
    pub fn reconnect(&mut self, id: &str) -> Report {
        let Some(share) = self.table.get(id).map(|record| record.share.clone()) else {
            return Report::failed(id, "that share is no longer listed");
        };
        self.mount(&share, None)
    }

    /// Store a password so a later mount or reconnect can answer without asking.
    pub fn remember(&self, share: &Share, password: Option<Secret>) -> Result<()> {
        self.store.put(share, share.user.as_deref(), password)
    }

    /// Drop a stored password, which is what "do not remember this one" means.
    pub fn forget(&self, share: &Share) -> Result<()> {
        self.store.forget(share)
    }

    /// Whether a password is stored, so a dialog can show dots instead of asking
    /// for a password that is already on disk.
    #[must_use]
    pub fn has_password(&self, share: &Share) -> bool {
        self.store.get(share).is_some()
    }

    /// Clean up after mounts that are no longer mounts: detach what the scan says
    /// should go, remove what needs no command, and account for both.
    ///
    /// The [`Sweep`] comes back with its notes intact for the caller to print.
    /// Notes are never fatal — a sweep that stops halfway leaves the rest of the
    /// leftovers to rot, and a leftover password file is the last thing that
    /// should rot — so a note does not become an `Err`.
    pub fn sweep(&mut self, options: &SweepOptions) -> Result<Sweep> {
        self.observer.reload();
        let mut sweep = Sweep::scan(&self.table, &self.observer, &self.layout.search, options);
        sweep.apply_files()?;
        let outcomes: Vec<std::result::Result<(), String>> = detachables(&sweep)
            .iter()
            .map(|item| match item.plan() {
                Some(plan) => {
                    let serves = item
                        .id
                        .as_deref()
                        .and_then(|id| self.table.get(id))
                        .is_some_and(|record| backend_serves(&record.share, &self.layout.search));
                    run_plan(plan, serves, &self.layout.pkexec).map_err(|e| e.to_string())
                }
                None => Err("a detach with no command to run".to_string()),
            })
            .collect();
        for (item, outcome) in detachables(&sweep).into_iter().zip(outcomes) {
            let Some(id) = item.id.as_deref() else {
                continue;
            };
            match outcome {
                // The detach worked, so the share is no longer connected and the
                // mount point is empty. Both eject paths end the same way.
                Ok(()) => {
                    let _ = std::fs::remove_dir(&item.path);
                    sweep.record_done(&item);
                    self.retire(id);
                }
                Err(message) => {
                    self.table.record_failure(id, message);
                }
            }
        }
        self.observer.reload();
        // A detach can empty a mount point the scan saw as non-empty, and a share
        // that stopped being a mount has no credential file worth keeping.
        sweep.apply_files()?;
        let _ = self.table.save();
        Ok(sweep)
    }

    /// Detach shares idle for [`IDLE_AFTER`] or longer. Called from a timer
    /// started when the last window closes.
    pub fn auto_unmount(&mut self) -> Result<Sweep> {
        let options = self.idle_options(IDLE_AFTER);
        self.sweep(&options)
    }

    /// The options for an idle sweep, against this layout's credential directory.
    #[must_use]
    pub fn idle_options(&self, idle_after: u64) -> SweepOptions {
        SweepOptions::new(&self.layout.credential_dir).idle_after(idle_after)
    }

    /// The options for a sweep that only removes leftovers.
    #[must_use]
    pub fn leftover_options(&self) -> SweepOptions {
        SweepOptions::new(&self.layout.credential_dir)
    }

    /// Add or update the record for a share without mounting, so a share can be
    /// listed before it is connected — which is what "connect at login" means
    /// before the first successful mount.
    #[must_use]
    pub fn list(&mut self, share: &Share) -> &mut MountRecord {
        self.table.add(share)
    }

    /// Stop listing a share, detaching nothing. Used when a card is removed and
    /// the share turns out not to be connected.
    pub fn forget_share(&mut self, id: &str) -> bool {
        let had = self.table.get(id).is_some();
        if had {
            self.retire(id);
        }
        had
    }

    /// Build the plan for `share`, run it, then ask the kernel whether it worked.
    fn mount_with(
        &mut self,
        share: &Share,
        mountpoint: &Path,
        secret: Option<&Secret>,
    ) -> std::result::Result<(), String> {
        let backend = Backend::for_share(share, &self.layout.search).map_err(|e| text(&e))?;
        let cred = self.layout.credential_dir.clone();
        let target = MountTarget::new(share, mountpoint, &cred);
        let plan = backend.mount(&target, secret).map_err(|e| text(&e))?;
        self.execute(&backend, &target, &plan, secret)?;
        self.observer.reload();
        match self.observer.state(mountpoint) {
            MountState::Mounted => Ok(()),
            MountState::Stale => Err(format!(
                "{label} is mounted but nothing answers there",
                label = share.label()
            )),
            MountState::Absent => Err(format!("{} did not mount", share.label())),
        }
    }

    /// Build and run the plan that detaches `record`.
    fn detach(&self, record: &MountRecord) -> std::result::Result<(), Error> {
        let backend = Backend::for_share(&record.share, &self.layout.search)?;
        let target = MountTarget::new(
            &record.share,
            &record.mountpoint,
            &self.layout.credential_dir,
        );
        let plan = backend.unmount(&target, &self.layout.search)?;
        self.execute(&backend, &target, &plan, None)
            .map_err(Error::Mount)
    }

    /// Prepare the mount point, write the credential file the plan asks for, run
    /// the plan, and remove the file again — including when the plan fails, which
    /// is the part a caller cannot redo afterwards.
    ///
    /// The child's exit status alone decides the answer here. Whether that status
    /// means the share is usable is the caller's question, because a mount and an
    /// unmount are verified differently.
    fn execute(
        &self,
        backend: &Backend,
        target: &MountTarget<'_>,
        plan: &Plan,
        secret: Option<&Secret>,
    ) -> std::result::Result<(), String> {
        plan.prepare().map_err(|e| text(&e))?;
        let path = target.credential_path();
        let mut written = false;
        if let Some(password) = secret.filter(|_| plan.secret_use().wanted()) {
            let bytes = credential_bytes(backend, target, password);
            let outcome = if bytes.is_empty() {
                Ok(())
            } else if matches!(backend, Backend::Davfs { .. }) {
                append_davfs_secret(&bytes).map_err(|e| text(&e))
            } else {
                write_credential(&path, &bytes).map_err(|e| text(&e))
            };
            if let Err(message) = outcome {
                let _ = std::fs::remove_file(&path);
                return Err(message);
            }
            written = true;
        }
        let outcome =
            run_plan(plan, backend.serves(), &self.layout.pkexec).map_err(|e| e.to_string());
        if written {
            let _ = std::fs::remove_file(&path);
        }
        outcome
    }

    /// Drop a record and its symlink, which is where both eject paths end.
    fn retire(&mut self, id: &str) {
        if let Some(record) = self.table.remove(id) {
            self.unpublish(&record);
        }
        let _ = self.table.save();
    }

    /// Create the symlink other applications see and describe the result. A
    /// failure to link is folded into the message rather than failing the mount:
    /// the share *is* connected, and the link only exists for programs that
    /// cannot reach `$XDG_RUNTIME_DIR`.
    fn publish(&self, label: &str, mountpoint: &Path) -> String {
        let dir = &self.layout.network_dir;
        let plain = format!("{label} is connected");
        if let Err(e) = std::fs::create_dir_all(dir) {
            return format!("{plain} ({e})");
        }
        let link = dir.join(label);
        if link == mountpoint {
            return plain;
        }
        let outcome = std::os::unix::fs::symlink(mountpoint, &link).or_else(|e| {
            if e.kind() != ErrorKind::AlreadyExists {
                return Err(e);
            }
            // Either this share reconnected, or two shares chose the same label.
            // Replacing is right in the first case, and the second is rare enough
            // that last mount winning is acceptable — the mount point itself is
            // always unambiguous.
            std::fs::remove_file(&link).and_then(|()| std::os::unix::fs::symlink(mountpoint, &link))
        });
        match outcome {
            Ok(()) => format!("{label} — {}", link.display()),
            Err(e) => format!("{plain} ({e})"),
        }
    }

    /// Remove a share's symlink, leaving alone a link that points somewhere else:
    /// a folder the user made by hand with the same name is theirs to keep.
    fn unpublish(&self, record: &MountRecord) {
        let link = self.layout.network_dir.join(record.label());
        let Ok(target) = std::fs::read_link(&link) else {
            return;
        };
        if target == record.mountpoint {
            let _ = std::fs::remove_file(&link);
        }
    }
}

/// Whether detaching this share leaves a process of ours to look after.
///
/// The sweep hands over plans rather than backends, so the answer is rebuilt from
/// the record's share: it depends only on which helper the protocol would use, and
/// rebuilding it costs one lookup of a program the plan names anyway.
fn backend_serves(share: &Share, search: &SearchPaths) -> bool {
    Backend::for_share(share, search).is_ok_and(|backend| backend.serves())
}

/// Run one plan: wait for the ones that finish, leave the ones that serve alone.
///
/// [`Plan::run`] is what is executed rather than the plan's own command, so that a
/// `cifs` or `nfs` plan goes through `pkexec` with the polkit action named — the
/// helper is not setuid, and running it directly as the user would fail with
/// `EPERM` rather than a message that says what to do about it.
fn run_plan(plan: &Plan, serves: bool, pkexec: &Path) -> std::result::Result<(), Error> {
    let command = plan.run(pkexec, ACTION);
    if serves {
        // `lftp mount` *is* the mount: waiting for it would block for the life of
        // the share, and killing it on exit would take the share with it.
        return command.serve().map(|_| ());
    }
    command.status()
}

/// The items in a sweep that carry a detach, in order, so their outcomes can be
/// matched to them without re-walking the list. Owned rather than borrowed: the
/// caller reports each success back into the sweep it came from, and a scan cannot
/// be read and written at the same time. Cloning a plan is copying an argv.
fn detachables(sweep: &Sweep) -> Vec<SweepItem> {
    sweep
        .items()
        .iter()
        .filter(|item| matches!(item.action, Action::Detach(_)))
        .cloned()
        .collect()
}

/// A credential file, created `0600` rather than tightened afterwards, so there
/// is no moment in which another user can open it.
fn write_credential(path: &Path, bytes: &[u8]) -> std::result::Result<(), Error> {
    let dir = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir)
        .map_err(|e| Error::Mount(format!("cannot create {}: {e}", dir.display())))?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
        .map_err(|e| Error::Mount(format!("cannot protect {}: {e}", dir.display())))?;
    ufm_core::atomic::write_atomic(path, bytes, 0o600)
}

/// davfs2 reads only `~/.davfs2/secrets`, so its line is appended there rather
/// than written as a file of ours. The mode matters: davfs2 refuses to use a
/// secrets file another user can read.
fn append_davfs_secret(bytes: &[u8]) -> std::result::Result<(), Error> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    let path = ufm_core::paths::home().join(".davfs2").join("secrets");
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|e| Error::Mount(format!("cannot create {}: {e}", dir.display())))?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(&path)
        .map_err(|e| Error::Mount(format!("cannot open {}: {e}", path.display())))?;
    file.write_all(bytes)
        .map_err(|e| Error::Mount(format!("cannot write {}: {e}", path.display())))
}

/// The message from an error, which is already a sentence and never holds a
/// password.
fn text(error: &Error) -> String {
    error.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    /// A session rooted in a temporary directory: stub helpers, an empty mount
    /// table, and nothing outside the directory that a test can write to.
    struct Fixture {
        /// Never read, which is the point: dropping it would delete the tree out
        /// from under the test that is still using it.
        _root: tempfile::TempDir,
        /// Canonicalised, because [`Observer::find`] resolves symlinks before it
        /// compares a mount point with what the mount table says.
        path: PathBuf,
        layout: Layout,
    }

    /// Every helper a test might need, so a missing stub reads as "that backend
    /// is not installed" rather than as a confusing spawn error.
    const HELPERS: &[&str] = &[
        "uflinux-files-mount",
        "sshfs",
        "rclone",
        "mount.davfs",
        "lftp",
        "fusermount3",
    ];

    impl Fixture {
        fn new() -> Self {
            let root = tempfile::tempdir().expect("tempdir");
            let path = fs::canonicalize(root.path()).expect("canonical");
            for dir in ["mnt", "cred", "net", "conf", "bin"] {
                fs::create_dir_all(path.join(dir)).expect("dir");
            }
            let mountinfo = path.join("mountinfo");
            fs::write(&mountinfo, "").expect("mountinfo");
            let fixture = Self {
                path: path.clone(),
                layout: Layout {
                    mount_root: path.join("mnt"),
                    credential_dir: path.join("cred"),
                    network_dir: path.join("net"),
                    config_dir: path.join("conf"),
                    search: SearchPaths::only(path.join("bin")),
                    mountinfo,
                    pkexec: path.join("bin").join("pkexec"),
                },
                _root: root,
            };
            for name in HELPERS {
                fixture.stub(name, "#!/bin/sh\nexit 0\n");
            }
            // The real `pkexec` authorises and then runs the action's program; a
            // stub that drops the action id and execs is the same shape.
            fixture.stub("pkexec", "#!/bin/sh\nshift\nexec \"$@\"\n");
            fixture
        }

        fn runner(&self) -> Runner {
            Runner::open(self.layout.clone())
        }

        fn path(&self, name: &str) -> PathBuf {
            self.path.join(name)
        }

        fn stub(&self, name: &str, script: &str) {
            let path = self.path("bin").join(name);
            fs::write(&path, script).expect("stub");
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("chmod");
        }

        /// A helper stub that works: `mount` creates the mount point and reports
        /// it in the mount table, anything else empties the mount table again.
        fn kernel_helper(&self, point: &Path, fstype: &str) {
            let script = format!(
                "#!/bin/sh\nif [ \"$1\" = mount ]; then\n  mkdir -p '{p}'\n  \
                 printf '%s\\n' '{l}' >> '{i}'\nelse\n  : > '{i}'\nfi\nexit 0\n",
                p = point.display(),
                l = line(point, fstype),
                i = self.layout.mountinfo.display(),
            );
            self.stub("uflinux-files-mount", &script);
        }

        /// The same for a FUSE backend, where mounting is the helper's own
        /// business and detaching is `fusermount3`'s.
        fn fuse_helper(&self, program: &str, point: &Path, fstype: &str) {
            let script = format!(
                "#!/bin/sh\nmkdir -p '{p}'\nprintf '%s\\n' '{l}' >> '{i}'\nexit 0\n",
                p = point.display(),
                l = line(point, fstype),
                i = self.layout.mountinfo.display(),
            );
            self.stub(program, &script);
            self.stub("fusermount3", &detach(&self.layout.mountinfo));
        }

        /// Append a mount table entry by hand, for a mount that exists before the
        /// test starts.
        fn claim(&self, point: &Path, fstype: &str) {
            let existing = fs::read_to_string(&self.layout.mountinfo).unwrap_or_default();
            fs::write(
                &self.layout.mountinfo,
                format!("{existing}{}\n", line(point, fstype)),
            )
            .expect("claim");
        }
    }

    fn line(point: &Path, fstype: &str) -> String {
        format!(
            "36 35 0:34 / {} rw - {fstype} host:/data rw",
            point.display()
        )
    }

    fn detach(mountinfo: &Path) -> String {
        format!("#!/bin/sh\n: > '{}'\nexit 0\n", mountinfo.display())
    }

    fn stamp() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs())
    }

    fn share(input: &str) -> Share {
        Share::parse(input).expect("parses").share
    }

    fn point(fixture: &Fixture, share: &Share) -> PathBuf {
        fixture.path("mnt").join(share.slug())
    }

    #[test]
    fn a_mount_the_kernel_agrees_with_is_reported_connected() {
        let fixture = Fixture::new();
        let share = share("sftp://host/data");
        let target = point(&fixture, &share);
        fixture.fuse_helper("sshfs", &target, "fuse.sshfs");
        let mut runner = fixture.runner();
        let report = runner.mount(&share, None);
        assert!(report.ok, "{}", report.message);
        let id = report.id.clone().expect("an id");
        assert_eq!(runner.state(&id), MountState::Mounted);
        let record = runner.table().get(&id).expect("record");
        assert!(record.mounted_at.is_some(), "{}", report.message);
        assert!(record.last_error.is_none());
    }

    #[test]
    fn a_mount_the_kernel_does_not_confirm_is_a_failure_with_a_reason() {
        let fixture = Fixture::new();
        let share = share("sftp://host/data");
        let mut runner = fixture.runner();
        let report = runner.mount(&share, None);
        assert!(!report.ok, "the stub mounts nothing");
        assert!(
            report.message.contains(&share.label()),
            "{}",
            report.message
        );
        // The share stays listed, with the reason, so the card is not inert.
        let record = runner.table().find(&share).expect("recorded");
        assert!(record.last_error.is_some());
        assert!(record.mountpoint.starts_with(fixture.path("mnt")));
    }

    #[test]
    fn a_failing_helper_reports_its_own_first_line() {
        let fixture = Fixture::new();
        let share = share("smb://user@srv/share");
        fixture.stub(
            "uflinux-files-mount",
            "#!/bin/sh\necho 'mount error(13): Permission denied' >&2\nexit 32\n",
        );
        let mut runner = fixture.runner();
        let report = runner.mount(&share, Some(&Secret::from("hunter2")));
        assert!(!report.ok);
        assert!(
            report.message.contains("Permission denied"),
            "{}",
            report.message
        );
    }

    #[test]
    fn a_root_plan_is_run_through_pkexec_naming_the_action() {
        let fixture = Fixture::new();
        let marker = fixture.path("action");
        let share = share("nfs://srv/export");
        fixture.stub(
            "pkexec",
            &format!(
                "#!/bin/sh\nprintf '%s' \"$1\" > '{}'\nshift\nexec \"$@\"\n",
                marker.display()
            ),
        );
        let mut runner = fixture.runner();
        let _ = runner.mount(&share, None);
        assert_eq!(
            fs::read_to_string(&marker).expect("marker"),
            ufm_mount::plan::ACTION,
            "the polkit policy, not the caller, decides what may run as root"
        );
    }

    #[test]
    fn a_password_never_reaches_the_saved_record() {
        let fixture = Fixture::new();
        let share = share("smb://user@srv/share");
        let mut runner = fixture.runner();
        let _ = runner.mount(&share, Some(&Secret::from("hunter2")));
        let saved =
            fs::read_to_string(MountTable::path_at(&fixture.path("conf"))).expect("table saved");
        assert!(!saved.contains("hunter2"), "{saved}");
        assert!(saved.contains("share"), "the record is still there");
    }

    #[test]
    fn the_credential_file_exists_only_while_the_plan_runs() {
        let fixture = Fixture::new();
        let share = share("smb://user@srv/share");
        let marker = fixture.path("saw-credentials");
        // The helper's argv carries `credentials=PATH` inside `--options`; the
        // stub reports whether that file was there while it ran.
        fixture.stub(
            "uflinux-files-mount",
            &format!(
                "#!/bin/sh\nfor a in \"$@\"; do case \"$a\" in \
                 *credentials=*) p=${{a##*credentials=}}; \
                 if [ -f \"$p\" ]; then echo yes > '{m}'; else echo no > '{m}'; fi;; \
                 esac; done\nexit 0\n",
                m = marker.display(),
            ),
        );
        let mut runner = fixture.runner();
        let _ = runner.mount(&share, Some(&Secret::from("hunter2")));
        assert_eq!(
            fs::read_to_string(&marker).expect("marker").trim(),
            "yes",
            "the password file has to be there when the helper runs"
        );
        assert!(
            fs::read_dir(fixture.path("cred"))
                .expect("cred dir")
                .count()
                == 0,
            "and gone again once it has run"
        );
    }

    #[test]
    fn a_connected_share_gets_a_symlink_and_ejecting_it_removes_the_symlink() {
        let fixture = Fixture::new();
        let share = share("nfs://srv/export");
        let target = point(&fixture, &share);
        fixture.kernel_helper(&target, "nfs");
        let mut runner = fixture.runner();
        let report = runner.mount(&share, None);
        assert!(report.ok, "{}", report.message);
        let link = fixture.path("net").join(share.label());
        assert_eq!(
            fs::read_link(&link).expect("symlink"),
            target,
            "other applications are pointed at the mount point"
        );
        assert!(report.message.contains(&link.display().to_string()));
        let report = runner.unmount(&report.id.expect("an id"));
        assert!(report.ok, "{}", report.message);
        assert!(!link.exists(), "the symlink goes with the mount");
        assert!(runner.table().is_empty());
        assert!(!target.exists(), "and so does the empty mount point");
    }

    #[test]
    fn unmounting_something_that_is_not_mounted_is_not_a_failure() {
        let fixture = Fixture::new();
        let share = share("nfs://srv/export");
        let mut runner = fixture.runner();
        let id = runner.list(&share).id.clone();
        let report = runner.unmount(&id);
        assert!(report.ok, "{}", report.message);
        assert!(runner.table().is_empty(), "and the record is gone");
    }

    #[test]
    fn a_mount_that_survives_an_unmount_keeps_its_record() {
        let fixture = Fixture::new();
        let share = share("nfs://srv/export");
        let target = point(&fixture, &share);
        fs::create_dir_all(&target).expect("point");
        fixture.claim(&target, "nfs");
        let mut runner = fixture.runner();
        let id = runner.list(&share).id.clone();
        let report = runner.unmount(&id);
        assert!(!report.ok, "{}", report.message);
        assert!(
            runner.table().get(&id).is_some(),
            "a mount nobody detached has to stay ejectable"
        );
        assert!(target.exists(), "and its mount point stays too");
    }

    #[test]
    fn a_sweep_removes_a_leftover_password_file() {
        let fixture = Fixture::new();
        fs::write(fixture.path("cred").join("ghost.conf"), b"password=x\n").expect("leftover");
        let mut runner = fixture.runner();
        let options = runner.leftover_options();
        let sweep = runner.sweep(&options).expect("sweep");
        // Nothing is left to do, and the run reports one thing it did: the two
        // answers are different questions, which is why they are two lists.
        assert!(sweep.is_empty(), "{}", sweep.summary());
        assert_eq!(sweep.removed().len(), 1, "{}", sweep.summary());
        assert!(
            sweep.summary().contains("ghost.conf"),
            "the summary has to name what it removed: {}",
            sweep.summary()
        );
        assert!(!fixture.path("cred").join("ghost.conf").exists());
    }

    #[test]
    fn a_sweep_detaches_a_stale_mount_and_forgets_the_share() {
        let fixture = Fixture::new();
        let share = share("sftp://host/data");
        let target = point(&fixture, &share);
        // Claimed in the mount table, with no directory behind it: exactly what a
        // dead sshfs leaves, and why a stale entry has to be detached rather than
        // merely forgotten.
        fixture.claim(&target, "fuse.sshfs");
        fixture.stub("fusermount3", &detach(&fixture.layout.mountinfo));
        let mut runner = fixture.runner();
        let _ = runner.list(&share);
        let options = runner.leftover_options();
        let sweep = runner.sweep(&options).expect("sweep");
        assert_eq!(sweep.plans().len(), 1, "{}", sweep.summary());
        assert!(runner.table().is_empty(), "the detach succeeded");
        assert_eq!(runner.state(&share.slug()), MountState::Absent);
    }

    #[test]
    fn an_idle_share_is_detached_and_a_busy_one_is_left_alone() {
        let fixture = Fixture::new();
        fixture.stub("fusermount3", &detach(&fixture.layout.mountinfo));
        let idle = share("sftp://old/data");
        let busy = share("sftp://fresh/data");
        let mut runner = fixture.runner();
        for share in [&idle, &busy] {
            let target = point(&fixture, share);
            fs::create_dir_all(&target).expect("point");
            fixture.claim(&target, "fuse.sshfs");
        }
        runner.list(&idle).last_used = Some(0);
        runner.list(&busy).last_used = Some(stamp());
        runner.refresh();
        let options = runner.idle_options(60);
        let sweep = runner.sweep(&options).expect("sweep");
        assert_eq!(sweep.plans().len(), 1, "{}", sweep.summary());
        assert!(
            runner.table().get(&busy.slug()).is_some(),
            "a share that was just used stays mounted"
        );
        assert!(runner.table().get(&idle.slug()).is_none());
    }

    #[test]
    fn a_share_marked_autostart_survives_an_idle_sweep() {
        let fixture = Fixture::new();
        let share = share("sftp://host/data");
        let target = point(&fixture, &share);
        fs::create_dir_all(&target).expect("point");
        fixture.claim(&target, "fuse.sshfs");
        let mut runner = fixture.runner();
        runner.list(&share).autostart = true;
        runner.list(&share).last_used = Some(0);
        runner.refresh();
        let options = runner.idle_options(60);
        let sweep = runner.sweep(&options).expect("sweep");
        assert!(sweep.is_empty(), "{}", sweep.summary());
        assert!(runner.table().get(&share.slug()).is_some());
    }

    #[test]
    fn a_reconnect_uses_the_stored_password_and_leaves_no_file_behind() {
        let fixture = Fixture::new();
        let share = share("smb://user@srv/share");
        let target = point(&fixture, &share);
        fixture.kernel_helper(&target, "cifs");
        let mut runner = fixture.runner();
        runner
            .remember(&share, Some(Secret::from("hunter2")))
            .expect("store");
        assert!(runner.has_password(&share));
        // Reconnect is for a share the table already knows — the Network group
        // lists it, and the user clicked it.
        let id = runner.list(&share).id.clone();
        let report = runner.reconnect(&id);
        assert!(report.ok, "{}", report.message);
        assert!(
            fs::read_dir(fixture.path("cred"))
                .expect("cred dir")
                .count()
                == 0,
            "the credential file is not left behind"
        );
        let saved = fs::read_to_string(MountTable::path_at(&fixture.path("conf"))).expect("table");
        assert!(!saved.contains("hunter2"), "{saved}");
    }

    #[test]
    fn forgetting_a_password_makes_a_mount_ask_again() {
        let fixture = Fixture::new();
        let share = share("smb://user@srv/share");
        let mut runner = fixture.runner();
        runner
            .remember(&share, Some(Secret::from("hunter2")))
            .expect("store");
        runner.forget(&share).expect("forget");
        assert!(!runner.has_password(&share));
        let report = runner.mount(&share, None);
        assert!(
            report.message.contains("password is needed"),
            "{}",
            report.message
        );
    }

    #[test]
    fn a_damaged_table_is_tolerated_rather_than_fatal() {
        let fixture = Fixture::new();
        fs::write(MountTable::path_at(&fixture.path("conf")), b"{ not json").expect("junk");
        assert!(fixture.runner().table().is_empty());
    }

    #[test]
    fn a_mount_point_is_created_before_the_command_runs() {
        let fixture = Fixture::new();
        let share = share("sftp://host/data");
        let mut runner = fixture.runner();
        let _ = runner.mount(&share, None);
        let target = point(&fixture, &share);
        assert!(
            target.is_dir(),
            "the helper refuses a target that does not exist"
        );
        assert_eq!(
            fs::metadata(&target).expect("mode").permissions().mode() & 0o777,
            0o700,
            "and refuses one another user can read"
        );
    }

    #[test]
    fn a_saved_table_reloads_with_mount_points_under_this_mount_root() {
        let fixture = Fixture::new();
        let share = share("nfs://srv/export");
        {
            let mut runner = fixture.runner();
            let _ = runner.mount(&share, None);
        }
        let reloaded = fixture.runner();
        let record = reloaded.table().find(&share).expect("survives");
        assert_eq!(record.mountpoint, point(&fixture, &share));
    }

    #[test]
    fn a_symlink_the_user_made_by_hand_is_left_alone() {
        let fixture = Fixture::new();
        let share = share("nfs://srv/export");
        let target = point(&fixture, &share);
        fixture.kernel_helper(&target, "nfs");
        // A folder in the Network directory that is not one of ours.
        fs::create_dir_all(fixture.path("net").join(share.label())).expect("folder");
        let mut runner = fixture.runner();
        let report = runner.mount(&share, None);
        let _ = runner.unmount(&report.id.expect("an id"));
        assert!(
            fixture.path("net").join(share.label()).is_dir(),
            "the user's folder is theirs to keep"
        );
    }
}
