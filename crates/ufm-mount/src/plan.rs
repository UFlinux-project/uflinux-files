//! The command a mount resolves to, and how it is allowed to run.
//!
//! A [`Plan`] is data, not an action. Building it is fallible and total — it
//! either names a helper that exists or explains which package to install — and
//! running it is the caller's decision, which is what makes the whole layer
//! testable without root: the tests assert on argv.
//!
//! The rule that shapes everything here is that a password is never an
//! argument. `/proc/<pid>/cmdline` is world-readable, so `mount -o pass=hunter2`
//! leaks a share password to every user on the machine, and to anything that
//! snapshots process listings. [`SecretUse`] says where the secret goes instead,
//! and [`Command::mentions`] is how the tests prove it stayed out of argv.

use std::ffi::OsStr;
use std::fs::Permissions;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use ufm_core::error::{Error, Result};

/// Where the child process expects to find a password.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecretUse {
    /// The protocol takes no password: NFS, and SFTP with a key or an agent.
    None,
    /// One line on stdin, then EOF. Kept for a helper that reads a password from
    /// its standard input — `mount.cifs` can, when it is not attached to a
    /// terminal — but no backend here relies on it today: SMB goes through a
    /// `credentials=` file instead, because a mount that outlives this process has
    /// to be able to re-read its password after a reconnect.
    Stdin,
    /// A named environment variable. Environment is not world-readable the way
    /// argv is, and `rclone` reads a password this way.
    Env(&'static str),
    /// A file the caller writes at `0600` before running the plan and removes
    /// afterwards: `mount.cifs`'s `credentials=`, davfs2's secrets file, lftp's
    /// netrc. A file rather than stdin because these helpers re-read it after a
    /// reconnect, when there is no stdin left to read from.
    File,
}

impl SecretUse {
    /// Whether a password has to be delivered at all for this plan.
    #[must_use]
    pub const fn wanted(self) -> bool {
        !matches!(self, Self::None)
    }
}

/// Who the command has to run as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Privilege {
    /// The user's own uid. Everything FUSE falls here: a FUSE mount only needs
    /// to create a file under a directory the user owns.
    User,
    /// Root, through the privileged helper. `mount(2)` for `cifs` and `nfs` is
    /// the only reason this exists.
    Root,
}

/// A program, its arguments, and the environment it needs.
///
/// Arguments are stored as `String` rather than `OsString` because every one of
/// them came from a parsed URI, a numeric id, or a constant, and keeping them
/// `String` is what lets [`Command::mentions`] check for a password without
/// lossy conversions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Command {
    program: PathBuf,
    args: Vec<String>,
    env: Vec<(String, String)>,
    secret: SecretUse,
}

impl Command {
    #[must_use]
    pub fn new(program: impl AsRef<Path>) -> Self {
        debug_assert!(
            sane_program(program.as_ref().as_os_str()),
            "a program name may not carry arguments: {}",
            program.as_ref().display()
        );
        Self {
            program: program.as_ref().to_path_buf(),
            args: Vec::new(),
            env: Vec::new(),
            secret: SecretUse::None,
        }
    }

    #[must_use]
    pub fn arg(mut self, arg: impl Into<String>) -> Self {
        self.args.push(arg.into());
        self
    }

    #[must_use]
    pub fn with_args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    #[must_use]
    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }

    #[must_use]
    pub fn secret(mut self, use_: SecretUse) -> Self {
        self.secret = use_;
        self
    }

    #[must_use]
    pub fn program(&self) -> &Path {
        &self.program
    }

    /// Replace the program, keeping the arguments. Used when a plan was built
    /// against the default helper path and the search found it somewhere else.
    #[must_use]
    pub fn clone_program(mut self, program: PathBuf) -> Self {
        self.program = program;
        self
    }

    /// The program's name without its directory, for error messages.
    #[must_use]
    pub fn name(&self) -> String {
        self.program.file_name().map_or_else(
            || self.program.display().to_string(),
            |n| n.to_string_lossy().into_owned(),
        )
    }

    #[must_use]
    pub fn arguments(&self) -> &[String] {
        &self.args
    }

    #[must_use]
    pub fn environment(&self) -> &[(String, String)] {
        &self.env
    }

    #[must_use]
    pub const fn secret_use(&self) -> SecretUse {
        self.secret
    }

    /// Program followed by arguments, which is what a test compares against.
    #[must_use]
    pub fn argv(&self) -> Vec<String> {
        let mut out = Vec::with_capacity(self.args.len() + 1);
        out.push(self.program.to_string_lossy().into_owned());
        out.extend(self.args.iter().cloned());
        out
    }

    /// The command as one line, for a status bar or a log. Values that would
    /// need quoting are quoted, and the result is safe to show because a secret
    /// is never in it.
    #[must_use]
    pub fn display(&self) -> String {
        let mut out = self.program.to_string_lossy().into_owned();
        for arg in &self.args {
            out.push(' ');
            if arg.is_empty() || arg.contains([' ', '\t', '\n', '"', '\\']) {
                out.push('"');
                for c in arg.chars() {
                    if matches!(c, '"' | '\\') {
                        out.push('\\');
                    }
                    out.push(c);
                }
                out.push('"');
            } else {
                out.push_str(arg);
            }
        }
        out
    }

    /// Whether `secret` appears anywhere in the arguments or the environment.
    /// Used by tests, and cheap enough to call before spawning.
    #[must_use]
    pub fn mentions(&self, secret: &str) -> bool {
        !secret.is_empty()
            && (self.args.iter().any(|a| a.contains(secret))
                || self
                    .env
                    .iter()
                    .any(|(k, v)| !is_secret_var(k) && v.contains(secret)))
    }

    /// Spawn this command with its stdin open, so a caller can write a password
    /// before the helper gives up waiting.
    pub fn spawn(&self) -> Result<std::process::Child> {
        let mut builder = std::process::Command::new(&self.program);
        builder
            .args(&self.args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped());
        for (key, value) in &self.env {
            builder.env(key, value);
        }
        builder.spawn().map_err(|e| {
            Error::Mount(format!(
                "{}: cannot start {} ({e})",
                self.name(),
                self.program.display()
            ))
        })
    }

    /// Spawn without waiting, for a plan whose process *is* the mount.
    ///
    /// `lftp mount` serves its filesystem for as long as the `lftp` process
    /// lives, so running that plan to completion would block forever and killing
    /// it on exit would take the share with it. Stderr is inherited rather than
    /// piped: nothing would read a pipe for the lifetime of a mount, and a full
    /// pipe stops the daemon that owns it. What it prints ends up in the session
    /// log, which is where a mount that dies on its own should be visible.
    pub fn serve(&self) -> Result<std::process::Child> {
        let mut builder = std::process::Command::new(&self.program);
        builder
            .args(&self.args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::inherit());
        for (key, value) in &self.env {
            builder.env(key, value);
        }
        builder.spawn().map_err(|e| {
            Error::Mount(format!(
                "{}: cannot start {} ({e})",
                self.name(),
                self.program.display()
            ))
        })
    }

    /// Run to completion and report the exit status, with the helper's stderr
    /// turned into a message that is safe to show.
    pub fn status(&self) -> Result<()> {
        let mut builder = std::process::Command::new(&self.program);
        builder.args(&self.args);
        for (key, value) in &self.env {
            builder.env(key, value);
        }
        let output = builder.output().map_err(|e| {
            Error::Mount(format!(
                "{}: cannot start {} ({e})",
                self.name(),
                self.program.display()
            ))
        })?;
        if output.status.success() {
            return Ok(());
        }
        Err(Error::Mount(describe_failure(self, &output)))
    }
}

/// Environment variables whose value *is* the secret, and so must not be
/// searched for it — the check is about argv, not about where the secret is
/// allowed to live.
fn is_secret_var(key: &str) -> bool {
    key.ends_with("_PASS") || key.ends_with("_PASSWORD")
}

/// Turn a helper's exit into one line the user can act on. Helpers write their
/// reason to stderr; the first line of that is usually the useful part, and it
/// never contains the password, which went in on stdin.
fn describe_failure(command: &Command, output: &std::process::Output) -> String {
    let detail = String::from_utf8_lossy(&output.stderr);
    let first = detail
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    let name = command.name();
    let Some(code) = output.status.code() else {
        // Killed by a signal: an OOM, or a timeout we imposed.
        return format!("{name} was killed");
    };
    if !first.is_empty() {
        return format!("{name}: {first}");
    }
    // mount(8) and mount.cifs both use 32 for "mount failed", which in practice
    // means a wrong password or a share that does not exist, and 2 for usage.
    if code == 32 || code == 2 {
        return format!("{name}: access denied, or the share does not exist");
    }
    format!("{name} failed with status {code}")
}

/// A mount or unmount, ready to run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    command: Command,
    privilege: Privilege,
    mountpoint: PathBuf,
}

impl Plan {
    /// Run as the user, after making sure `mountpoint` exists.
    #[must_use]
    pub fn user(command: Command, mountpoint: impl Into<PathBuf>) -> Self {
        Self {
            command,
            privilege: Privilege::User,
            mountpoint: mountpoint.into(),
        }
    }

    /// Run as root. The command is the helper's own argv; [`Plan::elevated`]
    /// wraps it in `pkexec`.
    #[must_use]
    pub fn root(command: Command, mountpoint: impl Into<PathBuf>) -> Self {
        Self {
            command,
            privilege: Privilege::Root,
            mountpoint: mountpoint.into(),
        }
    }

    #[must_use]
    pub fn command(&self) -> &Command {
        &self.command
    }

    #[must_use]
    pub const fn privilege(&self) -> Privilege {
        self.privilege
    }

    #[must_use]
    pub fn mountpoint(&self) -> &Path {
        &self.mountpoint
    }

    #[must_use]
    pub const fn secret_use(&self) -> SecretUse {
        self.command.secret_use()
    }

    /// The command as it should actually be run. For a root plan this is
    /// `pkexec` followed by the polkit action id and the helper's arguments:
    /// naming the action rather than a path is what makes the polkit policy the
    /// thing that decides which binary may run, and the policy annotates
    /// `org.freedesktop.policykit.exec.path` with the helper's location.
    #[must_use]
    pub fn run(&self, pkexec: &Path, action: &str) -> Command {
        match self.privilege {
            Privilege::User => self.command.clone(),
            Privilege::Root => {
                let mut out = Command::new(pkexec).arg(action);
                for arg in self.command.argv() {
                    out = out.arg(arg);
                }
                for (key, value) in self.command.environment().to_vec() {
                    out = out.env(key, value);
                }
                out.secret(self.command.secret_use())
            }
        }
    }

    /// The directory that has to exist before this plan can run, created `0700`.
    ///
    /// The privileged helper will not create it, because creating a directory as
    /// root in a location the user controls invites a race with whatever else is
    /// writing there, and it refuses a target that group or other can read — so
    /// the mode set here is part of the request being accepted, not cosmetics.
    /// A remount reuses the directory and resets its mode rather than trusting
    /// what happens to be on disk.
    pub fn prepare(&self) -> Result<()> {
        let point = &self.mountpoint;
        let failed = |context: &str, e: &io::Error| {
            Error::Mount(format!(
                "cannot prepare {}: {context}: {e}",
                point.display()
            ))
        };
        if let Err(e) = std::fs::create_dir(point) {
            if e.kind() == io::ErrorKind::AlreadyExists {
                return std::fs::set_permissions(point, Permissions::from_mode(0o700))
                    .map_err(|e| failed("resetting the mount point", &e));
            }
            if e.kind() != io::ErrorKind::NotFound {
                return Err(failed("creating the mount point", &e));
            }
            // The first mount of the session: the mount root does not exist yet.
            let root = point
                .parent()
                .ok_or_else(|| Error::Mount(format!("{} has no parent", point.display())))?;
            std::fs::create_dir_all(root).map_err(|e| failed("creating the mount root", &e))?;
            std::fs::set_permissions(root, Permissions::from_mode(0o700))
                .map_err(|e| failed("protecting the mount root", &e))?;
            std::fs::create_dir(point).map_err(|e| failed("creating the mount point", &e))?;
        }
        // `create_dir` applies the umask, so a fresh directory is `0755` for an
        // instant before this line. It is empty in that instant, and the helper's
        // refusal is what turns leaving it that way into a failed mount rather
        // than an open one.
        std::fs::set_permissions(point, Permissions::from_mode(0o700))
            .map_err(|e| failed("protecting the mount point", &e))
    }
}

/// The polkit action the privileged helper is registered under.
pub const ACTION: &str = "org.uflinux.files.mount";

/// The privileged helper, which does the `mount(2)` calls for `cifs` and `nfs`.
pub const HELPER: &str = "/usr/libexec/uflinux-files-mount";

/// `pkexec`, which is how a GUI asks for root. `doas` cannot be used here: it
/// reads its password from `/dev/tty`, so it only works from a terminal.
pub const PKEXEC: &str = "/usr/bin/pkexec";

/// Build the argv for the privileged helper. Kept here so the helper's argument
/// parser and the planner cannot drift apart.
///
/// The result declares no [`SecretUse`]: the helper reads a password from the
/// `credentials=` file its caller wrote at `0600` and never from stdin, so the
/// caller says where the secret goes — `mount_cifs` marks a password mount
/// [`SecretUse::File`], and a guest mount or an unmount stays
/// [`SecretUse::None`].
#[must_use]
pub fn helper_argv(
    verb: &str,
    fstype: &str,
    source: &str,
    target: &Path,
    options: &str,
) -> Command {
    Command::new(HELPER)
        .arg(verb)
        .arg("--type")
        .arg(fstype)
        .arg("--source")
        .arg(source)
        .arg("--target")
        .arg(path(target))
        .arg("--options")
        .arg(options)
}

/// `Path` to a `String`, lossy only for names that cannot be mounted anyway.
fn path(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

/// Quote-free join for the comma-separated option lists both kernel filesystems
/// take. Values are already checked for `,` and NUL when the URI is parsed, so
/// an option cannot smuggle an extra mount option in.
pub(crate) fn join_options(options: &[String]) -> String {
    options.join(",")
}

/// Whether `name` looks like a program rather than a path with arguments in it.
pub(crate) fn sane_program(name: &OsStr) -> bool {
    let text = name.to_string_lossy();
    !text.is_empty() && !text.contains(' ') && !text.contains('\n')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_command_keeps_its_arguments_in_order() {
        let c = Command::new("/bin/mount")
            .arg("-t")
            .arg("cifs")
            .with_args(["--ro", "/x"]);
        assert_eq!(
            c.argv(),
            vec!["/bin/mount", "-t", "cifs", "--ro", "/x"],
            "{:?}",
            c.argv()
        );
        assert_eq!(c.name(), "mount");
        assert_eq!(c.arguments().len(), 4);
    }

    #[test]
    fn display_quotes_only_what_needs_it() {
        let c = Command::new("/bin/mount")
            .arg("-o")
            .arg("dir_mode=0700")
            .arg("two words")
            .arg("say \"hi\"");
        assert_eq!(
            c.display(),
            r#"/bin/mount -o dir_mode=0700 "two words" "say \"hi\"""#
        );
    }

    #[test]
    fn a_password_in_argv_is_caught() {
        let c = Command::new("/bin/mount").arg("-o").arg("pass=hunter2");
        assert!(c.mentions("hunter2"));
        assert!(!c.mentions(""));
        let clean = Command::new("/bin/mount")
            .arg("-o")
            .arg("credentials=/tmp/x");
        assert!(!clean.mentions("hunter2"));
    }

    #[test]
    fn a_secret_environment_variable_is_not_a_leak() {
        let c = Command::new("/usr/bin/rclone")
            .arg("mount")
            .env("RCLONE_CONFIG_REMOTE_PASS", "hunter2");
        assert!(!c.mentions("hunter2"), "the env var is where it belongs");
        let leaked = Command::new("/usr/bin/rclone")
            .arg("mount")
            .arg("hunter2")
            .env("RCLONE_CONFIG_REMOTE_PASS", "hunter2");
        assert!(leaked.mentions("hunter2"));
    }

    #[test]
    fn a_root_plan_is_wrapped_in_the_action_id() {
        let inner = helper_argv(
            "mount",
            "cifs",
            "//srv/share",
            Path::new("/run/user/1000/ufm/srv-share"),
            "uid=1000",
        );
        let plan = Plan::root(
            inner.secret(SecretUse::File),
            "/run/user/1000/ufm/srv-share",
        );
        assert_eq!(plan.privilege(), Privilege::Root);
        let run = plan.run(Path::new(PKEXEC), ACTION);
        assert_eq!(run.program(), Path::new(PKEXEC));
        assert_eq!(run.arguments()[0], ACTION, "pkexec takes the action first");
        assert_eq!(run.arguments()[1], HELPER);
        assert_eq!(run.arguments()[2], "mount");
        assert_eq!(
            run.secret_use(),
            SecretUse::File,
            "wrapping must not lose where the password goes"
        );
        assert!(!run.mentions("hunter2"));
    }

    #[test]
    fn a_user_plan_runs_unchanged() {
        let inner = Command::new("/usr/bin/sshfs").arg("host:/x").arg("/mnt");
        let plan = Plan::user(inner.clone(), "/mnt");
        assert_eq!(plan.privilege(), Privilege::User);
        assert_eq!(plan.run(Path::new(PKEXEC), ACTION), inner);
        assert_eq!(plan.mountpoint(), Path::new("/mnt"));
    }

    #[test]
    fn the_helper_itself_declares_no_password_channel() {
        // The helper reads `credentials=` from a file, so neither type implies a
        // channel here; a plan that needs one says so at the call site. Getting
        // this wrong would have the runner write a password to a stdin nobody
        // reads and wait for a child that already gave up.
        for fstype in ["cifs", "nfs"] {
            assert_eq!(
                helper_argv("mount", fstype, "s:/x", Path::new("/m"), "").secret_use(),
                SecretUse::None,
                "{fstype}"
            );
        }
    }

    #[test]
    fn the_helper_argv_is_the_documented_shape() {
        let c = helper_argv(
            "unmount",
            "nfs",
            "host:/export",
            Path::new("/run/user/1000/ufm/x"),
            "soft",
        );
        assert_eq!(
            c.argv(),
            vec![
                HELPER,
                "unmount",
                "--type",
                "nfs",
                "--source",
                "host:/export",
                "--target",
                "/run/user/1000/ufm/x",
                "--options",
                "soft"
            ]
        );
    }

    #[test]
    fn options_join_with_commas_and_nothing_else() {
        assert_eq!(join_options(&[]), "");
        assert_eq!(join_options(&["ro".into()]), "ro");
        assert_eq!(
            join_options(&["vers=3.1".into(), "uid=1000".into()]),
            "vers=3.1,uid=1000"
        );
    }

    #[test]
    fn a_program_name_may_not_carry_arguments() {
        assert!(sane_program(OsStr::new("mount")));
        assert!(sane_program(OsStr::new("/usr/bin/mount")));
        assert!(!sane_program(OsStr::new("")));
        assert!(!sane_program(OsStr::new("mount -t")));
        assert!(!sane_program(OsStr::new("mount\n")));
    }

    #[test]
    fn preparing_creates_the_mount_point_at_the_mode_the_helper_demands() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("mnt").join("srv-share");
        let plan = Plan::user(Command::new("/bin/true"), &target);
        assert!(!target.exists());
        plan.prepare().expect("prepare");
        assert!(target.is_dir());
        // Idempotent, because a remount after a failure reuses the directory.
        plan.prepare().expect("prepare again");

        #[cfg(unix)]
        {
            use std::os::unix::fs::{MetadataExt, PermissionsExt};
            // The privileged helper refuses a target that group or other can
            // read, so a mode left at the umask default would turn every mount
            // into a failure that looks like a polkit problem.
            let meta = std::fs::metadata(&target).expect("metadata");
            assert_eq!(meta.permissions().mode() & 0o777, 0o700);
            assert_eq!(meta.uid(), ufm_core::paths::uid());
            // The mount root too, since it holds every share's credentials-adjacent
            // directory and is created here on the first mount of a session.
            let root = std::fs::metadata(target.parent().expect("parent")).expect("root meta");
            assert_eq!(root.permissions().mode() & 0o777, 0o700);

            // A directory that got the wrong mode between mounts is reset rather
            // than trusted.
            std::fs::set_permissions(&target, Permissions::from_mode(0o755)).expect("chmod");
            plan.prepare().expect("re-prepare");
            assert_eq!(
                std::fs::metadata(&target)
                    .expect("metadata")
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
        }
    }

    #[test]
    fn a_missing_program_is_reported_with_its_path() {
        let c = Command::new("/nonexistent/ufm-helper").arg("mount");
        let err = c.status().unwrap_err();
        let text = err.to_string();
        assert!(text.contains("ufm-helper"), "{text}");
        assert!(matches!(err, Error::Mount(_)));
    }
}
