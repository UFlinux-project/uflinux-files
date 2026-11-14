# Validation

What is checked, by what, and what each check does **not** prove. A green suite that
certifies less than it appears to is worse than a smaller suite with honest names.

## What runs on any host

```sh
cargo test
cargo clippy --all-targets
```

Both are green today with no GTK4 development packages installed. Every default
member is pure Rust — the seven library crates and the mount helper — and the three
binaries that need GLib are not default members. That is the whole reason for
the split: the logic that decides what a user sees is testable on a machine that
cannot build the UI, and so is the code that runs as root.

`--all-targets` and not `--workspace`: `-p uflinux-files` pulls in gtk4-sys, gsk4-sys
and graphene-sys, which need `pkg-config` to find GTK4, and `-p uflinux-files-provider`
needs `glib-dev`. Packaging builds `--workspace` inside the Alpine chroot where those
exist.

Clippy runs at `pedantic` level with `-D warnings` in CI. The allows are narrow and
each one is a decision, not a convenience: `cast_possible_truncation` and friends for
sizes that are bounded by construction, `missing_errors_doc` because a `Result` whose
error type is named in the signature does not need an essay per method.

### What the unit tests actually cover

| Area | The property under test |
| --- | --- |
| `ufm-core` | `Location` round-trips, including a `!` inside an archive path and a `..` that is a legal Linux component; traversal out of an archive is rejected; atomic writes leave no partial file; config migration from an older `version`. |
| `ufm-vfs` | streaming reads publish in bounded slices; the cache respects all three caps; a cancelled read stops within one chunk; the watcher's overflow path reports the directories it dropped. |
| `ufm-archive` | `.tar.gz` is a tar and `.gz` alone is a one-member container; a corrupt archive degrades to an error row rather than a panic, and a truncated one never lists a member it cannot read whole; a `../` member cannot escape the extract directory; a zip-in-tar navigates two levels. |
| `ufm-mount` | the mount table survives a kill mid-write; a `Share` renders for display without its password; every backend's argv is checked for the absence of the secret; `SecretUse` matches what the real program expects. |
| `ufm-index` | a commit that returned is visible after reopening the index; `hidden` filtering works with scoring on; the n-gram field finds a prefix the tokenised field cannot; a re-crawl replaces rather than accumulates. |
| `ufm-search` | unknown fields are rejected; an oversized reply shrinks to snippets then page and says which; `Exec` expansion drops a field code with no value; a `NAME=value` prefix is not treated as an argument. |
| `ufm-desktop-ipc` | an id with a slash cannot name a file outside the desktop directory; unplugging a monitor moves icons and deletes none; a saved layout meets a directory that changed since. |
| `uflinux-files-mount` | the argv handed to `mount(8)` contains no argument from the request that begins with `-`; a target that is a symlink, relative, world-readable, or somebody else's is refused; a `credentials=` file anyone can read is refused and its contents never reach an error message; a `pass=` option is refused; the owner comes from `PKEXEC_UID` and is never `0`. |
| `uflinux-files` | the location on argv reaches the window; `--new-window` gives up being the single instance; the application id matches the desktop entry; a machine with no render node is asked for the cairo renderer, and an explicit `GSK_RENDERER` is never overridden; a paste plan drops the rows that cannot land and names the reason for each, growing `report (2).pdf` rather than replacing; copying a folder takes its contents and deleting one takes what is inside it; a prompt refuses an empty name, hands the typed name over exactly once, and pre-selects the stem of a filename but not its extension. |
| `uflinux-files-provider` | the crate does not depend on GTK, checked against its own manifest; the bus name is taken through the C entry point whose handlers are nullable, not the gio-rs wrapper that aborts at logout; the polling deadline is longer than the timer that checks it; a missing login session is refused before the bus is asked for. |

## Archives

`tests/archive-fixtures/` holds a zip, `tar.gz`, `tar.xz`, `7z`, a zip inside a tar,
and names with unicode, spaces and a `!` in them. Each is browsed and one member is
extracted byte for byte, and each is then read back with its tail removed.

What a truncated container has to do depends on how it is indexed, and the answer is
not this program's to invent — `unzip`, Python's `zipfile` and GNU `tar` behave the
same way and the tests assert that behaviour rather than a hoped-for one. A compressed
stream must be decoded from its first byte, so a `.tar.gz` or `.tar.xz` missing its
tail cannot be listed at all and comes back as an error row. A zip keeps its index in
a central directory at the end and a plain tar in header blocks as it goes, so the
surviving head of either is still a well-formed container and `unzip -l` and `tar tvf`
both list it without complaint; the promise there is instead that no member is offered
whose own bytes did not survive, and that nothing reads short while claiming success.
One quirk is pinned rather than hidden: `sample.zip` stores `outer.zip` uncompressed
near the front, so a copy missing its tail still contains a complete end-of-central-
directory record — the inner archive's — and every tool that reads zips, this one
included, then reads the half-file as the inner container.

The truncated copies matter more than the good ones: an archive read is the only place
in this program where the input is both untrusted and structurally parsed, and a panic
there loses the user's window, not just the file.

## Mounts

`tests/mount-fixture.sh` cannot be a unit test, because the thing being tested is a
process boundary. It runs against the built binary and the built helper, and skips
rather than fails wherever it cannot mount, so it needs a container that can:

```sh
podman run --rm --cap-add=CAP_SYS_ADMIN --device /dev/fuse \
    -v "$PWD":/src:z -w /src -e CARGO_TARGET_DIR=/tmp/t IMAGE \
    sh -c 'cargo build -p uflinux-files -p uflinux-files-mount &&
           sh tests/mount-fixture.sh /tmp/t/debug/uflinux-files \
                                     /tmp/t/debug/uflinux-files-mount'
```

The helper resolves `mount` and `umount` from a fixed list of directories and ignores
`$PATH`, which is the point of it and also why the fixture installs a shim over
`/usr/bin/mount` rather than putting one earlier in the path. The shim records the argv
it was handed and then mounts `tmpfs` whatever it was asked for: what is being certified
is the helper's checks and the exact words it passes on, not that a server answers. The
fixture asserts:

- the argv handed to `mount(8)` is the one the plan described, word for word and in
  position, and a mount the helper reports really appears in `/proc/self/mountinfo`;
- a failing mount exits 1 and leaves an empty directory and no kernel entry;
- **no credential ever appears in `/proc/*/cmdline`** for any child spawned — scanned
  from a loop while the mount child is alive, not after it exits, because the exposure
  being ruled out is the window in which the child runs;
- `pass=`, a group-readable password file, a symlink mount point, and a mount point
  belonging to somebody else are each refused before anything runs, and mounting over a
  live mount says so instead of hiding the old one;
- unmounting detaches, and doing it again is a no-op rather than an error;
- a stale mountpoint is swept by `--sweep-mounts` and a live one is not, that the
  report's claims all actually happened, that the table on disk then matches what is
  mounted, that a non-empty orphan is reported but kept, and that `--idle-after` detaches
  an idle FUSE share with no privilege escalation and takes its mount point with it.

The stale mount is real: `sshfs` against a local `sshd`, then `kill -9` on the daemon,
which leaves the kernel entry and makes the mount point answer `ENOTCONN`. That is the
state a sweep exists to notice and the reason this file cannot be faked with a synthetic
mount table.

What this does **not** certify: that a real SMB server accepts the options, or that a
real `davfs2` setuid helper works with the group and config on the target image. Those
are open items in the plan and need the installed image, not a fixture. `sshfs` is
tested against a real `sshd` on localhost, which proves the plumbing and not the
server. The privileged detach path goes through the helper directly rather than through
`pkexec`, because `pkexec` does not run in the test container; the unit tests cover the
argv `pkexec` would be handed.

## The desktop-icons bus

`tests/provider-bus.sh` is the part of the desktop feature a unit test cannot reach: the
provider's whole job is to answer D-Bus calls, and the unit tests drive the dispatch with
a core that has no connection at all. Everything up to the wire is tested there; what the
wire itself does — the introspection data being accepted by a real client, the interface
name being claimable, a refusal arriving as a real D-Bus error, the name being released
on `SIGTERM` — is only visible with a bus running.

```sh
cargo build -p uflinux-files-provider
tests/provider-bus.sh target/debug/uflinux-files-provider
```

With no session bus around — which is the case in a build chroot or a container — it
starts a private one for itself, so it needs `dbus-run-session` and `gdbus` either way.
It exits non-zero on the first answer that is not what the protocol says, so it chains
onto the test run rather than being read by a person.

## UI

`tests/files-ui-smoke.sh` brings up its own headless `labwc` (headless backend, pixman
renderer — no GPU, no X) and its own session bus, and runs the real binary under it. The
binary is the first argument; if a `uflinux-searchd` sits beside it, or is named in
`UFLINUX_SEARCHD_BIN`, the indexer is exercised too. Anything missing — `labwc`, `grim`,
`gdbus`, the AT-SPI typelib — makes it print a `SKIP` line and exit 0, so a build host
with no compositor is not read as a broken program.

The assertions are on the accessibility tree, read out-of-process by
`tests/files-ui-a11y.py` (an in-process AT-SPI walk deadlocks against the bus the walked
process is itself serving). The tree is what a screen reader sees, so it is the closest
thing the interface has to a public contract. The checks, in order: a window opens on the
location named on the command line; the sidebar offers every place; the header buttons
carry the right sensitivity for the start of a history; the grid lists the folder's
entries and counts them; the list mode swaps in a tree table with the four headings; the
columns mode renders one pane per path level; `Describe` reports `paste` disabled while
the clipboard is empty; the `new-folder` action opens a prompt, its button creates the
folder the prompt suggested, the listing picks it up without being told to, and answering
the prompt closes it; the search action opens a field and the field takes typed text; a
second invocation opens a `Computer` window in the running instance; `--new-window` opens
a window rather than focusing an open one; and the indexer answers `Status` on the bus.
The prompt is driven through the accessibility tree, by the `press` command of the probe,
which activates a node by name — the same controller path a mouse click takes.

The run fails if the app printed a GLib critical. One message is excepted, and it is
excepted on evidence rather than convenience: `GTK4` 4.22 re-reads a toplevel's
accessibility properties while a window is being torn down, and with an AT-SPI client
attached it sometimes reaches a surface that is already gone, printing
`gdk_wayland_toplevel_set_a11y_properties: assertion 'GDK_IS_WAYLAND_TOPLEVEL
(toplevel)' failed`. It reproduces with the connect dialog, which predates the name
prompts, and it does not appear at all without a tree reader attached, so it is a race
inside `GTK`'s teardown and not a report about this program. The count of what is left
over is printed either way.

The painted-frame probe is an upper bound and should be read as one — the same caveat the
distro's own desktop tests carry. `grim` captures the frame and the test counts distinct
colours in it: an empty desktop is one flat colour, a rendered folder is thousands. It
proves the window painted something with variety in it; it does not certify that the
pixels are right, and it says nothing about a physical GPU at 4K.

## Memory and latency

`tests/files-memory-bench.py` samples `/proc/<pid>/smaps_rollup` for six scenarios and
`scripts/files-memory-gate.py` fails the build over the budgets in
[memory-budget.md](memory-budget.md). See that file for what a report must contain and
why retained PSS and descriptor counts are checked across 100 cycles rather than
sampled once.

```sh
cargo build --release -p uflinux-files -p uflinux-files-provider -p uflinux-searchd
tests/files-memory-bench.py \
    --binary target/release/uflinux-files \
    --provider-binary target/release/uflinux-files-provider \
    --searchd target/release/uflinux-searchd \
    --output /tmp/report.json
scripts/files-memory-gate.py --report /tmp/report.json
```

Three things about reading the result. It has to be a **release** build; `--quick` shrinks
the fixtures to check the plumbing and is not a measurement; and a scenario the bench
could not run is reported as skipped and the gate prints `not measured` rather than
counting it as a pass. The four window scenarios need a compositor — run the bench inside
a Wayland session, or let it start a private bus and a headless `labwc` the way
`files-ui-smoke.sh` does — so on a host without either, the two rows that matter most go
unevaluated rather than wrong.

The renderer is recorded in the report and printed as a note by the gate, because it is
most of what a window costs: 222 MiB for Mesa's software GL against 54 MiB for cairo on
the same machine, same window, same folder.

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
