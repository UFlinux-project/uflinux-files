# Validation

What is checked, by what, and what each check does **not** prove. A green suite that
certifies less than it appears to is worse than a smaller suite with honest names.

## What runs on any host

```sh
cargo test
cargo clippy --all-targets
```

Both are green today with no GTK4 development packages installed, because the seven
library crates are pure Rust and the two binaries are not default members of the
workspace. That is the whole reason for the split: the logic that decides what a user
sees is testable on a machine that cannot build the UI.

`--all-targets` and not `--workspace`: `-p uflinux-files` pulls in gtk4-sys, gsk4-sys
and graphene-sys, which need `pkg-config` to find GTK4. Packaging builds
`--workspace` inside the Alpine chroot where those exist.

Clippy runs at `pedantic` level with `-D warnings` in CI. The allows are narrow and
each one is a decision, not a convenience: `cast_possible_truncation` and friends for
sizes that are bounded by construction, `missing_errors_doc` because a `Result` whose
error type is named in the signature does not need an essay per method.

### What the unit tests actually cover

| Area | The property under test |
| --- | --- |
| `ufm-core` | `Location` round-trips, including a `!` inside an archive path and a `..` that is a legal Linux component; traversal out of an archive is rejected; atomic writes leave no partial file; config migration from an older `version`. |
| `ufm-vfs` | streaming reads publish in bounded slices; the cache respects all three caps; a cancelled read stops within one chunk; the watcher's overflow path reports the directories it dropped. |
| `ufm-archive` | `.tar.gz` is a tar and `.gz` alone is a one-member container; a corrupt or truncated archive degrades to an error row rather than a panic; a `../` member cannot escape the extract directory; a zip-in-tar navigates two levels. |
| `ufm-mount` | the mount table survives a kill mid-write; a `Share` renders for display without its password; every backend's argv is checked for the absence of the secret; `SecretUse` matches what the real program expects. |
| `ufm-index` | a commit that returned is visible after reopening the index; `hidden` filtering works with scoring on; the n-gram field finds a prefix the tokenised field cannot; a re-crawl replaces rather than accumulates. |
| `ufm-search` | unknown fields are rejected; an oversized reply shrinks to snippets then page and says which; `Exec` expansion drops a field code with no value; a `NAME=value` prefix is not treated as an argument. |
| `ufm-desktop-ipc` | an id with a slash cannot name a file outside the desktop directory; unplugging a monitor moves icons and deletes none; a saved layout meets a directory that changed since. |

## Archives

`tests/archive-fixtures/` holds a zip, `tar.gz`, `tar.xz`, `7z`, a zip inside a tar,
and names with unicode, spaces and a `!` in them. Each is browsed, one member is
extracted, and a truncated copy of each must produce an error row.

The truncated copies matter more than the good ones: an archive read is the only place
in this program where the input is both untrusted and structurally parsed, and a panic
there loses the user's window, not just the file.

## Mounts

`tests/mount-fixture.sh` cannot be a unit test, because the thing being tested is a
process boundary. It asserts:

- the mount table matches what was attempted, including after a failure;
- unmounting leaves no stale entry and no leftover directory;
- **no credential ever appears in `/proc/*/cmdline`** for any child spawned;
- a stale mountpoint is swept by `--sweep-mounts` and a live one is not.

What this does **not** certify: that a real SMB server accepts the options, or that a
real `davfs2` setuid helper works with the group and config on the target image. Those
are open items in the plan and need the installed image, not a fixture. `sshfs` is
tested against a real `sshd` on localhost, which proves the plumbing and not the
server.

## UI

`tests/files-ui-smoke.sh` runs in a private headless labwc session with
`UFLINUX_FILES_BIN` set: open a window, switch all three views, navigate three column
levels, run a search, open the Computer view. Assertions are on the accessibility tree
plus a painted-frame probe.

The frame probe is an upper bound and should be read as one — the same caveat the
distro's own desktop tests carry. It proves the window painted something within the
budget; it does not certify that the pixels are right, and it says nothing about a
physical GPU at 4K.

## Memory and latency

`tests/files-memory-bench.py` samples `/proc/<pid>/smaps_rollup` for six scenarios and
`scripts/files-memory-gate.py` fails the build over the budgets in
[memory-budget.md](memory-budget.md). See that file for what a report must contain and
why retained PSS and descriptor counts are checked across 100 cycles rather than
sampled once.

## Packaging

In a fresh root, `abuild` then `apk`:

- the package signs and verifies; ELF types and PIE as the distro requires;
- every library resolves — this is where a `recommends` that should have been a
  `depends` shows up;
- all three services D-Bus-activate: `org.uflinux.Files`, `org.uflinux.Search1`,
  `org.uflinux.FileManager.Desktop1`. Activation is tested rather than assumed,
  because a `.service` file whose `Exec` path is wrong produces a desktop with no
  icons and no error anywhere;
- no file-ownership conflict with any other package in the set.

`scripts/files-source.py --check` runs first: it rebuilds the source bundle and
compares bytes, so a tarball whose digest drifted from the recipe fails before
anything is compiled. The bundle is reproducible by construction — no compressor, mode
0644, sorted entries, zeroed mtime and owner — which is what makes that check mean
something rather than being a coin flip.

## Manual pass, still required

A 512 MiB no-swap VM: three windows, column view, a search running, two network
mounts, 200 desktop icons, watching `smem`. Some failures only appear in that
combination — a thumbnail LRU that is fine alone and not with three windows, or a
crawl that starves the UI thread — and none of them are things a per-process budget
catches.

## Not covered, on purpose

- Physical HDR, VRR and GPU behaviour: separate hardware validation.
- Wireless and DHCP: ConnMan owns those, and the file manager has no opinion about
  them beyond mounting what is already mounted.
- Copy/move conflict resolution across devices, and the properties dialog: they do not
  exist yet, so there is nothing to certify. Their tests land with them.
