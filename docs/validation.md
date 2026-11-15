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
| `ufm-core` | `Location` round-trips, including a `!` inside an archive path and a `..` that is a legal Linux component; traversal out of an archive is rejected; atomic writes leave no partial file; config migration from an older `version`; a mode string renders the file-type letter and all nine permission characters, including the socket and fifo cases a `kind` has to cover when a backend supplies no mode; an absolute timestamp names the moment rather than a relative distance. |
| `ufm-vfs` | streaming reads publish in bounded slices and a cancelled read stops within one chunk; a second read of one folder is served from memory and reports no chunks at all; a changed sort order, hidden setting or filter is a miss rather than the wrong rows; a cancelled read stores nothing, an emptied folder is still a listing; the cache respects all three caps; invalidation reaches a folder, its parents and its children; the watcher's overflow path reports the directories it dropped; a folder total counts everything below, stops within one directory of a cancel, refuses to follow a symlink, says `partial` when a depth or entry cap stops it and when a directory cannot be read, and never loses a byte to overflow. |
| `ufm-archive` | `.tar.gz` is a tar and `.gz` alone is a one-member container; a corrupt archive degrades to an error row rather than a panic, and a truncated one never lists a member it cannot read whole; a `../` member cannot escape the extract directory; a zip-in-tar navigates two levels. |
| `ufm-mount` | the mount table survives a kill mid-write; a `Share` renders for display without its password; every backend's argv is checked for the absence of the secret; `SecretUse` matches what the real program expects. |
| `ufm-index` | a commit that returned is visible after reopening the index; `hidden` filtering works with scoring on; the n-gram field finds a prefix the tokenised field cannot; a re-crawl replaces rather than accumulates. |
| `ufm-search` | unknown fields are rejected; an oversized reply shrinks to snippets then page and says which; `Exec` expansion drops a field code with no value; a `NAME=value` prefix is not treated as an argument. |
| `ufm-desktop-ipc` | an id with a slash cannot name a file outside the desktop directory; unplugging a monitor moves icons and deletes none; a saved layout meets a directory that changed since. |
| `ufm-thumb` | a requested edge is clamped into the band the spec names, so an icon stays in `normal` and only a preview crosses into `large`; the cache key covers only the file's URI, which is what makes the band the only thing telling a 16 px icon and a 512 px preview apart; a stale entry is a miss. |
| `uflinux-files-mount` | the argv handed to `mount(8)` contains no argument from the request that begins with `-`; a target that is a symlink, relative, world-readable, or somebody else's is refused; a `credentials=` file anyone can read is refused and its contents never reach an error message; a `pass=` option is refused; the owner comes from `PKEXEC_UID` and is never `0`. |
| `uflinux-files` | the location on argv reaches the window; `--new-window` gives up being the single instance; the application id matches the desktop entry; a machine with no render node is asked for the cairo renderer, and an explicit `GSK_RENDERER` is never overridden; a paste plan drops the rows that cannot land and names the reason for each, growing `report (2).pdf` rather than replacing; copying a folder takes its contents and deleting one takes what is inside it; a prompt refuses an empty name, hands the typed name over exactly once, and pre-selects the stem of a filename but not its extension; a properties form draws a row only for a fact the entry actually has, spells one file without an `s`, says *at least* for a stopped walk, and holds the size row for a folder so the answer can be delivered into it or dropped when the dialog is gone; the Open With list is deduplicated by desktop entry, drops an entry that asked not to be shown, keeps a fallback when nothing is registered for the type, and names a file whose type nothing owns; a member of an archive is written out under its own name with its own bytes and a second one of the same name does not clobber the first, while a folder inside an archive is refused rather than written as a zero-byte file; a week-old write-out is swept; and a menu over one file offers Open With where a menu over a folder does not; a preview reads only the head of a file, refuses a binary by its `NUL` byte rather than by its extension, says `(empty file)` for an empty one and `(cannot be shown)` for an unreadable one, and takes text from a `JSON`, `XML` or shell script whose registration hides it under `application/`; arriving at columns mode paints the deepest pane from the listing the window already holds, and a read that was already running still overwrites those provisional rows when it answers. |
| `uflinux-files` (job table) | a report never reaches the strip more than once per throttle window, and the first one always does; a job with no total shows a bar with no fraction rather than a fake one; a tally reads `512 KiB of 1.0 MiB` for bytes and `4 of 9` for rows; finishing the only job empties the table and says so exactly once; pulling the brakes reaches every job on the table, including a second one registered after the first; a report naming a job that already finished is dropped rather than resurrected; the strip's text is read back from the widgets rather than from a parallel copy of it, so it cannot disagree with what is drawn. |
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
the clipboard is empty and `cancel` disabled while nothing runs, which is the job strip
keeping its own visibility and the action's sensitivity to agree; the `new-folder` action opens a prompt, its button creates the
folder the prompt suggested, the listing picks it up without being told to, and answering
the prompt closes it; copying a folder and pasting it where a folder of that name already
lives opens a prompt that names both sides, and answering it `Skip` leaves the disk alone
while answering it `Replace` merges the incoming folder into the one already standing
there; `Describe` reports `extract` disabled on an ordinary folder, a
second window opened on a real `.tar` reports it enabled, activating it leaves a folder
beside the container holding the members, and the window that asked for it says so in
its banner; a file created and then deleted by the shell appears and disappears in the
listing on its own; a picture placed in the folder is decoded and lands in the
thumbnail cache, and the `thumbnails` switch reports its state through `Describe`;
`Describe` reports `properties` enabled with exactly one row selected, the dialog it
opens is a window of its own that names the folder the row lives in and prints the mode
bits `stat` gives for it, and its Size row stops saying it is counting and states a
total; the `context-menu` action opens a menu over the selected row whose rows are
exactly the set that selection and that folder allow, each carrying the accelerator of
the command it names, pressing one runs that command and closes the menu, and
navigating away closes an open one; `Describe` reports `open-with` disabled with a
folder as the one selection and enabled with a plain file, the chooser it opens is a
window of its own that lists the program the desktop database registers for that type,
pressing its row really starts that program and hands it the file that was selected,
and its `Close` dismisses it; the `preview` action is enabled for a folder as well as
for a plain file, the popover it opens shows the file's own bytes and the mode bits
`stat` gives for it, the same key closes it again, and a picture preview lands in the
cache's `large` band rather than the `normal` one the row icons share; the search action
opens a field and the field takes typed
text; a
second invocation opens a `Computer` window in the running instance; `--new-window` opens
a window rather than focusing an open one; and the indexer answers `Status` on the bus.
The prompt is driven through the accessibility tree, by the `press` command of the probe,
which activates a node by name — the same controller path a mouse click takes.

The properties checks are the reason the probe's `press` takes an optional frame name. A
folder window's titlebar carries a button labelled `Close`, and a depth-first walk from
the desktop reaches that node before the one inside a dialog titled `Properties`, so an
unscoped press closed the window instead of the dialog and the two search checks after
it then failed for a reason that had nothing to do with search. Passing a frame scopes
the search to that window's subtree. The mode string is read off the fixture with `stat`
rather than hard-coded, because the umask of whoever ran the script decides whether a
folder is `755` or `750` and a check that guessed would fail for the wrong reason; the
folder total is matched on its shape — a size, the word `in`, a count, the word `file` —
for the same kind of reason, since the fixture gains and loses files as the checks above
it run.

The collision checks are the ones that had to be built up rather than read off, for two
reasons worth knowing before adding to them. The first is that a selection has to be made
without a mouse: a row answers `Action` and `Component` but not `Selection`, because
selection belongs to the container, so the probe gained a `select` command that finds a
node by name, works out its index among its parent's children, and calls
`Atspi.Selection.select_child` on the parent `table` or `list`. That drives the
application's real selection rather than a look-alike, which matters because
`Describe copy` reports the action enabled whether or not anything is selected —
sensitivity simply cannot be the assertion here. The second is that activating `paste` on
a folder with a collision does not return until the prompt is answered, since the prompt
runs its own `GLib` main-context loop; the harness backgrounds the activation and drives
the prompt from the shell. What the checks then assert is the effect rather than the
widget: the prompt names both sides of the collision, answering `Replace` changes the
bytes on disk and answering `Skip` leaves them alone, and the window closes either way.
The unit tests cover the answers the harness cannot reach — a standing replace-all, a
standing rename-all, a cancel that abandons the whole batch, a folder that merges rather
than fails — because those are decisions about a plan, and a plan does not need a screen.

The live-refresh checks double as the cache's end-to-end check, and that is the reason
they are load-bearing rather than incidental. A listing is now remembered between
visits, so a folder that gains a file and then shows it could be showing either a fresh
read or a cached listing that happened to be dropped for some other reason. What makes
the check mean something is that the write comes from the shell and nothing else
happens: no action is activated, no navigation happens, and the only thing that could
have invalidated the entry is the watch event itself. The unit tests cover the other
half — that a hit really is a hit, that a different view state is a miss, that a
cancelled read stores nothing, and that invalidation reaches a folder's parents as well
as its children — but the pairing of the two, a cache that is both fast and honest, is
only visible with a real folder changing under a real window.

The two live-refresh checks are the only ones that assert on the absence of a command.
Every other check either activates an action or waits for one; these write to the
fixture from the shell and then wait, touching nothing, because the claim being tested is
reload and no navigation were needed. Activating the window's own `refresh` action before
looking would make the check pass with watching removed entirely, and would be testing the
reload button instead. Both directions are checked: a directory that gained an entry and
one that lost it, since the watcher reports those through different events and only the
second can arrive after the kernel has already dropped the watch.

The picture-preview checks cannot assert on the accessibility tree at all. A row that
received a preview and a row that kept its themed icon are the same node with the same
name, because an image carries no label, so the assertion moves to the one place a
preview has to leave a trace: the freedesktop cache directory under the throwaway
`$HOME`. The switch is checked through `Describe`, whose state is the preference — a
menu checkmark that disagreed with what the window does would be worse than no switch
at all. Whether a decode is possible at all depends on the machine and not on this
program: `GdkPixbuf` hands every format but `XPM` to a loader that runs out of process
inside a sandbox, and a container that cannot create a pty cannot start one. So the
section begins with `tests/files-ui-pixbuf.py`, which writes a real `PNG`, tries to
read it back, and answers `ok` or `no`; on `no` the section prints a `skip` line rather
than a failure it did not cause. On a host that can decode, the same checks run for
real, and the one that matters is the cache file: it says the worker ran, the decode
succeeded, the encoder accepted the result and the write landed, which no view of the
tree could show.

The extraction checks are the first to act on a window other than the one the script
started. `GTK` exports every window an application owns, so the archive window is
`window/2` on the bus and the script's action helpers take an object path rather than
assuming the first one. The numbering survives the folder prompt that runs before it
for a reason worth keeping in mind when adding to either: a prompt is a plain window
that is only made transient for the main one, so it is never exported and never takes
an index.

The context-menu checks cannot name a row, which is worth knowing before adding to
them. `GTK` 4.22 rebuilt `GtkPopoverMenu` on a widget tree, and the rows it produces
answer the role `menu item` with an empty accessible name, empty description and empty
id — the label is drawn by a child that is not itself exposed — so there is no text for
a test to reach a row by. What a row does expose is a `keyshortcuts` attribute holding
the accelerator installed for the action it names, so the probe gained `menu`, which
prints one chord per visible row, and `menu-press`, which activates a row found by its
chord. That is a handle on the row and on the accelerator table at once, which is what a
keyboard user has. A row with no accelerator prints `-`; the one row that has none is
`Open`, because plain `Return` already activates a row. A closed popover contributes no
nodes at all, and the application has no other node of role `menu` while one is open, so
the presence and absence of that role is what says whether the menu opened and whether it
closed. The expected row set is compared whole rather than row by row, and it contains
`Control+V` because the copy that fed the collision prompt is still on the clipboard — a
copy can be pasted more than once — while `Control+E` is absent because the window is
standing on an ordinary folder.

That section is also how a real bug in the navigation history was found. Going up from
the fixture, back to it, and up again left the history's cursor pointing at the wrong
entry, so the following `Back` did nothing and the window never returned to the folder
the section began from; `History::push` was comparing a location with the tip of the
list instead of with the entry under the cursor. The unit tests for `History` could not
have caught it, because the two agree until something moves the cursor off the tip, and
only the harness walks that far.

The Open With checks are the only ones that install something into the fixture, and
that is the point of them. The chooser's list comes from the desktop database, and a
container with no `MimeType=` anywhere has an honest empty list — a check that only
looked for a dialog would pass on a machine where the lookup returns nothing at all.
So the section writes `Probe Editor` into the private `$XDG_DATA_HOME` the whole script
already runs with, refreshes the database, and then asserts the whole chain: that
`Describe` reports the action disabled for a folder and enabled for a plain file, that
the chooser draws a row for a name nothing else in the interface says, that pressing it
leaves a marker file behind, and that the argument the program was handed names the
selected file. The last of those is asserted against a plain path rather than the
`file:` URI the chooser passed, because `GIO` turns a `file:` URI back into a filename
when it builds a desktop entry's command line — which is the right answer for a program
expecting an argument it can open, and is reported as measured rather than as hoped.
The section prints a `skip` line where there is no `update-desktop-database`, on the
same reasoning as the picture previews. The chooser is closed again at the end because
the frame count `--new-window` is checked against is read from the same bus.

The preview checks sit after the `Open With` ones deliberately, and one of them exists
because of that. The text pane is what makes the popover checkable at all: its content
is the file's own bytes, which appear nowhere else on screen, so finding the string
proves the worker read the file and the answer found the label that asked for it. The
mode string is read off the fixture with `stat` for the same reason the properties check
reads it that way. The picture check asserts on the cache rather than the tree, like the
row-icon checks, and asserts on the *band*: a preview that shared an entry with an icon
would paint a 16 px picture at thirty times its size, and that looks like a rendering bug
rather than a cache one.

The archive window from the extraction section is closed before this section begins, and
that is not tidiness. An autohide popover anchors the grab that dismisses it on an
outside click to the parent window's keyboard focus, and a window holding none dismisses
the popover inside `GTK`'s own focus handling as the popup maps — not through `popdown`,
which is why the dismissal survives turning `autohide` off and why it took a while to
find. The `Open With` chooser had just taken that focus, and a `select` through the
accessibility tree moves the selection without moving the focus, so the window reached
this point with none. The action now puts focus back on whatever listing is on screen
before opening, which is the state a real user is always in; the check that the popover
stays open long enough to be read is therefore also the check that the fix holds.

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
- Replacing across devices, and a trash that lives on another filesystem than the
  row being discarded: both need a second mounted filesystem in the fixture, which
  the container has no way to offer honestly. The conflict prompt itself is the same
  code either way, and is covered below.
