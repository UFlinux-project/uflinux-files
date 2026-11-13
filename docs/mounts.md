# Network filesystems

UFLinux Files does not implement SMB, NFS, FTP, WebDAV or SFTP. It mounts them
and then browses the result with the ordinary local code path. That decision
shapes everything below, and the reason is not laziness: a mounted share is
readable by every application in the session, including the ones with no concept
of a remote location, and a file manager with one browsing path has one set of
bugs.

## The five protocols

| Protocol | How | As | Package |
| --- | --- | --- | --- |
| SMB | kernel `cifs`, through the helper | root | `cifs-utils` |
| NFS | kernel `nfs`, through the helper | root | `nfs-utils` |
| SFTP | `sshfs` | user | `sshfs-fuse` |
| WebDAV | `rclone mount`, else `mount.davfs` | user | `rclone` / `davfs2` |
| FTP | `rclone mount`, else `lftp mount` | user | `rclone` / `lftp` |

The preference is always for the backend that needs no privilege, so FUSE comes
first wherever it exists. `mount(2)` is the only reason root is involved at all,
which means two of the five protocols and one small helper binary.

`rclone` is listed first for WebDAV and FTP because one program covers both and
handles TLS, redirects and retries. It is also the least certain entry in the
table: its presence in the target Alpine release has to be confirmed at build
time. Where it is absent, `mount.davfs` and `lftp mount` take over, and the code
path for each is already written and tested rather than a plan.

Two consequences of using `lftp mount` for FTP are worth stating plainly. There
is no read-only option, so `?ro` is recorded in the mount table and the UI
refuses writes rather than pretending the kernel enforces something it does not.
And there is no kernel FTP client, so FTP is always FUSE.

## Mount points

Shares are mounted under `$XDG_RUNTIME_DIR/ufm/<slug>`, falling back to
`/tmp/ufm-$UID` when there is no runtime directory. The slug is derived from the
URI, so the same share always gets the same directory and two mounts of one
server never collide.

A per-user runtime directory means the mounts are gone when the session is,
which turns a stale mount into something that heals itself at login rather than
a surprise at boot. It also means an application that cannot see `/run/user`
needs a path it can, so the file manager maintains a `Network` folder of symlinks
into the mount root, and that is the folder a "connect to a share" flow points
other programs at.

## Credentials

**A password is never an argument.** `/proc/<pid>/cmdline` is world-readable, so
`mount -o pass=hunter2` leaks a share password to every user on the machine and
to anything that snapshots process lists. Each backend has a different way to
avoid this, and `SecretUse` names which one a plan uses:

| `SecretUse` | Backend | Where the password goes |
| --- | --- | --- |
| `None` | NFS, SFTP with a key, guest SMB | nowhere; no password exists |
| `File` | SMB, davfs2, lftp, rclone | a `0600` file, or an environment variable |
| `Stdin` | nothing today | one line on stdin, then EOF |

The last row exists because a helper could ask for a password that way and
`write_secret` is ready for it; no backend uses it, because a mount has to outlive
the process that created it and re-read its password after a reconnect, which a
closed pipe cannot do. That is also why the privileged helper reads SMB's password
from the `credentials=` file rather than from the stdin `mount.cifs` would accept.

The files are `<config>/mounts/<slug>.conf`, written at `0600` before the plan
runs and removed after, with the sweep cleaning up whatever a crash left behind.
Each format is what the helper itself wants: `username=`/`password=`/`domain=`
lines for `mount.cifs`, a `machine … login … password …` line for lftp's netrc,
one `<url> <user> <password>` line appended to `~/.davfs2/secrets` for davfs2,
which reads only its own file. rclone is the odd one: the generated config file
carries everything except the password, which arrives in
`RCLONE_CONFIG_UFM_PASS`. Environment is not world-readable the way argv is.

`Secret` scrubs its contents on drop and prints `****`, so a password cannot
reach a log by accident. Saved credentials live in
`<config>/credentials.json` at `0600`. The freedesktop Secret Service is a
possible later backend, not a dependency.

## Privilege, and why not udisks2

UFLinux has no udisks2. The alternative precedent in the distro is the
`org.uflinux.Settings.System1` D-Bus helper, whose rule is "no shell commands and
no arbitrary paths". The mount helper follows the same rule.

`data/org.uflinux.files.mount.policy` registers one action,
`org.uflinux.files.mount`, annotated with
`org.freedesktop.policykit.exec.path` pointing at
`/usr/libexec/uflinux-files-mount`. That annotation is what makes pkexec run
*that* binary rather than whatever the caller names, so the action cannot be
reused to run something else as root. One action covers mount and unmount:
splitting them means a user allowed to attach a share can be refused when they
try to leave, stranding a mountpoint until the next sweep.

`data/50-uflinux-files.rules` narrows the action to the owner of a local seat.
`allow_active` alone would nearly do it, but under greetd + cage whether the one
session counts as *active* depends on how logind marked it, and a session that is
local but not flagged active would get a prompt from an agent that belongs to
the session being questioned — a failure that looks like a broken file manager
rather than a polkit problem. Everything else falls through to `auth_admin`.

Authorising the action grants no capability that FUSE does not already give the
same user: they can already mount a share unprivileged with `sshfs` or `davfs2`.
The action exists only so the two kernel filesystems can reach `mount(2)`.

The helper's argv is fixed in shape and shared with the planner
(`ufm_mount::plan::helper_argv`), so the two cannot drift:

    uflinux-files-mount mount   --type cifs --source //srv/share --target DIR --options uid=1000,…
    uflinux-files-mount unmount --type nfs  --source srv:/export --target DIR --options

The helper validates the filesystem type against a short list — `cifs` and `nfs`,
nothing else, since those are the two that need `mount(2)` — and never runs a
shell. Every argument it passes on is built from its own literals, so the only
arguments beginning with `-` are `-t` and `-o`; a source or an option list that
starts with a dash is refused rather than re-read by `mount(8)` as a flag.

The target has to be absolute, contain no `.` or `..`, not be a symlink, be a
directory, be owned by the caller and be `0700`, and for a mount be empty. That
stands in for "inside the mount root", which the helper cannot check: `pkexec`
replaces the environment, so `$XDG_RUNTIME_DIR` — where the root is derived from
— is not available to a root-run program. Ownership and mode say the same thing
more directly: a directory somebody else owns, or one group can read, was not
made by this file manager for this mount. The file manager creates the target at
`0700` before asking, because creating it as root inside a location the user
controls invites a race.

`credentials=` is verified before `mount.cifs` sees it: absolute, owned by the
caller, `0600`, and its contents are read once and discarded, so a file pointing
at another user's secrets is refused instead of leaked into a kernel log. Any
`pass=` or `password=` option is refused outright, because argv is readable by
any process with `ps`. That leaves cifs needing either `guest` or a credentials
file, and the helper says so when neither is present.

`mount` and `umount` are looked up in `/usr/bin`, `/bin`, `/usr/sbin` and
`/sbin`, never through `$PATH`: an action with `allow_active=auth_self_keep` can
be answered without a prompt, and a retained authorization plus a caller-controlled
`PATH` is a privilege escalation. The child runs with stdin on `/dev/null` and a
60 second deadline, past which it is killed — a hung NFS mount is the case that
would otherwise hold the window open forever.

The exit code is the whole report: `0` means the requested state is true, which
includes remounting something already mounted and unmounting something already
gone, so a retry after a lost reply is not an error; `1` means `mount` or `umount`
failed, or the child had to be killed; `2` means the request itself was refused,
which is a bug in the caller rather than a problem with the share. The owner the
checks compare against comes from `PKEXEC_UID`, which `pkexec` sets to the uid of
the invoking process — the helper's own euid is always `0`.

`doas` is not an option here even though the distro uses it for administration:
it reads its password from `/dev/tty`, so it only works from a terminal. The
authentication agent is the one `uflinux-desktop` already registers.

## The mount table

`<config>/mounts.json` records intent, not state: the URI, the mount point, the
options, whether to connect at login, when it last carried traffic, and the last
error if it failed. State is read from the kernel instead, by parsing
`/proc/self/mountinfo`, because a table that claims a mount exists when the
kernel has forgotten it is worse than no table.

`MountState` distinguishes `Mounted`, `Stale` (present in mountinfo, unreachable
— a FUSE daemon that died leaves exactly this), `Detached` and `Absent`. A stale
mount needs `fusermount -u` before it can be remounted, and the UI says so rather
than retrying a mount that will fail the same way.

## Cleaning up: `--sweep-mounts`

Run at session start, before the window appears. It reads the table, reads
mountinfo, and looks at the mount root and the credential directory. For each
discrepancy it records an action and the evidence for it:

- a mount in the table that the kernel does not agree with → detach it
- a directory under the mount root with no entry in the table → remove it if empty
- a `.conf` file with no matching mount → remove it, since it may be a password
- a mount idle past `--idle-after` → detach it

`apply_files` performs the file removals itself; the detaches are plans the
caller runs, so a sweep containing no detaches never touches polkit at all. The
notes list collects what could not be classified and is reported, never fatal: a
sweep that stops halfway leaves the rest to rot, and a leftover password file is
exactly the thing that should not rot.

Idle auto-unmount uses the same mechanism with `--idle-after` set, on a timer
started when the last window closes. The idle clock is the mount record's last
use, updated when a listing is read through it, not the mount point's atime —
which a `noatime` NFS mount does not maintain at all.
