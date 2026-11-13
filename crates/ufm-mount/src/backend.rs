//! Turning a [`Share`] into the command that mounts it.
//!
//! One section per protocol, and each one is a function from data to argv — no
//! process is started here. The order of preference is set by what UFLinux
//! actually ships and by what needs no privilege:
//!
//! | protocol | how | as |
//! |---|---|---|
//! | SMB | `mount(2)` with `cifs`, through the helper | root |
//! | NFS | `mount(2)` with `nfs`, through the helper | root |
//! | SFTP | `sshfs` | user |
//! | WebDAV | `rclone mount`, else `mount.davfs` | user |
//! | FTP | `rclone mount`, else `lftp mount` | user |
//!
//! A password never reaches argv. SMB gets a `credentials=` file the caller
//! writes at `0600` and deletes afterwards; rclone gets `RCLONE_CONFIG_UFM_PASS`,
//! whose *name* is in the plan but whose value is not; `lftp` gets a netrc file.
//! [`SecretUse`] names which channel a plan uses, and
//! [`crate::credentials::write_secret`] is there for a helper that wants the
//! password on stdin — none of the ones here do, since a mount has to survive
//! this process and re-read its password later.

use std::path::{Path, PathBuf};

use ufm_core::error::{Error, Result};

use crate::credentials::Secret;
use crate::plan::{Command, Plan, Privilege, SecretUse, helper_argv, join_options};
use crate::uri::{Protocol, Share};

/// The remote name used inside the generated `rclone.conf`, and therefore the
/// name that appears in the password environment variable. Fixed, so the
/// variable name can be a constant rather than something computed at runtime.
const RCLONE_REMOTE: &str = "ufm";

/// rclone reads this instead of the `pass` line in its config file, which keeps
/// the plaintext password out of the file on disk.
pub const RCLONE_PASS_ENV: &str = "RCLONE_CONFIG_UFM_PASS";

/// Where `rclone` looks for its config, pointing at the file this module writes.
pub const RCLONE_CONFIG_ENV: &str = "RCLONE_CONFIG";

/// Where `lftp` looks for its netrc file.
pub const LFTP_NETRC_ENV: &str = "LFTP_NETRC";

/// The directories searched for a helper. `$PATH` alone is not enough: the
/// system administrators' mount helpers live in `/sbin` and `/usr/sbin`, which a
/// desktop session does not always have in its path.
const SYSTEM_DIRS: &[&str] = &["/usr/local/bin", "/usr/bin", "/bin", "/usr/sbin", "/sbin"];

/// Where to look for a helper program. Injectable so a test can put a directory
/// of stub executables in front of the real ones and assert on the argv that
/// comes out, without installing anything.
#[derive(Debug, Clone)]
pub struct Paths {
    dirs: Vec<PathBuf>,
}

impl Paths {
    /// `$PATH` plus the system directories.
    #[must_use]
    pub fn system() -> Self {
        let mut dirs: Vec<PathBuf> = std::env::var_os("PATH")
            .map(|p| std::env::split_paths(&p).collect())
            .unwrap_or_default();
        dirs.extend(SYSTEM_DIRS.iter().map(PathBuf::from));
        dirs.retain(|d| !d.as_os_str().is_empty());
        Self { dirs }
    }

    /// Look only inside `dir`, which is what a test wants: a helper cannot be
    /// found there unless the test put it there.
    #[must_use]
    pub fn only(dir: impl Into<PathBuf>) -> Self {
        Self {
            dirs: vec![dir.into()],
        }
    }

    /// The first executable file named `name`, or `None`.
    #[must_use]
    pub fn find(&self, name: &str) -> Option<PathBuf> {
        if name.contains('/') {
            return executable(Path::new(name)).then(|| PathBuf::from(name));
        }
        self.dirs
            .iter()
            .map(|dir| dir.join(name))
            .find(|candidate| executable(candidate))
    }

    /// Where a helper of this kind was found, for an error message that names
    /// what is missing rather than what is present.
    #[must_use]
    pub fn searched(&self) -> Vec<PathBuf> {
        self.dirs.clone()
    }
}

#[cfg(unix)]
fn executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    match std::fs::metadata(path) {
        Ok(meta) => meta.is_file() && meta.permissions().mode() & 0o111 != 0,
        Err(_) => false,
    }
}

#[cfg(not(unix))]
fn executable(path: &Path) -> bool {
    path.is_file()
}

/// Everything a plan needs to know about one mount attempt.
#[derive(Debug, Clone, Copy)]
pub struct MountTarget<'a> {
    pub share: &'a Share,
    /// The directory to attach the filesystem to. Created by [`Plan::prepare`].
    pub mountpoint: &'a Path,
    /// Where credential files for this mount are written: a directory the user
    /// owns at `0700`, never the mount point itself.
    pub credential_dir: &'a Path,
}

impl<'a> MountTarget<'a> {
    #[must_use]
    pub fn new(share: &'a Share, mountpoint: &'a Path, credential_dir: &'a Path) -> Self {
        Self {
            share,
            mountpoint,
            credential_dir,
        }
    }

    /// The file the caller must write before running a plan whose
    /// [`SecretUse`] is [`SecretUse::File`], and delete afterwards. The name is
    /// the share's slug, so two mounts never share a credential file and a
    /// crash leaves an orphan that the sweep can remove.
    #[must_use]
    pub fn credential_path(&self) -> PathBuf {
        self.credential_dir
            .join(format!("{}.conf", self.share.slug()))
    }

    #[must_use]
    pub fn protocol(&self) -> Protocol {
        self.share.protocol
    }
}

/// The helper a protocol will use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Backend {
    /// Kernel CIFS, mounted by the privileged helper.
    Cifs { helper: PathBuf },
    /// Kernel NFS, mounted by the privileged helper.
    Nfs { helper: PathBuf },
    /// `sshfs`, FUSE, no privilege needed.
    Sshfs { program: PathBuf },
    /// `rclone mount`, which handles both WebDAV and FTP.
    Rclone { program: PathBuf },
    /// `mount.davfs`, the fallback for WebDAV.
    Davfs { program: PathBuf },
    /// `lftp mount`, the fallback for FTP.
    Lftp { program: PathBuf },
}

impl Backend {
    /// The backend `share` should use, preferring the one that needs no
    /// privilege where more than one exists.
    ///
    /// `paths` is the search path; pass [`Paths::system`] in production. The
    /// error names the package to install, because "no such file or directory"
    /// tells a user nothing.
    pub fn for_share(share: &Share, paths: &Paths) -> Result<Self> {
        let want = |program: &str, package: &str| -> Result<Self> {
            paths
                .find(program)
                .map(|p| match program {
                    "sshfs" => Self::Sshfs { program: p },
                    "rclone" => Self::Rclone { program: p },
                    "mount.davfs" => Self::Davfs { program: p },
                    _ => Self::Lftp { program: p },
                })
                .ok_or_else(|| {
                    Error::Mount(format!(
                        "{}: {} is not installed; run `apk add {package}`",
                        share.protocol.label(),
                        package
                    ))
                })
        };
        let helper = || -> Result<PathBuf> {
            paths.find("uflinux-files-mount").ok_or_else(|| {
                Error::Mount(format!(
                    "{}: the mount helper {} is missing; reinstall uflinux-files",
                    share.protocol.label(),
                    crate::plan::HELPER
                ))
            })
        };
        match share.protocol {
            Protocol::Smb => helper().map(|helper| Self::Cifs { helper }),
            Protocol::Nfs => helper().map(|helper| Self::Nfs { helper }),
            Protocol::Sftp => want("sshfs", Protocol::Sftp.packages()),
            // rclone first: one process for both WebDAV and FTP, a write cache,
            // and no setuid binary. davfs2 and lftp are the fallbacks because
            // they are what the base image already pulls in.
            Protocol::WebDav => want("rclone", "rclone").or_else(|_| want("mount.davfs", "davfs2")),
            Protocol::Ftp => want("rclone", "rclone").or_else(|_| want("lftp", "lftp")),
        }
    }

    /// Which package provides this backend, for the message shown when the
    /// backend was chosen earlier and its program has since been removed.
    #[must_use]
    pub fn packages(&self) -> &'static str {
        match self {
            Self::Cifs { .. } => "cifs-utils",
            Self::Nfs { .. } => "nfs-utils",
            Self::Sshfs { .. } => "sshfs-fuse",
            Self::Rclone { .. } => "rclone",
            Self::Davfs { .. } => "davfs2",
            Self::Lftp { .. } => "lftp",
        }
    }

    /// What the user sees in the connect dialog next to the address.
    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            Self::Cifs { .. } => "SMB/CIFS (kernel)",
            Self::Nfs { .. } => "NFS (kernel)",
            Self::Sshfs { .. } => "SFTP (sshfs)",
            Self::Rclone { .. } => "rclone",
            Self::Davfs { .. } => "WebDAV (davfs2)",
            Self::Lftp { .. } => "FTP (lftp)",
        }
    }

    #[must_use]
    pub const fn privilege(&self) -> Privilege {
        match self {
            Self::Cifs { .. } | Self::Nfs { .. } => Privilege::Root,
            Self::Sshfs { .. } | Self::Rclone { .. } | Self::Davfs { .. } | Self::Lftp { .. } => {
                Privilege::User
            }
        }
    }

    /// Whether the program this backend needs is present. A backend chosen at
    /// connect time can stop being present if the package is removed later, and
    /// the answer belongs in the UI before an attempt is made.
    #[must_use]
    pub fn available(&self, paths: &Paths) -> bool {
        paths.find(&self.program_name()).is_some()
    }

    #[must_use]
    pub fn program_name(&self) -> String {
        match self {
            Self::Cifs { helper } | Self::Nfs { helper } => helper.to_string_lossy().into_owned(),
            Self::Sshfs { program }
            | Self::Rclone { program }
            | Self::Davfs { program }
            | Self::Lftp { program } => program.to_string_lossy().into_owned(),
        }
    }

    /// The mount plan. `password` is only consulted by the protocols that can
    /// use one; see [`Protocol::uses_password`].
    ///
    /// When the returned plan's [`SecretUse`] is [`SecretUse::File`], the caller
    /// must write [`MountTarget::credential_path`] at `0600` with the bytes from
    /// [`credential_bytes`] before running it, and remove the file afterwards.
    /// davfs2 is the exception: it only ever reads its own secrets file, so the
    /// caller appends the returned line to `~/.davfs2/secrets` instead and
    /// leaves [`MountTarget::credential_path`] alone.
    pub fn mount(&self, target: &MountTarget, password: Option<&Secret>) -> Result<Plan> {
        match self {
            Self::Cifs { .. } => self.mount_cifs(target, password),
            Self::Nfs { .. } => self.mount_nfs(target),
            Self::Sshfs { program } => self.mount_sshfs(program, target),
            Self::Rclone { program } => self.mount_rclone(program, target, password),
            Self::Davfs { program } => self.mount_davfs(program, target, password),
            Self::Lftp { program } => self.mount_lftp(program, target, password),
        }
    }

    /// Every planner takes `&self` and returns a `Result` even when it cannot
    /// fail, so that adding a validation step to one of them — a length cap on
    /// an option value, say — does not mean rewriting the dispatcher and the
    /// callers. The symmetry is worth more than the two `allow`s.
    #[allow(clippy::unused_self, clippy::unnecessary_wraps)]
    fn mount_cifs(&self, target: &MountTarget, password: Option<&Secret>) -> Result<Plan> {
        let share = target.share;
        let guest = share.user.is_none() && password.is_none();
        if !guest && password.is_none() {
            return Err(Error::Mount(format!(
                "{}: a password is needed to mount as {}",
                share.label(),
                share.user.as_deref().unwrap_or("the share")
            )));
        }
        let mut options = vec![format!(
            "uid={}",
            share.option("uid").map_or_else(own_uid, String::from)
        )];
        if guest {
            options.push("guest".into());
        } else {
            if let Some(user) = &share.user {
                options.push(format!("username={user}"));
            }
            if let Some(domain) = share.option("domain") {
                options.push(format!("domain={domain}"));
            }
            // A password mount gets owner-only modes: the share is one user's
            // files, and the mount point is not the only thing keeping them
            // private.
            options.push("dir_mode=0700".into());
            options.push("file_mode=0600".into());
            options.push(format!(
                "credentials={}",
                display(&target.credential_path())
            ));
        }
        if let Some(vers) = share.option("vers") {
            options.push(format!("vers={vers}"));
        }
        if share.read_only() {
            options.push("ro".into());
        }
        let command = helper_argv(
            "mount",
            "cifs",
            &share.source(),
            target.mountpoint,
            &join_options(&options),
        )
        .secret(if password.is_some() {
            // The helper reads the password from the credentials file, which the
            // caller has already written; stdin stays unused.
            SecretUse::File
        } else {
            SecretUse::None
        });
        Ok(Plan::root(command, target.mountpoint))
    }

    #[allow(clippy::unused_self, clippy::unnecessary_wraps)]
    fn mount_nfs(&self, target: &MountTarget) -> Result<Plan> {
        let share = target.share;
        let mut options = Vec::new();
        if let Some(vers) = share.option("vers") {
            options.push(format!("vers={vers}"));
        }
        if let Some(proto) = share.option("proto") {
            options.push(format!("proto={proto}"));
        }
        // A network hiccup should freeze one window, not the whole session.
        // `intr` is the default on modern kernels and is not passed.
        options.push("noatime".into());
        if share.read_only() {
            options.push("ro".into());
        }
        let command = helper_argv(
            "mount",
            "nfs",
            &share.source(),
            target.mountpoint,
            &join_options(&options),
        )
        .secret(SecretUse::None);
        Ok(Plan::root(command, target.mountpoint))
    }

    #[allow(clippy::unused_self, clippy::unnecessary_wraps)]
    fn mount_sshfs(&self, program: &Path, target: &MountTarget) -> Result<Plan> {
        let share = target.share;
        let source = share.source();
        let mut command = Command::new(program)
            .arg(source.as_str())
            .arg(display(target.mountpoint))
            // Reconnect after a suspend or a dropped link rather than leaving a
            // transport-endpoint error until the user unmounts by hand.
            .arg("-o")
            .arg("reconnect,ServerAliveInterval=15,ServerAliveCountMax=3")
            .arg("-o")
            .arg("Cache=yes,CacheTimeout=30");
        if share.port_used() != Protocol::Sftp.default_port(false) {
            command = command.arg("-p").arg(share.port_used().to_string());
        }
        // `idmap=user` maps the server's uid onto the local one, which is what
        // makes a listing look sane. `?idmap=off` turns it off for a share whose
        // ownership the user wants to see as it really is.
        match share.option("idmap") {
            Some("off" | "0") => {}
            _ => command = command.arg("-o").arg("idmap=user"),
        }
        if share.read_only() {
            command = command.arg("-o").arg("ro");
        }
        Ok(Plan::user(
            command.secret(SecretUse::None),
            target.mountpoint,
        ))
    }

    #[allow(clippy::unused_self)]
    fn mount_rclone(
        &self,
        program: &Path,
        target: &MountTarget,
        password: Option<&Secret>,
    ) -> Result<Plan> {
        let share = target.share;
        // Anonymous WebDAV and anonymous FTP are both common, so a missing
        // password is only fatal when a user was given: with a user and no key
        // there is nothing left for rclone to authenticate with.
        if share.user.is_some() && password.is_none() {
            return Err(Error::Mount(format!(
                "{}: a password is needed for {}",
                share.label(),
                share.user.as_deref().unwrap_or("this share")
            )));
        }
        let config = target.credential_path();
        let kind = match share.protocol {
            Protocol::WebDav => "webdav",
            Protocol::Ftp => "ftp",
            other => {
                return Err(Error::Mount(format!(
                    "rclone cannot mount {}",
                    other.label()
                )));
            }
        };
        let command = Command::new(program)
            .arg("mount")
            .arg(rclone_remote(share, kind))
            .arg(display(target.mountpoint))
            // rclone needs its config to be a file it can read; the password is
            // in the environment, not in it.
            .env(RCLONE_CONFIG_ENV, display(&config))
            .env("RCLONE_CONFIG_UFM_TYPE", kind)
            .env("RCLONE_CONFIG_UFM_URL", share.backend_url())
            .env("RCLONE_CONFIG_UFM_VENDOR", "other")
            // Writes go through a local cache, so an application that opens a
            // file for writing does not fail outright.
            .arg("--vfs-cache-mode")
            .arg("writes")
            .arg("--dir-cache-time")
            .arg("30s")
            .arg("--daemon");
        let command = match share.user {
            Some(ref user) => command
                .env("RCLONE_CONFIG_UFM_USER", user.clone())
                .secret(SecretUse::Env(RCLONE_PASS_ENV)),
            None => command.secret(SecretUse::None),
        };
        let command = if share.read_only() {
            command.arg("--read-only")
        } else {
            command
        };
        Ok(Plan::user(command, target.mountpoint))
    }

    #[allow(clippy::unused_self, clippy::unnecessary_wraps)]
    fn mount_davfs(
        &self,
        program: &Path,
        target: &MountTarget,
        password: Option<&Secret>,
    ) -> Result<Plan> {
        let share = target.share;
        let mut options = vec![format!("uid={}", own_uid()), "noauto".into()];
        if password.is_some() {
            options.push("file_mode=0600".into());
            options.push("dir_mode=0700".into());
        }
        if share.read_only() {
            options.push("ro".into());
        } else {
            options.push("rw".into());
        }
        // davfs2 reads the password from its own secrets file, keyed by URL, so
        // the plan carries no secret and the caller appends to that file first.
        let command = Command::new(program)
            .arg(share.backend_url())
            .arg(display(target.mountpoint))
            .arg("-o")
            .arg(join_options(&options))
            .secret(if password.is_some() {
                SecretUse::File
            } else {
                SecretUse::None
            });
        Ok(Plan::user(command, target.mountpoint))
    }

    #[allow(clippy::unused_self)]
    fn mount_lftp(
        &self,
        program: &Path,
        target: &MountTarget,
        password: Option<&Secret>,
    ) -> Result<Plan> {
        let share = target.share;
        let mut command = Command::new(program)
            .arg("-e")
            .arg(format!(
                "mount '{}' ; wait",
                display(target.mountpoint).replace('\'', "'\\''")
            ))
            .arg(share.backend_url())
            // `lftp` reads a netrc file named by `$LFTP_NETRC`; the password is
            // never an argument, where `-u user:password` would put it.
            .env(LFTP_NETRC_ENV, display(&target.credential_path()));
        if let Some(user) = &share.user {
            command = command.arg("-u").arg(user.clone());
        }
        if password.is_none() && share.user.is_some() {
            return Err(Error::Mount(format!(
                "{}: a password is needed for {}",
                share.label(),
                share.user.as_deref().unwrap_or("this share")
            )));
        }
        // `lftp mount` has no read-only option and `lftp` has no `-o`, so `?ro`
        // cannot be expressed here. The record keeps the flag and the UI refuses
        // writes to an FTP mount marked read-only rather than pretending the
        // kernel is enforcing something it is not.
        Ok(Plan::user(
            command.secret(if password.is_some() {
                SecretUse::File
            } else {
                SecretUse::None
            }),
            target.mountpoint,
        ))
    }

    /// The plan that detaches this mount. FUSE backends unmount through
    /// `fusermount`, which needs no privilege for a mount the caller created;
    /// the kernel filesystems go back through the helper, which is the only
    /// process that can call `umount(2)`.
    pub fn unmount(&self, target: &MountTarget, paths: &Paths) -> Result<Plan> {
        match self {
            Self::Cifs { helper } => Ok(Plan::root(
                helper_argv(
                    "unmount",
                    "cifs",
                    &target.share.source(),
                    target.mountpoint,
                    "",
                )
                .secret(SecretUse::None)
                .clone_program(helper.clone()),
                target.mountpoint,
            )),
            Self::Nfs { helper } => Ok(Plan::root(
                helper_argv(
                    "unmount",
                    "nfs",
                    &target.share.source(),
                    target.mountpoint,
                    "",
                )
                .clone_program(helper.clone()),
                target.mountpoint,
            )),
            Self::Sshfs { .. } | Self::Rclone { .. } | Self::Davfs { .. } | Self::Lftp { .. } => {
                let program = paths
                    .find("fusermount3")
                    .or_else(|| paths.find("fusermount"))
                    .ok_or_else(|| {
                        Error::Mount("cannot unmount: fusermount3 is missing; install fuse3".into())
                    })?;
                Ok(Plan::user(
                    Command::new(program)
                        .arg("-u")
                        .arg(display(target.mountpoint))
                        .secret(SecretUse::None),
                    target.mountpoint,
                ))
            }
        }
    }
}

/// The remote argument rclone takes: `webdav:` for a share root, `webdav:a/b`
/// for a subdirectory. A root spelled `webdav:/` is read as a path that does not
/// exist, and a missing colon makes rclone look for a remote named `webdav`.
fn rclone_remote(share: &Share, kind: &str) -> String {
    match share.resource.strip_prefix('/') {
        Some("") | None => format!("{kind}:"),
        Some(path) => format!("{kind}:{path}"),
    }
}

fn own_uid() -> String {
    ufm_core::paths::uid().to_string()
}

fn display(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// The bytes to write to a mount's credential file, in the format that backend
/// expects. `None` means the backend needs no file at all.
pub fn credential_bytes(backend: &Backend, target: &MountTarget, password: &Secret) -> Vec<u8> {
    let user = target.share.user.as_deref().unwrap_or("");
    match backend {
        // `mount.cifs` credentials files are `key=value` lines.
        Backend::Cifs { .. } => {
            let mut out = format!("username={user}\npassword={}\n", password.expose());
            if let Some(domain) = target.share.option("domain") {
                out.push_str("domain=");
                out.push_str(domain);
                out.push('\n');
            }
            out.into_bytes()
        }
        // davfs2 secrets are `<url> <user> <password>`, one mount per line.
        Backend::Davfs { .. } => format!(
            "{} {} {}\n",
            target.share.backend_url(),
            user,
            password.expose()
        )
        .into_bytes(),
        // rclone keeps the password in the environment, so the config file it
        // reads has no secret in it and can be world-readable without worry.
        Backend::Rclone { .. } => format!(
            "[{RCLONE_REMOTE}]\ntype = {}\nurl = {}\nvendor = other\nuser = {user}\n",
            match target.share.protocol {
                Protocol::Ftp => "ftp",
                _ => "webdav",
            },
            target.share.backend_url()
        )
        .into_bytes(),
        // netrc, which `lftp` and `curl` both understand.
        Backend::Lftp { .. } => format!(
            "machine {} login {user} password {}\n",
            target.share.host,
            password.expose()
        )
        .into_bytes(),
        Backend::Nfs { .. } | Backend::Sshfs { .. } => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn share(input: &str) -> Share {
        Share::parse(input).unwrap().share
    }

    /// A directory holding stub executables, so `Paths::only` finds exactly the
    /// helpers a test claims are installed.
    fn installed(names: &[&str]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        for name in names {
            let path = dir.path().join(name);
            std::fs::write(&path, b"#!/bin/sh\nexit 0\n").expect("write stub");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
                    .expect("chmod");
            }
        }
        dir
    }

    /// Where a test says the per-mount credential files live. No test writes
    /// here — the point is only that a plan names a path under a directory the
    /// caller controls, and a fixed name keeps the helper below free of paths
    /// that would not outlive the target it returns.
    const CRED_DIR: &str = "/tmp/ufm-mount-backend-tests";

    /// The mount point a test plans against is the caller's temporary directory
    /// itself, so the returned target borrows paths that outlive it.
    fn target<'a>(share: &'a Share, mountpoint: &'a Path) -> MountTarget<'a> {
        MountTarget::new(share, mountpoint, Path::new(CRED_DIR))
    }

    #[test]
    fn a_stub_is_only_found_when_it_is_executable() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("sshfs");
        std::fs::write(&path, b"#!/bin/sh\n").expect("write");
        assert!(Paths::only(dir.path()).find("sshfs").is_none());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            assert!(Paths::only(dir.path()).find("sshfs").is_some());
        }
        assert!(Paths::only(dir.path()).find("absent").is_none());
    }

    #[test]
    fn smb_prefers_the_helper_and_never_puts_the_password_in_argv() {
        let dir = installed(&["uflinux-files-mount"]);
        let paths = Paths::only(dir.path());
        let s = share("smb://alice@srv/Users/alice");
        let backend = Backend::for_share(&s, &paths).expect("backend");
        assert_eq!(
            backend,
            Backend::Cifs {
                helper: dir.path().join("uflinux-files-mount")
            }
        );
        let root = tempfile::tempdir().expect("root");
        let t = target(&s, root.path());
        let plan = backend
            .mount(&t, Some(&Secret::from("hunter2")))
            .expect("plan");
        assert_eq!(plan.privilege(), Privilege::Root);
        let joined = plan.command().argv().join(" ");
        assert!(!joined.contains("hunter2"), "{joined}");
        assert!(joined.contains("cifs"), "{joined}");
        assert!(joined.contains("//srv/Users/alice"), "{joined}");
        assert!(joined.contains("username=alice"), "{joined}");
        assert!(joined.contains("dir_mode=0700"), "{joined}");
        assert!(
            joined.contains(&format!("credentials={}", t.credential_path().display())),
            "{joined}"
        );
        assert_eq!(plan.secret_use(), SecretUse::File);
    }

    #[test]
    fn smb_without_a_user_or_a_password_is_a_guest_mount() {
        let dir = installed(&["uflinux-files-mount"]);
        let s = share("smb://srv/Public");
        let backend = Backend::for_share(&s, &Paths::only(dir.path())).expect("backend");
        let root = tempfile::tempdir().expect("root");
        let plan = backend.mount(&target(&s, root.path()), None).expect("plan");
        let joined = plan.command().argv().join(" ");
        assert!(joined.contains("guest"), "{joined}");
        assert!(!joined.contains("username="), "{joined}");
        assert!(!joined.contains("credentials="), "{joined}");
        assert_eq!(plan.secret_use(), SecretUse::None);
    }

    #[test]
    fn smb_with_a_user_but_no_password_is_refused_before_spawning() {
        let dir = installed(&["uflinux-files-mount"]);
        let s = share("smb://alice@srv/Users");
        let backend = Backend::for_share(&s, &Paths::only(dir.path())).expect("backend");
        let root = tempfile::tempdir().expect("root");
        let err = backend.mount(&target(&s, root.path()), None).unwrap_err();
        let text = err.to_string();
        assert!(text.contains("password"), "{text}");
        assert!(text.contains("alice"), "{text}");
    }

    #[test]
    fn smb_options_are_passed_through_and_read_only_is_honoured() {
        let dir = installed(&["uflinux-files-mount"]);
        let s = share("smb://alice@srv/Users?vers=2.0&domain=WORK&ro&uid=2000");
        let backend = Backend::for_share(&s, &Paths::only(dir.path())).expect("backend");
        let root = tempfile::tempdir().expect("root");
        let plan = backend
            .mount(&target(&s, root.path()), Some(&Secret::from("pw")))
            .expect("plan");
        let joined = plan.command().argv().join(",");
        for expected in ["vers=2.0", "domain=WORK", "ro", "uid=2000"] {
            assert!(
                joined.contains(expected),
                "{expected} missing from {joined}"
            );
        }
    }

    #[test]
    fn nfs_needs_no_password_and_pins_no_version_it_was_not_given() {
        let dir = installed(&["uflinux-files-mount"]);
        let s = share("nfs://host/export/data");
        let backend = Backend::for_share(&s, &Paths::only(dir.path())).expect("backend");
        let root = tempfile::tempdir().expect("root");
        let plan = backend.mount(&target(&s, root.path()), None).expect("plan");
        let joined = plan.command().argv().join(" ");
        assert!(joined.contains("host:/export/data"), "{joined}");
        assert!(joined.contains("noatime"), "{joined}");
        assert!(!joined.contains("vers="), "{joined}");
        assert_eq!(plan.secret_use(), SecretUse::None);
        assert_eq!(plan.privilege(), Privilege::Root);
    }

    #[test]
    fn sftp_is_a_user_mount_with_keepalives() {
        let dir = installed(&["sshfs"]);
        let s = share("sftp://host/data");
        let backend = Backend::for_share(&s, &Paths::only(dir.path())).expect("backend");
        let root = tempfile::tempdir().expect("root");
        let t = target(&s, root.path());
        let plan = backend.mount(&t, None).expect("plan");
        assert_eq!(plan.privilege(), Privilege::User);
        let argv = plan.command().argv();
        assert_eq!(argv[0], dir.path().join("sshfs").to_string_lossy());
        assert_eq!(argv[1], "host:/data");
        assert_eq!(argv[2], t.mountpoint.to_string_lossy());
        let joined = argv.join(" ");
        assert!(joined.contains("reconnect"), "{joined}");
        assert!(joined.contains("ServerAliveInterval=15"), "{joined}");
        assert!(joined.contains("idmap=user"), "{joined}");
        assert!(!joined.contains("-p "), "{joined}");
    }

    #[test]
    fn sftp_passes_a_non_default_port_and_can_drop_idmap() {
        let dir = installed(&["sshfs"]);
        let s = share("sftp://alice@host:2222/data?idmap=off");
        let backend = Backend::for_share(&s, &Paths::only(dir.path())).expect("backend");
        let root = tempfile::tempdir().expect("root");
        let plan = backend.mount(&target(&s, root.path()), None).expect("plan");
        let joined = plan.command().argv().join(" ");
        assert!(joined.contains("-p 2222"), "{joined}");
        assert!(!joined.contains("idmap=user"), "{joined}");
        assert!(joined.contains("alice@host:/data"), "{joined}");
    }

    #[test]
    fn webdav_prefers_rclone_and_keeps_the_password_out_of_arguments() {
        let dir = installed(&["rclone", "mount.davfs"]);
        let s = share("davs://alice@host:8443/files");
        let backend = Backend::for_share(&s, &Paths::only(dir.path())).expect("backend");
        assert!(matches!(backend, Backend::Rclone { .. }));
        let root = tempfile::tempdir().expect("root");
        let t = target(&s, root.path());
        let plan = backend
            .mount(&t, Some(&Secret::from("hunter2")))
            .expect("plan");
        let joined = plan.command().argv().join(" ");
        assert!(!joined.contains("hunter2"), "{joined}");
        assert!(joined.contains("webdav:files"), "{joined}");
        assert!(joined.contains("--daemon"), "{joined}");
        assert!(joined.contains("--vfs-cache-mode writes"), "{joined}");
        assert_eq!(plan.secret_use(), SecretUse::Env(RCLONE_PASS_ENV));
        let env = plan.command().environment();
        assert!(
            env.iter()
                .any(|(k, v)| k == RCLONE_CONFIG_ENV && v == &display(&t.credential_path())),
            "{env:?}"
        );
        assert!(
            env.iter()
                .any(|(k, v)| k == "RCLONE_CONFIG_UFM_URL" && v == "https://host:8443/files"),
            "{env:?}"
        );
    }

    #[test]
    fn webdav_falls_back_to_davfs2_when_rclone_is_absent() {
        let dir = installed(&["mount.davfs"]);
        let s = share("webdav://host/dav");
        let backend = Backend::for_share(&s, &Paths::only(dir.path())).expect("backend");
        assert!(matches!(backend, Backend::Davfs { .. }));
        let root = tempfile::tempdir().expect("root");
        let plan = backend
            .mount(&target(&s, root.path()), Some(&Secret::from("hunter2")))
            .expect("plan");
        let joined = plan.command().argv().join(" ");
        assert!(!joined.contains("hunter2"), "{joined}");
        assert!(joined.contains("http://host:80/dav"), "{joined}");
        assert!(joined.contains("rw"), "{joined}");
        assert_eq!(plan.secret_use(), SecretUse::File);
    }

    #[test]
    fn ftp_falls_back_to_lftp_and_uses_a_netrc_file() {
        let dir = installed(&["lftp"]);
        let s = share("ftp://bob@files.example/pub");
        let backend = Backend::for_share(&s, &Paths::only(dir.path())).expect("backend");
        assert!(matches!(backend, Backend::Lftp { .. }));
        let root = tempfile::tempdir().expect("root");
        let t = target(&s, root.path());
        let plan = backend
            .mount(&t, Some(&Secret::from("hunter2")))
            .expect("plan");
        let joined = plan.command().argv().join(" ");
        assert!(!joined.contains("hunter2"), "{joined}");
        assert!(joined.contains("-u bob"), "{joined}");
        assert!(joined.contains("mount '"), "{joined}");
        assert!(joined.contains("ftp://files.example:21/pub"), "{joined}");
        assert!(
            plan.command()
                .environment()
                .iter()
                .any(|(k, _)| k == LFTP_NETRC_ENV),
            "{:?}",
            plan.command().environment()
        );
    }

    #[test]
    fn a_protocol_with_no_installed_helper_names_the_package() {
        let dir = installed(&[]);
        for (input, package) in [
            ("smb://srv/share", "uflinux-files"),
            ("nfs://srv/export", "uflinux-files"),
            ("sftp://srv/data", "sshfs-fuse"),
            ("webdav://srv/dav", "davfs2"),
            ("ftp://srv/pub", "lftp"),
        ] {
            let s = share(input);
            let err = Backend::for_share(&s, &Paths::only(dir.path())).unwrap_err();
            let text = err.to_string();
            assert!(text.contains(package), "{input}: {text}");
        }
    }

    #[test]
    fn a_fuse_unmount_goes_through_fusermount() {
        let dir = installed(&["sshfs", "fusermount3"]);
        let s = share("sftp://srv/data");
        let backend = Backend::for_share(&s, &Paths::only(dir.path())).expect("backend");
        let root = tempfile::tempdir().expect("root");
        let t = target(&s, root.path());
        let plan = backend.unmount(&t, &Paths::only(dir.path())).expect("plan");
        assert_eq!(plan.privilege(), Privilege::User);
        assert_eq!(
            plan.command().argv(),
            vec![
                dir.path()
                    .join("fusermount3")
                    .to_string_lossy()
                    .into_owned(),
                String::from("-u"),
                t.mountpoint.to_string_lossy().into_owned()
            ]
        );
    }

    #[test]
    fn a_kernel_unmount_goes_back_through_the_helper() {
        let dir = installed(&["uflinux-files-mount"]);
        let s = share("nfs://srv/export");
        let backend = Backend::for_share(&s, &Paths::only(dir.path())).expect("backend");
        let root = tempfile::tempdir().expect("root");
        let t = target(&s, root.path());
        let plan = backend.unmount(&t, &Paths::only(dir.path())).expect("plan");
        assert_eq!(plan.privilege(), Privilege::Root);
        let argv = plan.command().argv();
        assert_eq!(
            argv[0],
            dir.path().join("uflinux-files-mount").to_string_lossy()
        );
        assert_eq!(argv[1], "unmount");
        assert_eq!(argv[4], "--source");
        assert_eq!(argv[5], "srv:/export");
    }

    #[test]
    fn an_unmount_without_fusermount_says_so() {
        let dir = installed(&["sshfs"]);
        let s = share("sftp://srv/data");
        let backend = Backend::for_share(&s, &Paths::only(dir.path())).expect("backend");
        let root = tempfile::tempdir().expect("root");
        let err = backend
            .unmount(&target(&s, root.path()), &Paths::only(dir.path()))
            .unwrap_err();
        assert!(err.to_string().contains("fuse3"), "{err}");
    }

    #[test]
    fn credential_files_are_written_in_the_format_each_backend_wants() {
        let root = tempfile::tempdir().expect("root");
        let secret = Secret::from("hunter2");
        let cifs = share("smb://alice@srv/share?domain=WORK");
        let bytes = credential_bytes(
            &Backend::Cifs {
                helper: "/usr/libexec/uflinux-files-mount".into(),
            },
            &target(&cifs, root.path()),
            &secret,
        );
        let text = String::from_utf8(bytes).expect("utf8");
        assert!(text.contains("username=alice\n"), "{text}");
        assert!(text.contains("password=hunter2\n"), "{text}");
        assert!(text.contains("domain=WORK\n"), "{text}");

        let webdav = share("davs://alice@host/files");
        let bytes = credential_bytes(
            &Backend::Davfs {
                program: "/usr/sbin/mount.davfs".into(),
            },
            &target(&webdav, root.path()),
            &secret,
        );
        let text = String::from_utf8(bytes).expect("utf8");
        assert_eq!(text, "https://host:443/files alice hunter2\n");

        let ftp = share("ftp://bob@host/pub");
        let bytes = credential_bytes(
            &Backend::Lftp {
                program: "/usr/bin/lftp".into(),
            },
            &target(&ftp, root.path()),
            &secret,
        );
        let text = String::from_utf8(bytes).expect("utf8");
        assert_eq!(text, "machine host login bob password hunter2\n");

        let rclone = share("webdav://alice@host/dav");
        let bytes = credential_bytes(
            &Backend::Rclone {
                program: "/usr/bin/rclone".into(),
            },
            &target(&rclone, root.path()),
            &secret,
        );
        let text = String::from_utf8(bytes).expect("utf8");
        assert!(!text.contains("hunter2"), "{text}");
        assert!(text.contains("[ufm]"), "{text}");
        assert!(text.contains("user = alice"), "{text}");

        let nfs = share("nfs://srv/export");
        assert!(
            credential_bytes(
                &Backend::Nfs {
                    helper: "/usr/libexec/uflinux-files-mount".into()
                },
                &target(&nfs, root.path()),
                &secret
            )
            .is_empty()
        );
    }

    #[test]
    fn an_rclone_remote_at_the_root_of_a_share_names_the_remote_alone() {
        assert_eq!(
            rclone_remote(&share("webdav://host"), "webdav"),
            "webdav:",
            "a trailing slash would point one level too deep"
        );
        assert_eq!(
            rclone_remote(&share("webdav://host/a/b"), "webdav"),
            "webdav:a/b"
        );
    }

    #[test]
    fn every_backend_names_a_package_and_a_privilege_level() {
        let backends = vec![
            Backend::Cifs {
                helper: "/usr/libexec/uflinux-files-mount".into(),
            },
            Backend::Nfs {
                helper: "/usr/libexec/uflinux-files-mount".into(),
            },
            Backend::Sshfs {
                program: "/usr/bin/sshfs".into(),
            },
            Backend::Rclone {
                program: "/usr/bin/rclone".into(),
            },
            Backend::Davfs {
                program: "/usr/sbin/mount.davfs".into(),
            },
            Backend::Lftp {
                program: "/usr/bin/lftp".into(),
            },
        ];
        for backend in backends {
            assert!(!backend.packages().is_empty());
            assert!(!backend.label().is_empty());
            let root = match backend.privilege() {
                Privilege::Root => matches!(backend, Backend::Cifs { .. } | Backend::Nfs { .. }),
                Privilege::User => true,
            };
            assert!(root, "{backend:?} has the wrong privilege level");
        }
    }
}
