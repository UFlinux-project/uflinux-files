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
And `lftp mount` is not a program that mounts and exits: the process *is* the
mount, and killing it drops the share. `Backend::serves()` reports that, and the
caller then uses `Command::serve()` — spawn and keep the child, do not wait for
it — instead of `status()`. Whether the mount exists is decided by re-reading
mountinfo either way, never by an exit code, so the two paths agree on what
"connected" means.
And there is no kernel FTP client, so FTP is always FUSE.

## Mount points

Shares are mounted under `$XDG_RUNTIME_DIR/ufm/<slug>`, falling back to
`/tmp/ufm-$UID` when there is no runtime directory. The slug is derived from the
URI, so the same share always gets the same directory and two mounts of one
server never collide.

A per-user runtime directory means the mounts are gone when the session is,
which turns a stale mount into something that heals itself at boot rather than
a surprise at login. It also means an application that cannot see `/run/user`
needs a path it can, so the file manager maintains a `Network` folder of symlinks
into the mount root, and that is the folder a "connect to a share" flow points
other programs at.

The folder is `$XDG_DATA_HOME/uflinux/filemanager/Network`
(`~/.local/share/…` by default), overridable with
`UFLINUX_FILES_NETWORK_DIR` for tests. Mounting a share puts one symlink in it,
named after the share's label and pointing at the mount point; ejecting removes
that symlink and nothing else. A link the user made by hand with the same name,
pointing somewhere else, is left alone — the folder is shared with whatever else
the desktop puts there.

GTK4 is not what surfaces this folder. GtkPlacesView builds its network section
from the GIO volume monitor, and there is no location in the library that it
scans for network entries, so the file manager adds its own rows for the shares
in its own table. The `Network` folder exists for *other* applications — a file
chooser in an editor, a terminal, an uploader — which is the reason for mounting
in the first place.

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

`MountState` has three cases: `Mounted`, `Stale` (mountinfo has an entry but
nothing answers there — a FUSE daemon that died leaves exactly this) and
`Absent`. A stale mount has to be detached before it can be remounted, with
`fusermount -u` for FUSE and the helper for the two kernel filesystems, and the
UI says so rather than retrying a mount that would fail the same way.

The state is also what a share's sidebar row is allowed to offer, so the three
cases are a menu rather than only a label: `Mounted` offers Disconnect, `Stale`
offers Disconnect and Forget, `Absent` offers Reconnect — when its password is
already on disk — and Forget. Every state also offers the one row that is a
function of the record rather than of the kernel, described under *Connecting at
login* below. See *What a mount row offers* in `docs/architecture.md`.

Reconnect reuses the stored record and the stored password and takes no new
input, which is why the row is only offered when the answer is already known;
`Runner::can_reconnect` is that question. A failure is recorded in the record's
`last_error` and shown in the window that asked, so a share that cannot come
back says why beside its own row instead of looking like a card that ignores
being clicked.

Forget drops the record without detaching, and refuses a share the kernel still
reports `Mounted` — the record is the only handle on a live mount, and losing it
strands a mount point no window can eject. A `Stale` share is dropped without a
detach and left to the next sweep.

## Cleaning up: `--sweep-mounts`

Run at session start, before the window appears. It reads the table, reads
mountinfo, and looks at the mount root and the credential directory. For each
discrepancy it records an action and the evidence for it:

- a mount in the table that the kernel does not agree with → detach it
  (`Absent` records are left alone: reconnecting is a user action, not a
  cleanup, and a share listed but not yet connected is a row rather than a
  leftover)
- a directory under the mount root with no entry in the table → remove it if empty
- a `.conf` file with no matching mount → remove it, since it may be a password
- a mount idle past `--idle-after` → detach it

`apply_files` performs the file removals itself; the detaches are plans the
caller runs, so a sweep containing no detaches never touches polkit at all. The
notes list collects what could not be classified and is reported, never fatal: a
sweep that stops halfway leaves the rest to rot, and a leftover password file is
exactly the thing that should not rot.

A sweep keeps two lists, because "what is left to do" and "what this run did" are
not the same question. `items()` is the pending detaches; `removed()` is what has
already been taken care of, whether `apply_files` deleted it or a detach the
caller ran succeeded. Printing only the pending list would report a successful
sweep as one that did nothing, which is the opposite of what a session-start
cleanup needs to say. `--json` reports both, as `done`, `pending` and `notes`.

Idle auto-unmount uses the same mechanism with `--idle-after` set, on a timer
started when the last window closes; the file manager's own limit is 900 seconds
(`runner::IDLE_AFTER`). A share marked connect-at-login is exempt, since it is
meant to be there. The idle clock is the mount record's last use, updated when a
listing is read through it, not the mount point's atime — which a `noatime` NFS
mount does not maintain at all.

## Connecting at login

The `autostart` flag on a record is the whole feature: a share's menu row flips
it, `mounts.json` stores it, the idle sweep spares shares that carry it, and
`--connect-at-login` acts on it. There is no second file and no second daemon —
the table already knows which shares exist, and whatever runs this command is one
short-lived process rather than something to keep resident.

What runs it is not this package's business, and the reason is worth recording
here because it rules the obvious answer out. UFlinux 3 is OpenRC, greetd and
labwc: there is no systemd in the image at all as init or userspace — the only
package with that name is `systemd-boot`, the EFI bootloader — so there is no
user unit to drop in and `--connect-at-login` is not a `.service` waiting to be
enabled. The hook that does exist is `/usr/libexec/uflinux-autostart`, the shell
script the distro's own `uflinux` package runs once a labwc session is up; it is
the file that launches `uflinux-desktop --supervise` and the rest of the session,
and it is the file a line calling this command would go into. That file is the
distro's to ship, so this project ships the command and the answers, and not the
trigger.

The row is a label that says the opposite of the current answer ("Connect at
Login" on a share that does not, "Do Not Connect at Login" on one that does)
and an action that toggles, rather than a check button carrying a boolean. That
keeps `panel::MenuRow` the same `{label, action, target}` triple it is for the
other three rows, and the label is itself the feedback about what state the
press found. `Runner::set_autostart` reports whether the answer *changed*, so the
banner can distinguish "now it will" from "it already did".

`--connect-at-login` is a separate mode from `--sweep-mounts`, and the two
refuse to be combined in one invocation, because they do opposite things: one
detaches what is dead, the other mounts what is wanted. Run sweep first and
connect second and the second run finds clean mount points. It is a separate
mode rather than a flag on sweep for the same reason — a session starter should
not be able to ask for both at once and get an order nobody specified.

The run asks nobody anything, since it happens before a window exists. Each
marked share gets one of four answers, and two of them are not failures:

- `mounted` — the share was absent, nothing was needed to try, and the mount
  worked
- `already` — the share was already connected, so nothing was run. Two windows,
  or a session starter and a late manual run, will both ask; the second should be
  a no-op and not a "device or resource busy" to interpret
- `skipped` — nothing was attempted: the mount point is `Stale` (mounting over a
  dead endpoint cannot work, and detaching it is sweep's job, which is why the
  runs are separate), or the protocol takes a password that was never remembered
  (`Runner::can_reconnect`, the same question the Reconnect row asks)
- `failed` — the command ran and the kernel did not confirm a mount; the
  backend's reason is recorded in `last_error` beside the record, as it is for
  any other failure

`Login::summary` prints `verb<TAB>id` with the reason indented under each — the
shape `Sweep::summary` uses, so one session log reads as one format — and a run
with nothing marked says `nothing marked connect at login` rather than printing
nothing. `--json` reports `attempts` and `not_mounted`. The exit code is 0 for
any run that got far enough to answer: a server that is not up yet is an
ordinary morning, and the reason is beside the record for the Network group to
show. A share whose server was down is one click from Reconnect in a window; the
session that ran this command has nothing it could do differently.
