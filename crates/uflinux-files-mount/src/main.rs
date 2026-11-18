//! The privileged helper: `/usr/libexec/uflinux-files-mount`.
//!
//! SMB and NFS are kernel filesystems, so attaching one needs `mount(2)`, so
//! something here has to run as root. UFLinux has no `udisks2` to ask, so the
//! file manager builds a [`Plan`] in `ufm-mount`, wraps it in `pkexec` with the
//! `org.uflinux.files.mount` action, and runs this program. That is the whole
//! privilege boundary, and it is deliberately narrow:
//!
//! * the argv shape is fixed by [`ufm_mount::plan::helper_argv`], which this
//!   file parses rather than re-derives, so planner and helper cannot drift;
//! * only `cifs` and `nfs` are accepted, because those are the only types that
//!   need root — everything FUSE runs as the user and never reaches here;
//! * the target must already exist, be owned by the calling user, be `0700` and
//!   be a real directory rather than a symlink, which is what makes "mount
//!   anywhere root's writable" impossible: the file manager creates mount points
//!   at `0700` itself and root never creates them. For a mount it must also be
//!   empty, so the target cannot be a directory that already holds files the
//!   mount would bury;
//! * no shell, and no argument that starts with `-`, so nothing here can be
//!   re-read as an option by `mount(8)`;
//! * a `credentials=` file is verified to belong to the caller before
//!   `mount.cifs` is let near it as root.
//!
//! `pkexec` hands us a minimal environment (`PATH` among them, rewritten), so
//! `mount` and `umount` are looked for in a fixed list of directories rather
//! than in `$PATH`: an attacker who controls `PATH` would otherwise get root to
//! execute a program of their choosing, which is the escalation rather than an
//! inconvenience.
//!
//! Exit codes: `0` success, including the two no-ops (already mounted, and not
//! mounted when asked to unmount), so a retry after a lost reply is not an error;
//! `1` the mount or unmount itself failed; `2` the request was refused before
//! anything ran. `describe_failure` in `ufm-mount` prefers our first stderr line
//! over its own generic text, so every message here is one sentence that names
//! what to do.

// The prose in this file names products and protocols — pkexec, WebDAV,
// mount.cifs — which `doc_markdown` reads as unquoted code.
#![allow(clippy::doc_markdown)]

use std::io::Write;
use std::os::unix::fs::FileTypeExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::process::{Command as OsCommand, ExitCode, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use ufm_mount::credentials::read_protected_by;
use ufm_mount::monitor::{MountState, observe};

/// The filesystem types this helper will touch. The two kernel filesystems, and
/// only those: `ufm-mount` sends every other protocol to a FUSE helper running
/// as the user.
const TYPES: &[&str] = &["cifs", "nfs"];

/// Where `mount` and `umount` are looked for. Searched in this order, and `$PATH`
/// is not consulted — see the module comment.
const PROGRAM_DIRS: &[&str] = &["/usr/bin", "/bin", "/usr/sbin", "/sbin"];

/// `mount.cifs` and `mount.nfs` can block on an unreachable server for minutes.
/// Long enough to be a real timeout, short enough that the UI is not left
/// waiting on a prompt it cannot see.
const DEADLINE: Duration = Duration::from_secs(60);

/// Longest accepted source, target, or option string. A mount option list this
/// long is a mistake or an attack, never a share.
const MAX_LEN: usize = 4096;

/// Longest single mount option, and the most options accepted.
const MAX_OPTION_LEN: usize = 255;
const MAX_OPTIONS: usize = 64;

/// `PKEXEC_UID`, which pkexec sets to the uid of the process that asked. This is
/// the only trustworthy answer to "whose directory is this": the helper's own
/// euid is `0`, and `$USER`/`$HOME` are not even in the environment pkexec
/// passes on.
const PKEXEC_UID: &str = "PKEXEC_UID";

/// What was asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verb {
    Mount,
    Unmount,
}

impl Verb {
    fn parse(word: &str) -> Result<Self, String> {
        match word {
            "mount" => Ok(Self::Mount),
            "unmount" => Ok(Self::Unmount),
            other => Err(format!(
                "unknown request {other:?}: expected mount or unmount"
            )),
        }
    }
}

/// A request that has been parsed but not yet checked against the filesystem.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Request {
    verb: Verb,
    fstype: &'static str,
    source: String,
    target: PathBuf,
    options: Vec<String>,
}

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if argv.is_empty() || matches!(argv[0].as_str(), "-h" | "--help" | "help") {
        // No arguments is a mistake, so the usage goes to stderr with the exit
        // code that means "refused"; asking for help is a request that succeeded.
        if argv.is_empty() {
            let _ = writeln!(std::io::stderr(), "{USAGE}");
            return ExitCode::from(2);
        }
        let _ = writeln!(std::io::stdout(), "{USAGE}");
        return ExitCode::SUCCESS;
    }
    match dispatch(&argv) {
        Ok(0) => ExitCode::SUCCESS,
        Ok(code) => ExitCode::from(code),
        Err(message) => {
            let _ = writeln!(std::io::stderr(), "{message}");
            ExitCode::from(2)
        }
    }
}

const USAGE: &str = "usage: uflinux-files-mount mount|unmount --type cifs|nfs --source SOURCE --target DIR [--options OPTS]";

/// Parse, check, and run. The split exists so the tests can cover every
/// rejection without a root-owned mount namespace.
fn dispatch(argv: &[String]) -> Result<u8, String> {
    if argv
        .first()
        .is_some_and(|s| matches!(s.as_str(), "device-mount" | "device-unmount"))
    {
        return device_dispatch(argv);
    }
    let request = parse(argv)?;
    let owner = owner()?;
    let request = check(&request, owner)?;
    Ok(run(&request))
}

/// Local volumes have a separate, fixed argument shape and mount root. They do
/// not accept caller-supplied filesystem types or mount options.
fn device_dispatch(argv: &[String]) -> Result<u8, String> {
    if argv.len() != 5 || argv[1] != "--device" || argv[3] != "--target" {
        return Err(
            "device request expects device-mount|device-unmount --device /dev/NAME --target DIR"
                .into(),
        );
    }
    let owner = owner()?;
    let mount = argv[0] == "device-mount";
    let source = PathBuf::from(&argv[2]);
    let device =
        std::fs::canonicalize(&source).map_err(|e| format!("{}: {e}", source.display()))?;
    if !device.starts_with("/dev/")
        || !std::fs::metadata(&device)
            .map_err(|e| e.to_string())?
            .file_type()
            .is_block_device()
    {
        return Err("the source is not a block device under /dev".into());
    }
    let target = PathBuf::from(&argv[4]);
    let allowed = PathBuf::from(format!("/run/user/{owner}/ufm/devices"));
    if target.parent() != Some(allowed.as_path()) || target.file_name().is_none() {
        return Err("the target is outside this user's device mount directory".into());
    }
    let canonical = std::fs::canonicalize(&target).map_err(|e| e.to_string())?;
    if canonical != target {
        return Err("the mount target contains a symlink".into());
    }
    let request = Request {
        verb: if mount { Verb::Mount } else { Verb::Unmount },
        fstype: "ext4",
        source: device.display().to_string(),
        target: target.clone(),
        options: Vec::new(),
    };
    if mount {
        check_target(&request, owner)?;
    } else {
        let meta = std::fs::metadata(&allowed).map_err(|e| e.to_string())?;
        if meta.uid() != owner || meta.permissions().mode() & 0o077 != 0 {
            return Err("the device mount directory is not private to the caller".into());
        }
    }
    let observer = ufm_mount::monitor::Observer::new();
    let existing = observer.find(&target);
    if mount {
        if existing.is_some() {
            return Err("the target is already mounted".into());
        }
        if observer
            .points()
            .iter()
            .any(|point| point.source == device.display().to_string())
        {
            return Err("the device is already mounted elsewhere".into());
        }
        let blkid = program("blkid").ok_or("blkid is not installed")?;
        let output = OsCommand::new(blkid)
            .args(["-o", "value", "-s", "TYPE"])
            .arg(&device)
            .output()
            .map_err(|e| e.to_string())?;
        if !output.status.success() {
            return Err("cannot determine the device filesystem".into());
        }
        let kind = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !matches!(
            kind.as_str(),
            "ext2"
                | "ext3"
                | "ext4"
                | "btrfs"
                | "xfs"
                | "f2fs"
                | "vfat"
                | "exfat"
                | "ntfs"
                | "ntfs3"
                | "iso9660"
                | "udf"
        ) {
            return Err(format!("{kind} is not a supported unencrypted filesystem"));
        }
        let mut options = "nosuid,nodev".to_string();
        if matches!(kind.as_str(), "iso9660" | "udf") {
            options.push_str(",ro,noexec");
        }
        if matches!(kind.as_str(), "vfat" | "exfat" | "ntfs" | "ntfs3") {
            let group = std::fs::metadata(&allowed)
                .map_err(|e| e.to_string())?
                .gid();
            options.push_str(&format!(",uid={owner},gid={group},umask=077"));
        }
        let program = program("mount").ok_or("mount is not installed")?;
        let mut command = OsCommand::new(program);
        command
            .args(["-t", &kind, "-o", &options])
            .arg(&device)
            .arg(&target);
        Ok(execute(&mut command, "mount"))
    } else {
        let Some(point) = existing else {
            return Ok(0);
        };
        if point.source != device.display().to_string() {
            return Err("the target holds a different device".into());
        }
        let program = program("umount").ok_or("umount is not installed")?;
        let mut command = OsCommand::new(program);
        command.arg(&target);
        Ok(execute(&mut command, "umount"))
    }
}

/// The uid whose directories and files this request is allowed to touch.
fn owner() -> Result<u32, String> {
    let euid = ufm_core::paths::uid();
    if euid != 0 {
        // Run by hand from a terminal, or by a test. Nothing is privileged, so
        // the caller's own uid is the right answer and `mount` will fail on its
        // own if it turns out not to be allowed.
        return Ok(euid);
    }
    let given = std::env::var(PKEXEC_UID).map_err(|_| {
        "PKEXEC_UID is missing: this helper has to be run through pkexec".to_string()
    })?;
    owner_from(0, Some(&given))
}

/// [`owner`] with both inputs explicit, so the two failure modes are testable.
fn owner_from(euid: u32, pkexec_uid: Option<&str>) -> Result<u32, String> {
    if euid != 0 {
        return Ok(euid);
    }
    let text = pkexec_uid.ok_or_else(|| format!("{PKEXEC_UID} is missing: {USAGE}"))?;
    let uid = text
        .trim()
        .parse::<u32>()
        .map_err(|_| format!("{PKEXEC_UID}={text:?} is not a user id"))?;
    if uid == 0 {
        return Err(format!(
            "{PKEXEC_UID}=0: a share is mounted for a user, not for root"
        ));
    }
    Ok(uid)
}

/// Parse the exact shape `helper_argv` emits: a verb, then four flags, each
/// exactly once, in any order. Anything else is refused rather than guessed at,
/// because a helper that guesses is a helper an attacker can phrase at.
fn parse(argv: &[String]) -> Result<Request, String> {
    let (verb_word, rest) = argv
        .split_first()
        .ok_or_else(|| format!("no request: {USAGE}"))?;
    let verb = Verb::parse(verb_word)?;
    let mut type_ = None;
    let mut source = None;
    let mut target = None;
    let mut options = None;
    let mut rest = rest;
    while let Some((flag, tail)) = rest.split_first() {
        rest = tail;
        let slot = match flag.as_str() {
            "--type" => &mut type_,
            "--source" => &mut source,
            "--target" => &mut target,
            "--options" => &mut options,
            other => return Err(format!("unexpected argument {other:?}: {USAGE}")),
        };
        let Some(value) = rest.first() else {
            return Err(format!("{flag} needs a value"));
        };
        if slot.is_some() {
            // Twice would mean the last one wins, which is exactly how an
            // argument gets smuggled past a check written for the first.
            return Err(format!("{flag} given twice"));
        }
        if value.len() > MAX_LEN {
            return Err(format!("{flag} is longer than {MAX_LEN} characters"));
        }
        *slot = Some(value.clone());
        rest = &rest[1..];
    }
    let taken = |slot: &mut Option<String>, name: &str| -> Result<String, String> {
        slot.take()
            .ok_or_else(|| format!("{name} is missing: {USAGE}"))
    };
    let fstype = check_fstype(&taken(&mut type_, "--type")?)?;
    Ok(Request {
        verb,
        fstype,
        source: taken(&mut source, "--source")?,
        target: PathBuf::from(taken(&mut target, "--target")?),
        options: split_options(&taken(&mut options, "--options")?)?,
    })
}

/// The filesystem type, checked against [`TYPES`]. `'static` because the only
/// answers are the two literals.
fn check_fstype(given: &str) -> Result<&'static str, String> {
    TYPES
        .iter()
        .find(|t| *t == &given)
        .copied()
        .ok_or_else(|| format!("cannot mount {given:?}: only cifs and nfs need a helper"))
}

/// Split and validate the comma-separated option list. Every value ends up as
/// one `-o` argument, so the checks are the ones that keep `mount(8)` from
/// reading an option as a flag or a password.
fn split_options(raw: &str) -> Result<Vec<String>, String> {
    if raw.is_empty() {
        return Ok(Vec::new());
    }
    if raw.split(',').count() > MAX_OPTIONS {
        return Err(format!("more than {MAX_OPTIONS} mount options"));
    }
    raw.split(',')
        .map(|option| {
            let (key, _) = option.split_once('=').unwrap_or((option, ""));
            if key.is_empty() {
                return Err(format!("empty mount option in {raw:?}"));
            }
            if option.len() > MAX_OPTION_LEN {
                return Err(format!("mount option {key} is longer than {MAX_OPTION_LEN}"));
            }
            if option.starts_with('-') {
                return Err(format!("mount option {option:?} cannot start with -"));
            }
            if option.chars().any(|c| c.is_whitespace() || c.is_control()) {
                return Err(format!("mount option {option:?} contains a space"));
            }
            if matches!(key, "pass" | "password") {
                // `pass=` is a `mount.cifs` option that carries the password in
                // argv, where any user on the machine can read it out of
                // /proc/<pid>/cmdline. The credentials file exists for this.
                return Err(format!(
                    "mount option {key} puts a password in the process list; use credentials= instead"
                ));
            }
            Ok(option.to_owned())
        })
        .collect()
}

/// Everything that needs the filesystem, the caller's uid, or both. Returns the
/// request unchanged so the caller can chain it into [`run`].
fn check(request: &Request, owner: u32) -> Result<Request, String> {
    check_source(request)?;
    check_target(request, owner)?;
    check_credentials(request, owner)?;
    check_password_channel(request)?;
    Ok(request.clone())
}

/// The source has to be the shape that filesystem expects, and must not be
/// something `mount(8)` could read as a flag.
fn check_source(request: &Request) -> Result<(), String> {
    let source = request.source.as_str();
    if source.is_empty() {
        return Err("no source given".to_string());
    }
    if source.starts_with('-') {
        return Err(format!("source {source:?} cannot start with -"));
    }
    if source.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(format!("source {source:?} contains a space"));
    }
    match request.fstype {
        "cifs" if !source.starts_with("//") => Err(format!(
            "a SMB source looks like //server/share, not {source:?}"
        )),
        "nfs" if !source.contains(':') || source.starts_with('/') => Err(format!(
            "an NFS source looks like server:/export, not {source:?}"
        )),
        _ => Ok(()),
    }
}

/// The mount point must be a directory the *caller* created, empty, and not a
/// symlink. That combination is the whole safety argument: the file manager
/// creates its mount points at `0700` under a directory the user owns, root
/// never creates them, and a directory that is empty and mode `0700` cannot be
/// anything's live data. So an attacker aiming root at `/etc` or at
/// `~/.config/autostart` is refused without the helper needing to know where the
/// mount root happens to be — which matters, because pkexec strips
/// `$XDG_RUNTIME_DIR` and the helper cannot recompute it.
fn check_target(request: &Request, owner: u32) -> Result<(), String> {
    let target = &request.target;
    if !target.is_absolute() {
        return Err(format!(
            "{}: the mount point must be an absolute path",
            target.display()
        ));
    }
    if target
        .components()
        .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
    {
        return Err(format!(
            "{}: the mount point may not contain . or ..",
            target.display()
        ));
    }
    // `symlink_metadata`, not `metadata`: following a symlink here would let a
    // link planted in the mount root point root at a directory the user does
    // not own, which is the check this function exists to make.
    let meta = std::fs::symlink_metadata(target).map_err(|e| {
        format!(
            "{}: the mount point has to exist first ({}); Files creates it before mounting",
            target.display(),
            e.kind()
        )
    })?;
    if meta.file_type().is_symlink() {
        return Err(format!(
            "{}: the mount point is a symlink",
            target.display()
        ));
    }
    if !meta.is_dir() {
        return Err(format!(
            "{}: the mount point is not a directory",
            target.display()
        ));
    }
    if meta.uid() != owner {
        return Err(format!(
            "{}: the mount point is not yours, so it is not mounted",
            target.display()
        ));
    }
    if meta.permissions().mode() & 0o077 != 0 {
        return Err(format!(
            "{}: the mount point has to be readable only by you (chmod 700)",
            target.display()
        ));
    }
    if request.verb == Verb::Mount {
        check_empty(target)?;
    }
    Ok(())
}

/// Only an empty directory is mounted over. A non-empty one means either a
/// previous mount died with something holding it open, or the caller pointed at
/// a directory with data in it; hiding that data is a worse outcome than
/// refusing, and the sweep's rule is the same one.
fn check_empty(target: &Path) -> Result<(), String> {
    let mut entries = std::fs::read_dir(target)
        .map_err(|e| format!("{}: cannot list the mount point ({e})", target.display()))?;
    if entries.next().is_some() {
        drop(entries);
        return Err(format!(
            "{}: the mount point is not empty, so nothing is mounted over it",
            target.display()
        ));
    }
    Ok(())
}

/// A `credentials=` file is a file the caller names and root reads. Refuse it
/// unless the caller owns it and nobody else can read it, which is what stops
/// `--options credentials=/etc/shadow` from meaning anything.
fn check_credentials(request: &Request, owner: u32) -> Result<(), String> {
    let Some(option) = request
        .options
        .iter()
        .find(|o| o.starts_with("credentials="))
    else {
        return Ok(());
    };
    let path = Path::new(option.trim_start_matches("credentials="));
    if !path.is_absolute() {
        return Err(format!(
            "{}: a credentials file has to be an absolute path",
            path.display()
        ));
    }
    // Reading it is the check: `read_protected_by` refuses a file the caller
    // does not own or that anyone else can read, and its message names the path
    // without the password, which is the only thing in the file. The value
    // itself is dropped here — `mount.cifs` reads the file, not us.
    read_protected_by(path, owner)
        .map(drop)
        .map_err(|e| e.to_string())
}

/// `mount.cifs` prompts on a terminal when it has a user and no password. Our
/// stdin is `/dev/null`, so a prompt would be an instant failure with a message
/// about standard input; say what is actually missing instead.
fn check_password_channel(request: &Request) -> Result<(), String> {
    if request.fstype != "cifs" {
        return Ok(());
    }
    let has = |key: &str| {
        request.options.iter().any(|o| {
            o.split_once('=').is_some_and(|(k, _)| k == key)
                && o.split_once('=').is_some_and(|(_, v)| !v.is_empty())
        })
    };
    if request.options.iter().any(|o| o == "guest") {
        return Ok(());
    }
    if has("username") && !has("credentials") {
        return Err("a SMB mount as another user needs a password file".to_string());
    }
    Ok(())
}

/// Run the request. Returns `0` on success or an accepted no-op, `1` when
/// `mount`/`umount` failed.
fn run(request: &Request) -> u8 {
    match (request.verb, observe(&request.target)) {
        // Mounting over a live mount would leave the old one unreachable with no
        // record of it, which is a leak rather than a remount. Reported as
        // success because the state the caller wanted is already true.
        (Verb::Mount, MountState::Mounted) => {
            eprintln!(
                "{} is already mounted; nothing to do",
                request.target.display()
            );
            0
        }
        // `Stale` is not mounted — the kernel entry is there and nothing answers
        // — but mounting over it would hide the evidence and double the problem.
        // Detaching is the sweep's job, and it needs the same privilege as this.
        (Verb::Mount, MountState::Stale) => {
            eprintln!(
                "{} has a mount that does not answer; detach it before mounting again",
                request.target.display()
            );
            1
        }
        // Unmounting something that is not mounted is what the sweep does to an
        // idle share twice, and what a user does from a card whose mount already
        // died. Success, because it is the state the caller asked for.
        (Verb::Unmount, MountState::Absent) => {
            eprintln!("{} is not mounted", request.target.display());
            0
        }
        _ => {
            let argv = argv(request);
            let Some(program) = program(&argv[0]) else {
                eprintln!(
                    "{} is not installed; it comes from {}",
                    argv[0],
                    match (request.verb, request.fstype) {
                        (Verb::Unmount, _) => "util-linux",
                        (_, "cifs") => "cifs-utils",
                        _ => "nfs-utils",
                    }
                );
                return 1;
            };
            let mut command = OsCommand::new(program);
            command.args(&argv[1..]);
            execute(&mut command, &argv[0])
        }
    }
}

/// The argv to run: `mount -t TYPE [-o OPTS] SOURCE TARGET`, or `umount TARGET`.
/// Built in one place so the shape can be asserted as a whole, and so the only
/// arguments beginning with `-` are the literals here — nothing parsed from the
/// request can be re-read by `mount(8)` as a flag. The program is `argv[0]`, and
/// [`program`] resolves it without consulting `$PATH`.
fn argv(request: &Request) -> Vec<String> {
    let mut argv = vec![
        match request.verb {
            Verb::Mount => "mount",
            Verb::Unmount => "umount",
        }
        .to_owned(),
    ];
    if request.verb == Verb::Mount {
        argv.push("-t".to_owned());
        argv.push(request.fstype.to_owned());
        if !request.options.is_empty() {
            argv.push("-o".to_owned());
            argv.push(request.options.join(","));
        }
        argv.push(request.source.clone());
    }
    argv.push(request.target.display().to_string());
    argv
}

/// Spawn, with a deadline. Stdin is `/dev/null` so a helper that wants a
/// password fails instead of blocking forever, and stderr is inherited so the
/// message from `mount.cifs` reaches the file manager through pkexec unchanged.
fn execute(command: &mut OsCommand, name: &str) -> u8 {
    let mut child = match command.stdin(Stdio::null()).spawn() {
        Ok(child) => child,
        Err(e) => {
            eprintln!("{name}: cannot start it ({e})");
            return 1;
        }
    };
    let timed_out = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&timed_out);
    let pid = ChildPid(child.id());
    let (tx, rx) = std::sync::mpsc::channel::<()>();
    let watchdog = thread::spawn(move || {
        if rx.recv_timeout(DEADLINE).is_err() {
            flag.store(true, Ordering::SeqCst);
            pid.kill();
        }
    });
    let status = child.wait();
    let _ = tx.send(());
    let _ = watchdog.join();
    match status {
        Ok(status) if status.success() => 0,
        Ok(_) if timed_out.load(Ordering::SeqCst) => {
            eprintln!("{name} gave up after {} seconds", DEADLINE.as_secs());
            1
        }
        Ok(status) => {
            // The child's own message is already on stderr; this only names the
            // status so a code of 32 is not mistaken for a refusal.
            eprintln!(
                "{name} failed with {}",
                status
                    .code()
                    .map_or_else(|| "a signal".to_string(), |c| c.to_string())
            );
            1
        }
        Err(e) => {
            eprintln!("{name}: cannot wait for it ({e})");
            1
        }
    }
}

/// Find `name` in [`PROGRAM_DIRS`], not in `$PATH`.
fn program(name: &str) -> Option<PathBuf> {
    PROGRAM_DIRS
        .iter()
        .map(|dir| Path::new(dir).join(name))
        .find(|candidate| candidate.is_file() && executable(candidate))
}

#[cfg(unix)]
fn executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// A child pid, narrowed to what `kill(2)` takes at the point it is used. A
/// `u32`/`i32` cast is lossless for every pid the kernel hands out, and naming
/// the newtype is what keeps the cast from being invisible at the call.
#[derive(Debug, Clone, Copy)]
struct ChildPid(u32);

impl ChildPid {
    fn kill(self) {
        let pid = libc::pid_t::try_from(self.0).unwrap_or(0);
        // SAFETY: kill accepts any pid and its only failure here is ESRCH, which
        // the discarded return value ignores. The pid is this process's own child
        // and has not been reaped, so it cannot name an unrelated process.
        let _ = unsafe { libc::kill(pid, libc::SIGKILL) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// tempfile creates `0700` directories owned by the caller, which is exactly
    /// what `check_target` asks for.
    fn temp_root() -> tempfile::TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    fn mount_point() -> (tempfile::TempDir, PathBuf) {
        let root = temp_root();
        let point = root.path().join("srv-share");
        std::fs::create_dir(&point).expect("mkdir");
        std::fs::set_permissions(&point, std::fs::Permissions::from_mode(0o700)).expect("chmod");
        (root, point)
    }

    fn request(verb: Verb, fstype: &str, source: &str, target: &Path, options: &str) -> Request {
        parse(&[
            verb_word(verb).to_string(),
            "--type".to_string(),
            fstype.to_string(),
            "--source".to_string(),
            source.to_string(),
            "--target".to_string(),
            target.display().to_string(),
            "--options".to_string(),
            options.to_string(),
        ])
        .expect("parses")
    }

    fn verb_word(verb: Verb) -> &'static str {
        match verb {
            Verb::Mount => "mount",
            Verb::Unmount => "unmount",
        }
    }

    #[test]
    fn the_documented_shape_parses() {
        let point = Path::new("/run/user/1000/ufm/srv-share");
        let r = request(
            Verb::Mount,
            "cifs",
            "//srv/share",
            point,
            "uid=1000,guest,ro",
        );
        assert_eq!(r.verb, Verb::Mount);
        assert_eq!(r.fstype, "cifs");
        assert_eq!(r.source, "//srv/share");
        assert_eq!(r.target, point);
        assert_eq!(r.options, vec!["uid=1000", "guest", "ro"]);
    }

    #[test]
    fn the_flags_may_come_in_any_order_but_only_once() {
        let point = Path::new("/m");
        let argv = [
            "unmount",
            "--target",
            "/m",
            "--type",
            "nfs",
            "--source",
            "h:/e",
            "--options",
            "",
        ]
        .map(String::from)
        .to_vec();
        let r = parse(&argv).expect("parses");
        assert_eq!(r.verb, Verb::Unmount);
        assert_eq!(r.fstype, "nfs");
        assert!(r.options.is_empty());

        let doubled = [
            "mount",
            "--type",
            "nfs",
            "--type",
            "cifs",
            "--source",
            "h:/e",
            "--target",
            "/m",
            "--options",
            "",
        ]
        .map(String::from)
        .to_vec();
        let err = parse(&doubled).unwrap_err();
        assert!(err.contains("--type") && err.contains("twice"), "{err}");
        let _ = point;
    }

    #[test]
    fn a_missing_or_unknown_argument_is_named() {
        let err =
            parse(&["mount".to_string(), "--type".to_string(), "nfs".to_string()]).unwrap_err();
        assert!(err.contains("--source"), "{err}");
        let err = parse(&[
            "mount".to_string(),
            "--fstype".to_string(),
            "nfs".to_string(),
        ])
        .unwrap_err();
        assert!(err.contains("--fstype"), "{err}");
        let err = parse(&["mount".to_string(), "--source".to_string()]).unwrap_err();
        assert!(err.contains("needs a value"), "{err}");
        let err = parse(&["remount".to_string()]).unwrap_err();
        assert!(err.contains("mount or unmount"), "{err}");
    }

    #[test]
    fn only_the_two_kernel_filesystems_are_accepted() {
        assert_eq!(check_fstype("cifs").expect("cifs"), "cifs");
        assert_eq!(check_fstype("nfs").expect("nfs"), "nfs");
        for given in ["fuse", "vfat", "cifs,", "", "CIFS", "nfs4", "ext4"] {
            let err = check_fstype(given).unwrap_err();
            assert!(err.contains("cifs and nfs"), "{given}: {err}");
        }
    }

    #[test]
    fn a_source_has_to_look_like_the_protocol_expects() {
        let point = Path::new("/m");
        let smb = request(Verb::Mount, "cifs", "//srv/s", point, "guest");
        check_source(&smb).expect("smb source");
        let nfs = request(Verb::Mount, "nfs", "srv:/export", point, "");
        check_source(&nfs).expect("nfs source");
        for (fstype, source) in [
            ("cifs", "srv/share"),
            ("cifs", "-o"),
            ("cifs", "//srv two"),
            ("nfs", "/export"),
            ("nfs", "-x"),
            ("nfs", "srv:two paths"),
        ] {
            let r = request(Verb::Mount, fstype, source, point, "");
            let err = check_source(&r).unwrap_err();
            assert!(err.contains(source), "{source}: {err}");
        }
    }

    #[test]
    fn an_option_cannot_smuggle_a_flag_or_a_password() {
        assert_eq!(split_options("").expect("empty"), Vec::<String>::new());
        assert_eq!(
            split_options("uid=1000,dir_mode=0700").expect("two"),
            vec!["uid=1000", "dir_mode=0700"]
        );
        for option in [
            "-o",
            "ro,noatime,-x",
            "uid=1000, vers=3",
            "pass=hunter2",
            "password=hunter2",
            "=value",
        ] {
            let err = split_options(option).unwrap_err();
            assert!(!err.is_empty(), "{option} accepted");
            assert!(!err.contains("hunter2"), "{err} leaked the password");
        }
    }

    #[test]
    fn the_mount_point_has_to_be_ours_empty_and_ours() {
        let (root, point) = mount_point();
        let r = request(Verb::Mount, "nfs", "srv:/e", &point, "");
        check_target(&r, uid()).expect("fresh");

        // Not ours: readable by the group, which is what a directory created by
        // anything other than Files looks like.
        std::fs::set_permissions(&point, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        let err = check_target(&r, uid()).unwrap_err();
        assert!(err.contains("700"), "{err}");
        std::fs::set_permissions(&point, std::fs::Permissions::from_mode(0o700)).expect("chmod");

        // Somebody else's uid.
        let err = check_target(&r, uid() + 1).unwrap_err();
        assert!(err.contains("not yours"), "{err}");

        // Not empty.
        std::fs::write(point.join("leftover"), b"x").expect("write");
        let err = check_target(&r, uid()).unwrap_err();
        assert!(err.contains("not empty"), "{err}");
        // An unmount is allowed over leftovers: they are usually the contents of
        // the dead mount itself.
        let down = request(Verb::Unmount, "nfs", "srv:/e", &point, "");
        check_target(&down, uid()).expect("unmount over leftovers");
        std::fs::remove_file(point.join("leftover")).expect("rm");

        // A symlink, a missing directory, and a relative path.
        let link = root.path().join("link");
        std::os::unix::fs::symlink(&point, &link).expect("symlink");
        let err =
            check_target(&request(Verb::Mount, "nfs", "srv:/e", &link, ""), uid()).unwrap_err();
        assert!(err.contains("symlink"), "{err}");
        let err = check_target(
            &request(
                Verb::Mount,
                "nfs",
                "srv:/e",
                &root.path().join("absent"),
                "",
            ),
            uid(),
        )
        .unwrap_err();
        assert!(err.contains("has to exist"), "{err}");
        let err = check_target(
            &request(Verb::Mount, "nfs", "srv:/e", Path::new("ufm/x"), ""),
            uid(),
        )
        .unwrap_err();
        assert!(err.contains("absolute"), "{err}");
        let err = check_target(
            &request(
                Verb::Mount,
                "nfs",
                "srv:/e",
                Path::new("/run/user/1000/../ufm/x"),
                "",
            ),
            uid(),
        )
        .unwrap_err();
        assert!(err.contains(".."), "{err}");
    }

    #[test]
    fn a_credentials_file_anyone_can_read_is_refused() {
        let (root, point) = mount_point();
        let conf = root.path().join("srv-share.conf");
        std::fs::write(&conf, "username=alice\npassword=hunter2\n").expect("write");
        // The runner writes this file at 0600; `fs::write` would leave it 0644
        // under a normal umask, which is a refusal rather than a mount.
        std::fs::set_permissions(&conf, std::fs::Permissions::from_mode(0o600)).expect("chmod");
        let options = format!("username=alice,credentials={}", conf.display());
        let r = request(Verb::Mount, "cifs", "//srv/s", &point, &options);
        check_credentials(&r, uid()).expect("0600 file");
        assert!(!argv(&r).join(" ").contains("hunter2"));

        std::fs::set_permissions(&conf, std::fs::Permissions::from_mode(0o644)).expect("chmod");
        let err = check_credentials(&r, uid()).unwrap_err();
        assert!(err.contains("readable by anyone else"), "{err}");
        assert!(!err.contains("hunter2"), "{err}");

        // Somebody else's file, same mode.
        std::fs::set_permissions(&conf, std::fs::Permissions::from_mode(0o600)).expect("chmod");
        let err = check_credentials(&r, uid() + 1).unwrap_err();
        assert!(!err.contains("hunter2"), "{err}");

        // A relative name, and a file that is not there.
        let err = check_credentials(
            &request(Verb::Mount, "cifs", "//srv/s", &point, "credentials=x.conf"),
            uid(),
        )
        .unwrap_err();
        assert!(err.contains("absolute"), "{err}");
        let missing = root.path().join("gone.conf");
        let err = check_credentials(
            &request(
                Verb::Mount,
                "cifs",
                "//srv/s",
                &point,
                &format!("credentials={}", missing.display()),
            ),
            uid(),
        )
        .unwrap_err();
        assert!(!err.contains("hunter2"), "{err}");
    }

    #[test]
    fn a_smb_mount_needs_guest_or_a_password_file() {
        let point = Path::new("/m");
        check_password_channel(&request(Verb::Mount, "cifs", "//srv/s", point, "guest"))
            .expect("guest");
        let err = check_password_channel(&request(
            Verb::Mount,
            "cifs",
            "//srv/s",
            point,
            "username=alice,uid=1000",
        ))
        .unwrap_err();
        assert!(err.contains("password file"), "{err}");
        check_password_channel(&request(
            Verb::Mount,
            "cifs",
            "//srv/s",
            point,
            "username=alice,credentials=/run/x.conf",
        ))
        .expect("has a file");
        // NFS has no password at all, so it is never asked for one.
        check_password_channel(&request(Verb::Mount, "nfs", "srv:/e", point, "ro")).expect("nfs");
    }

    #[test]
    fn the_argv_never_starts_with_a_dash_from_our_side() {
        let (root, point) = mount_point();
        let options = "uid=1000,dir_mode=0700,file_mode=0600,noatime";
        let up = request(Verb::Mount, "cifs", "//srv/Users", &point, options);
        let built = argv(&up);
        assert_eq!(built[0], "mount");
        assert_eq!(built[1], "-t");
        assert_eq!(built[2], "cifs");
        assert_eq!(built[3], "-o");
        assert_eq!(built[4], options);
        assert_eq!(built[5], "//srv/Users");
        assert_eq!(built[6], point.display().to_string());
        // Only the two flags we build ourselves begin with `-`.
        assert_eq!(built.iter().filter(|a| a.starts_with('-')).count(), 2);
        let down = request(Verb::Unmount, "nfs", "srv:/e", &point, "");
        // An unmount carries the target and nothing else: no type, no options,
        // because `umount` takes a mount point and the kernel knows the rest.
        assert_eq!(
            argv(&down),
            vec![String::from("umount"), point.display().to_string()]
        );
        let _ = root;
    }

    #[test]
    fn an_empty_option_list_is_not_an_argument() {
        let point = Path::new("/m");
        let r = request(Verb::Mount, "nfs", "srv:/e", point, "");
        let built = argv(&r);
        assert!(!built.contains(&"-o".to_string()), "{built:?}");
        assert_eq!(built, vec!["mount", "-t", "nfs", "srv:/e", "/m"]);
    }

    #[test]
    fn unmounting_something_not_mounted_is_success_not_an_error() {
        // The one path in `run` that can be exercised without root: a fresh
        // directory is not a mount point, so the answer has to be "that is
        // already the state you wanted" rather than a failed `umount`.
        let (_, point) = mount_point();
        let r = request(Verb::Unmount, "nfs", "srv:/e", &point, "");
        assert_eq!(run(&r), 0);
    }

    #[test]
    fn the_owner_comes_from_pkexec_and_is_never_root() {
        assert_eq!(owner_from(1000, None).expect("unprivileged"), 1000);
        assert_eq!(
            owner_from(1000, Some("0")).expect("unprivileged wins"),
            1000
        );
        assert_eq!(owner_from(0, Some("1000")).expect("pkexec"), 1000);
        assert_eq!(owner_from(0, Some(" 42 ")).expect("padded"), 42);
        let err = owner_from(0, None).unwrap_err();
        assert!(err.contains(PKEXEC_UID), "{err}");
        for bad in ["", "root", "-1", "1000x", "99999999999999"] {
            let err = owner_from(0, Some(bad)).unwrap_err();
            assert!(err.contains(PKEXEC_UID), "{bad}: {err}");
        }
        let err = owner_from(0, Some("0")).unwrap_err();
        assert!(err.contains("not for root"), "{err}");
    }

    #[test]
    fn a_program_is_found_without_consulting_path() {
        // Whatever $PATH says, the helper only trusts the fixed directories.
        assert!(program("sh").is_some() || program("true").is_some());
        assert!(program("definitely-not-a-program-here").is_none());
        assert!(program("").is_none());
        // A directory is not a program even when it is named like one.
        assert!(program("usr").is_none());
    }

    #[test]
    fn every_rejection_is_one_line_that_names_the_problem() {
        let (_, point) = mount_point();
        let good = request(Verb::Mount, "nfs", "srv:/e", &point, "");
        let cases: Vec<Vec<String>> = vec![
            vec!["mount".into(), "--type".into(), "9p".into()],
            vec![
                "mount".into(),
                "--type".into(),
                "cifs".into(),
                "--source".into(),
                "srv/share".into(),
                "--target".into(),
                point.display().to_string(),
                "--options".into(),
                "guest".into(),
            ],
            vec![
                "mount".into(),
                "--type".into(),
                "nfs".into(),
                "--source".into(),
                "srv:/e".into(),
                "--target".into(),
                "relative".into(),
                "--options".into(),
                String::new(),
            ],
        ];
        for argv in cases {
            let err = parse(&argv).and_then(|r| check(&r, uid())).unwrap_err();
            assert!(!err.is_empty());
            assert_eq!(err.lines().count(), 1, "{err}");
            assert!(!err.contains('\n'), "{err}");
        }
        let _ = good;
    }

    fn uid() -> u32 {
        ufm_core::paths::uid()
    }

    #[test]
    fn the_usage_text_is_the_shape_the_planner_emits() {
        assert!(USAGE.contains("--type"));
        assert!(USAGE.contains("--source"));
        assert!(USAGE.contains("--target"));
        assert!(USAGE.contains("--options"));
        assert!(USAGE.starts_with("usage:"));
    }

    #[test]
    fn device_requests_reject_wrong_shape_and_non_devices() {
        let bad = vec!["device-mount".into(), "--device".into(), "/dev/null".into()];
        assert!(device_dispatch(&bad).is_err());
        let bad = vec![
            "device-mount".into(),
            "--device".into(),
            "/tmp".into(),
            "--target".into(),
            format!("/run/user/{}/ufm/devices/fake", uid()),
        ];
        assert!(
            device_dispatch(&bad)
                .unwrap_err()
                .contains("not a block device")
        );
    }
}
