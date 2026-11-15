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

/// One share's place in a connect-at-login run.
///
/// The outcome rather than a [`Report`], because the two halves of the answer are
/// asked separately: whether anything was attempted, and whether the share is
/// mounted now. A share that was already connected is what the user asked for and
/// is not a failure, and a share sitting on a dead mount point is not a failure
/// either — it is a share that needs the sweep before it needs a mount. Folding
/// those into `ok` alone would make a session log read as three broken servers
/// where one command did the right thing three times.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attempt {
    /// The record this attempt is about. Always present: a connect-at-login run
    /// only ever talks about shares the table already names.
    pub id: String,
    /// What to call the share, taken from the record rather than from the mount
    /// point, because the message is read by a person.
    pub label: String,
    /// Whether a mount command was run. `false` means the share was left as it
    /// was, which is a decision and not a failure to decide.
    pub attempted: bool,
    /// Whether the share is mounted as a result of this run.
    pub ok: bool,
    /// What happened, in the user's terms. Safe to print, for the reasons
    /// [`Report`] gives.
    pub message: String,
}

impl Attempt {
    /// The word for this outcome, matching what [`Login::summary`] prints. Four
    /// states, because there are four honest answers and two of them are not
    /// failures.
    #[must_use]
    pub fn verb(&self) -> &'static str {
        match (self.attempted, self.ok) {
            (true, true) => "mounted",
            (true, false) => "failed",
            (false, true) => "already",
            (false, false) => "skipped",
        }
    }
}

/// What a connect-at-login run did, in the order the table lists the shares.
///
/// The shape [`ufm_mount::sweep::Sweep`] has, for the same reason: this runs from
/// a session starter with no terminal attached, so the record of the run is
/// whatever it prints. A run that tried nothing is not an error — a user who never
/// ticked the box has nothing to connect — and a run whose server was down is not
/// one the session can act on either, since the reason is already written beside
/// the record and Reconnect is one click in a window.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Login {
    attempts: Vec<Attempt>,
}

impl Login {
    /// Every share the table marked connect-at-login, whether it was attempted or
    /// not. A run that reports nothing about a marked share would leave a user
    /// unable to tell "not tried" from "not even looked at".
    #[must_use]
    pub fn attempts(&self) -> &[Attempt] {
        &self.attempts
    }

    /// Whether the table had nothing marked, which is the common case and worth
    /// saying out loud rather than printing nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.attempts.is_empty()
    }

    /// How many shares are not mounted as a result. `already` counts as mounted;
    /// `skipped` and `failed` do not.
    #[must_use]
    pub fn not_mounted(&self) -> usize {
        self.attempts.iter().filter(|a| !a.ok).count()
    }

    /// Lines of `verb<TAB>id` with the reason indented under each, the shape
    /// [`ufm_mount::sweep::Sweep::summary`] uses so one session log reads as one
    /// format.
    #[must_use]
    pub fn summary(&self) -> String {
        use std::fmt::Write as _;
        if self.attempts.is_empty() {
            return "nothing marked connect at login\n".into();
        }
        let mut out = String::new();
        for attempt in &self.attempts {
            let _ = writeln!(out, "{}\t{}", attempt.verb(), attempt.id);
            let _ = writeln!(out, "\t{}", attempt.message);
        }
        out
    }

    fn record(&mut self, attempt: Attempt) {
        self.attempts.push(attempt);
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

    /// Re-read `mounts.json` from disk, and the kernel's table with it.
    ///
    /// The connect dialog mounts on a worker thread, which owns its own runner
    /// and writes the record it created to disk. This is how the window's runner
    /// finds out about it: without it the sidebar would keep showing the shares
    /// that were there when the window opened, and ejecting the new one would
    /// look like it had never connected.
    ///
    /// The file wins over memory, which is why a damaged file is ignored rather
    /// than propagated: a half-written read must not throw away the records this
    /// runner already had.
    pub fn reload(&mut self) {
        if let Ok(table) = MountTable::try_load_at(&self.layout.config_dir, &self.layout.mount_root)
        {
            self.table = table;
        }
        self.observer.reload();
    }

    /// Remember or forget a share for the next session, which is what the
    /// "Connect at login" row in a share's menu means.
    ///
    /// The flag is kept beside the record rather than in a file of its own: it is
    /// one boolean about one share, and the mount table is already the thing that
    /// knows which shares exist. Setting it on a share that has never been mounted
    /// is legitimate — that is the shape of "mount this when I log in" — and costs
    /// a mount point, which `--connect-at-login` creates.
    ///
    /// Returns whether the answer changed, so a caller can say "now this share
    /// connects at login" instead of repeating that back after a no-op.
    pub fn set_autostart(&mut self, id: &str, autostart: bool) -> bool {
        if !self.table.set_autostart(id, autostart) {
            return false;
        }
        // Unlike the password store, a lost write here is visible: the box would
        // come back unticked next session, so the caller is told nothing happened
        // rather than being handed `true`.
        self.table.save().is_ok()
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

    /// Whether [`Self::reconnect`] could finish without asking anyone anything,
    /// which is what decides whether a Reconnect row is worth offering.
    ///
    /// Reconnect deliberately has no password argument: it exists to be the row
    /// that costs a click, so a share whose password was never saved can only
    /// fail, and a row that is known in advance to fail should not be on screen.
    /// The two protocols that authenticate without one — NFS with the client's
    /// identity, SFTP with a key — are therefore always reconnectable, and the
    /// three that take one only when something was stored. An empty stored
    /// password counts as nothing stored, which matches [`Self::mount`]: it
    /// would refuse the same share for the same reason.
    ///
    /// This settles the credential half only. Whether the share is still listed,
    /// and whether its mount point is already alive, are the caller's questions,
    /// because the caller is the thing holding the kernel's answer.
    #[must_use]
    pub fn can_reconnect(&self, share: &Share) -> bool {
        !share.protocol.uses_password() || self.has_password(share)
    }

    /// Mount every share the user marked connect-at-login, and say what happened
    /// to each of them.
    ///
    /// The point of this run is that nobody is asked anything: it happens from a
    /// session starter, before a window exists, so a share that would need a
    /// password or a polkit prompt is left alone rather than started and watched
    /// fail. That
    /// is the same test [`Self::can_reconnect`] makes for a Reconnect row, for the
    /// same reason — an attempt known in advance to fail costs a mount point and a
    /// confusing error, and buys nothing.
    ///
    /// A share already mounted is reported and left as it is. Two windows, or a
    /// session starter and a late `--connect-at-login`, will both ask; the second
    /// one should be a no-op and not a "device or resource busy" the user has to
    /// interpret. A share sitting on a *dead* mount point is likewise left alone:
    /// mounting over a stale mount point does not work, and detaching it is what
    /// `--sweep-mounts` is for — the two runs are separate precisely so that this
    /// one never has to guess whether it is allowed to run a `umount`.
    ///
    /// Nothing here returns `Err`. A server that is down until lunchtime is an
    /// ordinary morning, and the reason is already stored beside the record for the
    /// Network group to show, so the run reports per share and exits clean.
    #[must_use]
    pub fn connect_at_login(&mut self) -> Login {
        let mut login = Login::default();
        // One read for the whole run: every share is judged against the same
        // snapshot of the kernel's state, so a share that appears partway through
        // does not make the earlier answers inconsistent with the later ones.
        self.observer.reload();
        let marked: Vec<(String, String, Share, MountState)> = self
            .table
            .autostart()
            .into_iter()
            .map(|record| {
                (
                    record.id.clone(),
                    record.label(),
                    record.share.clone(),
                    self.observer.state(&record.mountpoint),
                )
            })
            .collect();
        for (id, label, share, state) in marked {
            let (attempted, ok, message) = match state {
                MountState::Mounted => (false, true, format!("{label} was already connected")),
                MountState::Stale => (
                    false,
                    false,
                    format!("{label} is mounted but nothing answers there; clean it up first"),
                ),
                MountState::Absent if !self.can_reconnect(&share) => (
                    false,
                    false,
                    format!("{label} needs a password that was never remembered"),
                ),
                MountState::Absent => {
                    let report = self.mount(&share, None);
                    (true, report.ok, report.message)
                }
            };
            login.record(Attempt {
                id,
                label,
                attempted,
                ok,
                message,
            });
        }
        login
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

    /// Stop listing a share, detaching nothing that is in use.
    ///
    /// The refusal matters. A share the kernel says is mounted keeps its record,
    /// for the reason [`Self::unmount`] gives for keeping a record when a detach
    /// fails: a mount point with no card and no row is one nobody can let go of
    /// again from this window, which is worse than a row that will not go away.
    /// A stale mount point is a different answer — there is nothing mounted to
    /// strand, and the directory is the leftover the sweep exists for — so a
    /// dead share is dropped without argument.
    ///
    /// The state is asked of a fresh read rather than of whatever the last
    /// listing saw, because the question is being asked of a menu that may have
    /// been open while the share was connecting somewhere else.
    #[must_use]
    pub fn forget_share(&mut self, id: &str) -> Report {
        let Some(record) = self.table.get(id).cloned() else {
            return Report::done(id, "that share is no longer listed");
        };
        self.observer.reload();
        if self.observer.state(&record.mountpoint) == MountState::Mounted {
            return Report::failed(
                id,
                format!("{} is still connected; disconnect it first", record.label()),
            );
        }
        self.retire(id);
        Report::done(id, format!("{} is no longer listed", record.label()))
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
    fn forgetting_a_share_that_never_mounted_takes_the_row_and_the_link() {
        let fixture = Fixture::new();
        let share = share("smb://srv/share");
        let mut runner = fixture.runner();
        let id = runner.list(&share).id.clone();
        let report = runner.forget_share(&id);
        assert!(report.ok, "{}", report.message);
        assert!(runner.table().get(&id).is_none(), "the record is gone");
        let saved = fs::read_to_string(MountTable::path_at(&fixture.path("conf"))).expect("table");
        assert!(!saved.contains(&id), "{saved}");
    }

    #[test]
    fn forgetting_a_share_that_is_still_connected_refuses_and_keeps_the_row() {
        let fixture = Fixture::new();
        let share = share("sftp://host/data");
        let target = point(&fixture, &share);
        fixture.fuse_helper("sshfs", &target, "fuse.sshfs");
        let mut runner = fixture.runner();
        let id = runner.mount(&share, None).id.clone().expect("an id");
        // The published link is the share's own, so a refusal that left it
        // behind would leave a link to a record that is still there anyway.
        let report = runner.forget_share(&id);
        assert!(
            !report.ok,
            "a live share cannot be dropped out from under itself"
        );
        assert!(
            report.message.contains("still connected"),
            "{}",
            report.message
        );
        assert!(runner.table().get(&id).is_some(), "the row stays");
        assert!(
            fixture.path("net").join(share.label()).is_symlink(),
            "the share is still published, because it is still mounted"
        );
    }

    #[test]
    fn a_share_whose_mount_point_died_can_be_forgotten_without_detaching() {
        let fixture = Fixture::new();
        let share = share("sftp://host/data");
        let target = point(&fixture, &share);
        fixture.fuse_helper("sshfs", &target, "fuse.sshfs");
        let mut runner = fixture.runner();
        let id = runner.mount(&share, None).id.clone().expect("an id");
        // The daemon died: the mount table still names the point, and the point
        // no longer answers. Nothing is mounted to strand, so the row goes.
        fs::remove_dir(&target).expect("the mount point goes away");
        assert_eq!(runner.state(&id), MountState::Stale);
        let report = runner.forget_share(&id);
        assert!(report.ok, "{}", report.message);
        assert!(runner.table().get(&id).is_none(), "the dead row is gone");
        assert!(
            !fixture.path("net").join(share.label()).exists(),
            "the link to a share nobody can reach is taken away too"
        );
    }

    #[test]
    fn forgetting_a_share_that_is_not_listed_says_so_rather_than_failing() {
        let fixture = Fixture::new();
        let mut runner = fixture.runner();
        let report = runner.forget_share("smb-gone-000000");
        assert!(report.ok, "the state asked for is the state already in");
        assert!(runner.table().is_empty());
    }

    #[test]
    fn only_a_share_with_a_password_on_disk_is_worth_offering_to_reconnect() {
        let fixture = Fixture::new();
        let runner = fixture.runner();
        // The two protocols that authenticate without a password are always
        // reconnectable, and asking would be the wrong question for them.
        assert!(runner.can_reconnect(&share("nfs://srv/export")));
        assert!(runner.can_reconnect(&share("sftp://host/data")));
        // The three that take one are reconnectable only once it is stored;
        // without it, `mount` refuses the share before reaching the network and
        // the row would be a promise the menu knew it could not keep.
        let smb = share("smb://user@srv/share");
        let webdav = share("davs://host/files");
        let ftp = share("ftp://user@host/pub");
        for share in [&smb, &webdav, &ftp] {
            assert!(!runner.can_reconnect(share), "{}", share.display());
        }
        runner
            .remember(&smb, Some(Secret::from("hunter2")))
            .expect("store");
        assert!(runner.can_reconnect(&smb), "stored, so one click is enough");
        // An empty answer is still an answer: it is what a user who pressed
        // Connect without typing chose to save.
        runner
            .remember(&webdav, Some(Secret::from("")))
            .expect("store");
        assert!(
            !runner.can_reconnect(&webdav),
            "an empty stored password is the same answer as none, which is what \
             `mount` already decides"
        );
        // An anonymous share written `guest@` is a user with no password, which
        // the cifs backend refuses for exactly that reason — so the store's empty
        // answer is the right answer here, and the row is left out honestly rather
        // than by luck. A truly anonymous share is written without a user at all,
        // and that one is reconnected by the Connect dialog, which is where a
        // share with no record of its own is mounted anyway.
        assert!(
            !runner.can_reconnect(&share("smb://guest@srv/share")),
            "{}",
            "a password is needed to mount as guest, so a one-click row would fail"
        );
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

    #[test]
    fn marking_a_share_connect_at_login_is_saved_and_a_second_press_changes_nothing() {
        let fixture = Fixture::new();
        let share = share("nfs://srv/export");
        let mut runner = fixture.runner();
        let id = runner.list(&share).id.clone();
        assert!(runner.set_autostart(&id, true), "the answer changed");
        assert!(
            !runner.set_autostart(&id, true),
            "asking for the same answer again is not a change"
        );
        let saved = fs::read_to_string(MountTable::path_at(&fixture.path("conf"))).expect("table");
        assert!(saved.contains("\"autostart\": true"), "{saved}");
        assert!(
            !runner.set_autostart("no-such-share", true),
            "a share that is not listed cannot be marked"
        );
        let reloaded = fixture.runner();
        assert!(
            reloaded.table().get(&id).expect("record").autostart,
            "the answer survives to the next session, which is the only session it matters to"
        );
    }

    #[test]
    fn a_login_run_with_nothing_marked_says_so_rather_than_printing_nothing() {
        let fixture = Fixture::new();
        let mut runner = fixture.runner();
        let login = runner.connect_at_login();
        assert!(login.is_empty());
        assert_eq!(login.summary(), "nothing marked connect at login\n");
        assert_eq!(login.not_mounted(), 0);
    }

    #[test]
    fn a_marked_share_that_is_already_connected_is_reported_and_left_as_it_is() {
        let fixture = Fixture::new();
        let share = share("nfs://srv/export");
        let target = point(&fixture, &share);
        fs::create_dir_all(&target).expect("point");
        fixture.claim(&target, "nfs");
        let mut runner = fixture.runner();
        let id = runner.list(&share).id.clone();
        runner.set_autostart(&id, true);
        let login = runner.connect_at_login();
        let attempt = login.attempts().first().expect("one attempt");
        assert_eq!(attempt.verb(), "already");
        assert!(
            attempt.message.contains("was already connected"),
            "{}",
            attempt.message
        );
        assert_eq!(
            login.not_mounted(),
            0,
            "a share that was already there is a share that is mounted"
        );
        assert_eq!(runner.state(&id), MountState::Mounted);
    }

    #[test]
    fn a_marked_share_sitting_on_a_dead_mount_point_is_left_for_the_sweep() {
        let fixture = Fixture::new();
        let share = share("sftp://host/data");
        let target = point(&fixture, &share);
        fixture.fuse_helper("sshfs", &target, "fuse.sshfs");
        let mut runner = fixture.runner();
        let id = runner.mount(&share, None).id.clone().expect("an id");
        runner.set_autostart(&id, true);
        // The daemon died: the kernel still names the point, nothing answers there.
        fs::remove_dir(&target).expect("the mount point goes away");
        let login = runner.connect_at_login();
        let attempt = login.attempts().first().expect("one attempt");
        assert_eq!(attempt.verb(), "skipped");
        assert!(
            attempt.message.contains("clean it up first"),
            "{}",
            attempt.message
        );
        assert_eq!(login.not_mounted(), 1);
        // The record is untouched apart from the failure the run did not cause.
        assert!(
            runner.table().get(&id).is_some(),
            "a share the sweep can fix keeps its row"
        );
    }

    #[test]
    fn a_marked_share_that_would_need_a_password_is_never_started() {
        let fixture = Fixture::new();
        let share = share("smb://user@srv/share");
        let target = point(&fixture, &share);
        fixture.kernel_helper(&target, "cifs");
        let mut runner = fixture.runner();
        let id = runner.list(&share).id.clone();
        runner.set_autostart(&id, true);
        let login = runner.connect_at_login();
        let attempt = login.attempts().first().expect("one attempt");
        assert_eq!(
            attempt.verb(),
            "skipped",
            "nobody is there to type a password"
        );
        assert!(
            attempt.message.contains("needs a password"),
            "{}",
            attempt.message
        );
        assert!(
            !target.exists(),
            "a mount point is not spent on an attempt known to fail"
        );
        // With the password remembered, the same share is worth attempting.
        runner
            .remember(&share, Some(Secret::from("hunter2")))
            .expect("store");
        let login = runner.connect_at_login();
        assert_eq!(login.attempts().first().expect("one").verb(), "mounted");
    }

    #[test]
    fn a_marked_share_that_is_not_mounted_gets_mounted_and_a_refusal_is_a_failure() {
        let fixture = Fixture::new();
        let up = share("nfs://srv/export");
        let target = point(&fixture, &up);
        fixture.kernel_helper(&target, "nfs");
        let mut runner = fixture.runner();
        let id = runner.list(&up).id.clone();
        runner.set_autostart(&id, true);
        let login = runner.connect_at_login();
        let attempt = login.attempts().first().expect("one attempt");
        assert!(attempt.attempted, "the share was not already there");
        assert_eq!(attempt.verb(), "mounted");
        assert_eq!(runner.state(&id), MountState::Mounted);
        assert_eq!(login.not_mounted(), 0);

        // The same share on a box whose helper mounts nothing: the run says so.
        let broken = Fixture::new();
        let down = share("nfs://srv/export");
        let mut runner = broken.runner();
        let id = runner.list(&down).id.clone();
        runner.set_autostart(&id, true);
        let login = runner.connect_at_login();
        let attempt = login.attempts().first().expect("one attempt");
        assert_eq!(attempt.verb(), "failed");
        assert!(attempt.attempted, "the command was run and did not work");
        assert_eq!(login.not_mounted(), 1);
        assert!(
            attempt.message.contains(&down.label()),
            "the message names the share: {}",
            attempt.message
        );
    }

    #[test]
    fn a_login_run_answers_every_marked_share_in_the_order_the_table_lists_them() {
        let fixture = Fixture::new();
        let mounted = share("nfs://srv/export");
        let skipped = share("smb://user@srv/share");
        fixture.kernel_helper(&point(&fixture, &mounted), "nfs");
        let mut runner = fixture.runner();
        for share in [&mounted, &skipped] {
            let id = runner.list(share).id.clone();
            runner.set_autostart(&id, true);
        }
        let login = runner.connect_at_login();
        let ids: Vec<String> = login
            .attempts()
            .iter()
            .map(|attempt| attempt.id.clone())
            .collect();
        assert_eq!(
            ids,
            vec![mounted.slug(), skipped.slug()],
            "one row per marked share, in table order"
        );
        assert_eq!(
            login.not_mounted(),
            1,
            "the password-less share is not mounted"
        );
        let summary = login.summary();
        assert!(summary.contains("mounted\t"), "{summary}");
        assert!(summary.contains("skipped\t"), "{summary}");
        assert!(summary.contains(&skipped.label()), "{summary}");
    }
}
