# Architecture

UFLinux Files is a GTK 4 file manager written in Rust, without libadwaita. It
browses directories, archives and mounted network shares through one path model,
searches filenames and application entries through a Tantivy index, and provides
the desktop icon model that `uflinux-desktop` draws.

The design constraint throughout is a 512 MiB machine with no swap. That rules
out two things a naive implementation would reach for: a second full-screen
process to draw desktop icons, and an async runtime to keep the UI responsive.
Neither is needed. The UI stays responsive because everything slow happens on a
small pool of worker threads publishing to a GLib main context, and the desktop
stays cheap because the shell draws icons while this program only supplies data.

## Two kinds of crate

    crates/
      ufm-core/          pure Rust
      ufm-vfs/           pure Rust
      ufm-archive/       pure Rust
      ufm-mount/         pure Rust
      ufm-index/         pure Rust
      ufm-search/        pure Rust
      ufm-desktop-ipc/   pure Rust
      uflinux-files-mount/  pure Rust, runs as root
      uflinux-files/     GTK4 + GIO
      uflinux-searchd/   GLib + GIO

Everything above `uflinux-files` has no `GLib` anywhere in its dependency graph.
That is not purity for its own sake: it means `cargo test` runs on any machine
with a C toolchain, so the mount planner, the archive readers, the query builder
and the two D-Bus wire formats are covered by ordinary unit tests on a build host
that has no GTK installed. The packaging builds `--workspace` inside a chroot that
does. It also keeps the two binaries thin, which matters because a binary's
resident cost starts from the libraries it links.

The mount helper is a binary and still pure Rust for a second reason. It runs as
root, so every dependency is code that runs with elevated privileges and has to be
audited; it links `libc` for `kill(2)` and nothing else that matters.

`uflinux-files-provider` is the case where that reasoning has teeth: it serves the
desktop icon protocol using GLib and GIO only, with no GTK and no windows, which is
what keeps its budget at 10 MiB rather than the 90 MiB a GTK process costs. It is a
separate binary rather than a mode of the file manager because the linker maps a
program's whole library closure at `exec`, so sharing an executable with the UI would
have charged the provider for Mesa and `LLVM` — measured at 28.6 MiB against a 10 MiB
budget, of which all but 7 MiB was shared library text. See
[memory-budget.md](memory-budget.md).

## The location model

`ufm_core::Location` is the load-bearing type. Every view, every operation and
every cache key is a `Location`, and there are two kinds:

- `file:///abs/path` — a local directory **and every mounted network share**.
  Shares are mounted under `$XDG_RUNTIME_DIR/ufm/<slug>`, so a CIFS share is a
  path like any other: thumbnails, drag-and-drop, sorting, copying and the
  column view all work on it with no remote code path.
- `archive:///home/u/a.zip!/inner/dir` — the only virtual location. `!`
  separates the container from the path inside it; a literal `!` in a filename
  is percent-encoded, so the separator is unambiguous.

Three more spellings exist for virtual folders backed by the indexer:
`computer://`, `recent://`, `starred://` and `search://<id>`. They are views over
data the other locations already describe, not a second filesystem.

Because mounting covers the network, `DirReader` has exactly two
implementations: `Local` and `Archive`. A third per protocol would mean every
feature has to be written N times, and the features that are hard — random
access, streaming, partial reads — are exactly the ones nobody would rewrite.

## Reading a directory

`DirReader::read_streaming` yields entries through a `ListingSink` rather than
returning a `Vec`. A worker thread reads, sorts and filters, and publishes in
slices sized to fit one frame, so opening a 5 000-entry folder paints the first
rows immediately instead of after the last one. `Cancel` is a flag the reader
checks between batches; leaving a folder mid-read is the common case, not an
error.

`DirCache` sits inside `LocalReader` and holds at most 64 directories and 20 000
entries for 30 seconds, which is what makes Back repaint rather than re-walk. A hit
is served as one sorted listing and no batches, because batching exists to paint
something while `readdir` is still running and there is no `readdir` on this path;
the count a caller gets back is the number of rows handed over either way, so
nothing upstream has to know a hit happened. What is stored is exactly what the view
was given — filtered, sorted, and stored only after the listing has been handed over,
so a read cancelled part way through leaves nothing behind. An empty folder is cached
too: it is the folder the user just emptied.

The key is the path plus the view state that changes the answer — sort field,
direction, folders-first, hidden, and the filter text — because a listing sorted by
size is not a listing sorted by name and showing the wrong rows is worse than
reading twice. Only a real directory can be a key at all: an archive location's path
names its *container*, so every folder inside one archive would collide, and
rewriting the container emits no inotify event to notice.

Correctness rests on invalidation rather than on short lifetimes, so every path that
means "look again" forgets first. The unit of that is `invalidate_around`, which drops
the folder's own listing, its parents', and everything below it. The parents matter
because of the shape of the window: a file created in `src` makes the `projects`
column stale, not the `src` column, whose rows the window re-reads anyway — and a
caller holding one path cannot say which column a change shows up in. So a watch event
invalidates each reported path, a finished file operation invalidates each folder it
reported (including the source of a move, which is nowhere on screen and so gets no
re-read to do it), a mount invalidates its mount point's neighbourhood, a sweep
invalidates what it detached, and the reload button drops the whole cache. Navigation
deliberately does not: that is the point.

## Live updates

A listing read once is a photograph: a file saved in an editor, a download that
finished, a folder deleted from a terminal, none of it appears until the user
navigates or reloads. `ufm_vfs::watch::Watcher` is the inotify layer — one thread,
a 300 ms debounce, coalesced per directory, capped at 16 watches — and the UI, the
provider and the indexer each drive their own instance of it.

A window watches what it shows: the folder in the title bar plus the column chain,
which is at most seven paths. The set is recomputed on every navigation rather than
accumulated, because watching a home directory recursively costs one kernel watch
per subfolder, which on a large tree is more memory than the listing itself.
Recomputing is also what re-acquires a watch the kernel dropped on its own: deleting
a directory retires its watch, so the set is re-declared after a live re-read as
well, which is how a folder that was deleted and created again becomes live once
more rather than staying dead until the user leaves it.

What happens on an event depends on which half of the window the folder is. A
parent column re-reads itself alone; re-reading a chain of five small folders
because one of them moved is four directory walks wasted. The folder in the title
bar is re-read by the window, through a path that deliberately does not go via
navigation: navigation empties the view first, so a window would flash blank on
every save in an editor, and it drops the filter the user typed and resets the index
bar. The column holding those same rows is told to expect the listing rather than
reading the directory a second time.

A change that arrives while a listing is already on its way is only noted, and
answered once that listing lands. Without that rule a program writing forty files
cancels forty reads and walks the folder forty times; with it the burst costs one
walk, and the listing that was already coming usually answers it for free.

Every re-read that a watch event triggers drops the cache for the reported paths
first. A re-read served from the cache shows the very rows the event was reported in
order to get rid of, and a column that is not re-read at all still has to forget what
it remembered, or the next visit to it paints a folder as it was before the change.

## Archives

`ufm-archive` builds a table of contents in one streaming pass and caches the
last eight containers, keyed by path, mtime and size. zip is read in Rust with
random access; tar is parsed in Rust with the codec handled by an external
`gzip`/`bzip2`/`xz`/`zstd`, which is what lets the crate stay pure Rust; `.7z`
goes through the `7z` command line, because the maintained Rust crate needs a
newer compiler than the packaging toolchain and the unmaintained one has an open
advisory.

Browsing is read-only. Extract writes to disk and is the only mutation. A
corrupt container degrades to an error row in the listing rather than an empty
folder, which is the difference between "this archive is odd" and "this archive
is empty".

Unpacking a whole container goes through a helper process — `unzip`, `tar`, `7z`
— rather than through the readers, because a helper applies its own long-tested
rules about permissions and links and because extracting a 4 GiB container then
costs about what `unzip` would cost. Member-by-member extraction through this
crate's own readers is what a single selected folder needs, what a bare `.gz`
holding one file needs, and what runs when no helper is installed; it is also the
fallback when a helper fails, so the fast path is an optimisation and never a
requirement. Both paths stream, and both write each member to a dotted temporary
name before renaming it, so a container that fails halfway leaves no member that
looks complete; the destination folder is removed whole when the extract fails, so
a cancelled one leaves nothing to clean up by hand. What reaches the caller is a
`Report`: where it landed, how many items, how many bytes, and what was skipped.

The UI half of extraction is a separate module from the file operations, because
the two answer to different rules. A paste takes names the user chose and goes
through GIO; an extract takes a name the archive chose and goes through
`ufm-archive`, and the folder it creates is a sibling of the container rather than
a child of whatever folder happens to be on screen. The destination is computed
before the worker starts — the container's base name, with the codec suffix removed
whole so `photos.tar.gz` becomes `photos` and not `photos.tar`, made unique by the
same counter a paste uses — and there is no dialog asking for it, which is what lets
the command be a single keystroke. Extract is offered in two places and both go
down the same path: on a selected archive row, and while standing inside an
archive, in which case the location being browsed is turned into the root of its own
archive so the library sees one shape of input rather than two. A position partway
inside an archive deliberately has no extract command; unpacking `docs/` and
unpacking the card that holds it are different questions, and guessing wrong about
which one was asked is worse than not offering it.

## Mounts

Nothing here speaks SMB, NFS or FTP. It plans a mount and runs it. See
`docs/mounts.md` for the protocols, the privilege split and the credential
rules.

## File operations

Copy, move, rename, trash, delete and new folder live in one UI module, and
every one of them goes through GIO on a worker thread rather than through
`std::fs`. That is what makes them work on a mounted share, on a path with a
non-UTF-8 name, and on a filesystem with a recycle bin, without the module
knowing about any of those things.

Two of GIO's calls are not what a file manager needs, and both are made up for
locally. `g_file_copy` refuses a directory, so copying a folder walks it and
rebuilds it, recreating each directory before filling it so an interrupted copy
leaves a partial tree rather than a flat one. `g_file_delete` refuses a non-empty
one, so deleting a folder recurses first. Trash is tried before deleting, and a
filesystem that has no trash falls back to deleting rather than reporting a
failure for something the user asked to be removed.

Naming is decided before anything touches the disk. A paste builds a plan — a
list of `(from, to)` pairs plus a reason for each row it dropped — and the
worker only ever executes that plan. That is what lets a batch skip the one row
that cannot land rather than aborting the six that can, and it is where the
counter names come from: `report (2).pdf`, `Untitled folder 3`, with the counter
growing after the extension and a leading dot counted as part of the stem, so
`.bashrc` becomes `.bashrc 2` and `archive.tar.gz` becomes `archive (2).tar.gz`.

The result arrives as one event on the same queue a directory listing uses, so
an operation refreshes the folder it changed and reports through the same banner
a failed read uses. Nothing about an operation blocks the interface, and no
operation needs a thread of its own beyond the one running it.

A taken name is a question rather than a decision. The plan records one
collision per row whose name the destination already has, holding both sides of
it — what is being pasted, and what `stat` says is standing there — along with
how many collisions sit behind it, which is what lets the prompt offer "apply to
the rest" instead of asking the same question six times. `resolve` turns the plan
into an answered one: replace points the step at the name it displaces and marks
it, skip drops the step and leaves a line for the banner, cancel empties the plan
and says how much was lost. A standing answer — replace all, rename all, skip
all — is remembered by kind, so the rows behind the one that was asked about are
never asked about again. Replacing a folder merges into the one already there
rather than failing, because a folder that refuses to accept a paste is the
result nobody wanted.

The prompt itself lives in its own module and blocks on the `GLib` main context
rather than on a thread, which is how `gtk_dialog_run` worked and what keeps the
interface repainting while the question is open; the window's visibility is the
loop's second exit, so closing the folder out from under a prompt abandons the
paste instead of hanging on it. One consequence matters for anything that drives
the interface from outside: activating `paste` on a folder with a collision does
not return until the prompt is answered.

Reading the existing side has one trap worth recording. `time::modified` is a
`UINT64` attribute, so `attribute_int64` trips a type assertion inside `GLib` and
answers zero, and `attribute_status` is no use as a presence test — it reports
`Unset` for attributes a plain `stat` clearly filled in. The timestamp is read as
a string and parsed, which leaves exactly two answers: a number, or nothing at
all. A row that could not be read at all is shown as a folder with no size rather
than as an empty one.

What is deliberately not there yet: there is no progress reporting, so a large
copy reports once at the end; and there is no undo. Both want a job record that a
single operation does not need. Extracting an archive shares both gaps, and shares
the reason: the command reports once, when it is over.

## Search

`uflinux-searchd` owns the two Tantivy indexes and serves
`org.uflinux.Search1`. `ufm-search` holds the wire format, the index
configuration, the text extractors and the `Exec` expander — everything the
client and the daemon must agree on, so the two cannot drift. The file manager is
a client; it never opens the index. See `docs/search-indexer.md`.

## Desktop icons

The file manager owns the model, `uflinux-desktop` draws it, over
`org.uflinux.FileManager.Desktop1`. See `docs/desktop-integration.md`.

## Concurrency

No tokio. A GLib main context plus a bounded worker pool, the same shape
`uflinux-settings` uses. Mounts are external processes, archive readers are
synchronous, D-Bus is GIO's. An async runtime would add threads, heap and a
second way for a task to be cancelled, in a process whose budget is measured in
single-digit megabytes for the provider and under a hundred for the UI.

The rule that keeps the UI honest: nothing on the main thread touches the
filesystem except `stat` on a path already being displayed.

## Error handling

`ufm_core::Error` has nine variants and every one of them carries a string the
user can be shown. `InvalidLocation` and `UnsafeComponent` come from parsing,
`Mount` names the package to install, `Cancelled` is not an error and is never
displayed. Library code returns `Result`; the UI decides whether an error is a
dialog, a banner or a row in a list.

## Where state lives

| File | What |
| --- | --- |
| `~/.config/uflinux/filemanager/preferences.json` | view mode, sort, column widths, sidebar |
| `~/.config/uflinux/filemanager/bookmarks.json` | bookmarks |
| `~/.config/uflinux/filemanager/mounts.json` | mount table |
| `~/.config/uflinux/filemanager/credentials.json` | saved credentials, `0600` |
| `~/.config/uflinux/filemanager/mounts/*.conf` | per-mount credential files, `0600` |
| `~/.config/uflinux/filemanager/desktop.json` | desktop icon layout, provider only |
| `~/.config/uflinux/filemanager/index.json` | indexer configuration |
| `~/.cache/uflinux-files/index/` | the two Tantivy indexes |
| `~/.cache/thumbnails/` | thumbnails, freedesktop layout |
| `~/.cache/uflinux-files/extracted/` | single members materialised for open-with |

No GSettings schema. The distro stores configuration in JSON, and a schema would
add a compile step, an installed XML file and a dependency on
`gsettings-desktop-schemas` for state that only this program reads. The one
GSettings value read is `org.gnome.desktop.interface color-scheme`, read-only and
only if the schema is present.

All writes are atomic: a temporary file in the same directory, `fsync`, rename.
A crash leaves the previous file intact rather than a truncated one, which
matters most for the mount table, where a half-written file would mean a share
the user cannot unmount from the UI.
