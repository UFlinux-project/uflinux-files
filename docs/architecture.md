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

`DirCache` holds at most 64 directories and 20 000 entries for 30 seconds, and
is invalidated by the inotify watcher rather than by polling. The watcher coalesces
events with a 300 ms debounce and caps itself at 16 watches; past the cap the
overflowing directories are reported as needing a rescan instead of being
watched, which keeps the descriptor count bounded on a machine where a home
directory has hundreds of thousands of them.

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

## Mounts

Nothing here speaks SMB, NFS or FTP. It plans a mount and runs it. See
`docs/mounts.md` for the protocols, the privilege split and the credential
rules.

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
