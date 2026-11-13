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
use std::path::{Path, PathBuf};

use ufm_core::error::{Error, Result};

/// Where the child process expects to find a password.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecretUse {
    /// The protocol takes no password: NFS, and SFTP with a key or an agent.
    None,
    /// One line on stdin, then EOF. `mount.cifs` does this when it is not
    /// attached to a terminal.
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

    /// The directory that has to exist before this plan can run. The privileged
    /// helper will not create it, because creating a directory as root in a
    /// location the user controls invites a race with whatever else is writing
    /// there; the file manager creates it at `0700` first.
    pub fn prepare(&self) -> Result<()> {
        std::fs::create_dir_all(&self.mountpoint)
            .map_err(|e| Error::Mount(format!("cannot create {}: {e}", self.mountpoint.display())))
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
#[must_use]
pub fn helper_argv(
    verb: &str,
    fstype: &str,
    source: &str,
    target: &Path,
    options: &str,
) -> Command {
    let mut command = Command::new(HELPER)
        .arg(verb)
        .arg("--type")
        .arg(fstype)
        .arg("--source")
        .arg(source)
        .arg("--target")
        .arg(path(target))
        .arg("--options")
        .arg(options);
    if fstype == "cifs" {
        command = command.secret(SecretUse::Stdin);
    }
    command
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
        let plan = Plan::root(inner.clone(), "/run/user/1000/ufm/srv-share");
        assert_eq!(plan.privilege(), Privilege::Root);
        let run = plan.run(Path::new(PKEXEC), ACTION);
        assert_eq!(run.program(), Path::new(PKEXEC));
        assert_eq!(run.arguments()[0], ACTION, "pkexec takes the action first");
        assert_eq!(run.arguments()[1], HELPER);
        assert_eq!(run.arguments()[2], "mount");
        assert_eq!(run.secret_use(), SecretUse::Stdin);
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
    fn only_cifs_asks_for_a_password_on_stdin() {
        assert_eq!(
            helper_argv("mount", "cifs", "//s/x", Path::new("/m"), "").secret_use(),
            SecretUse::Stdin
        );
        assert_eq!(
            helper_argv("mount", "nfs", "s:/x", Path::new("/m"), "").secret_use(),
            SecretUse::None
        );
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
    fn preparing_creates_the_mount_point() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("mnt").join("srv-share");
        let plan = Plan::user(Command::new("/bin/true"), &target);
        assert!(!target.exists());
        plan.prepare().expect("prepare");
        assert!(target.is_dir());
        // Idempotent, because a remount after a failure reuses the directory.
        plan.prepare().expect("prepare again");
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
