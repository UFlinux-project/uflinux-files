# UFLinux Files

A file manager for [UFLinux](https://github.com/UFlinux-project/uflinux3): GTK4
without libadwaita, written in Rust, laid out like GNOME Nautilus and modelled
on the `nautilus-my-computer` view for everything that is not a folder.

It browses directories, archives and mounted network shares through one path
model, indexes filenames and application entries with Tantivy, and provides the
desktop icon model that `uflinux-desktop` draws.

    uflinux-files ~/Documents            # open a window at a folder
    uflinux-files computer://            # the My Computer panel
    uflinux-files --sweep-mounts         # clean up after an unclean shutdown

Desktop icons are served by a separate program, `uflinux-files-provider`, which holds
the model over D-Bus and never opens a window.

## What it does

- **Three views** of the same folder: icon grid, list with columns, and macOS
  Miller columns. Per-tab view mode, sort order and column widths are remembered.
- **A My Computer view** for the things that are not folders: `computer://` renders
  a card per drive and per mounted share, each saying how full its filesystem is
  and whether the share is still there, with a Disconnect button on the ones that
  can be. It is a fourth presentation rather than a fourth reader — there is no
  directory behind it to read.
- **Archives as folders**: zip, tar and its codecs, and 7z open like a directory
  and unpack with `<Ctrl>E` into a folder named after them. Read-only browsing
  plus extract; no write-back.
- **Network shares as folders**: SMB, NFS, SFTP, WebDAV and FTP are *mounted*
  rather than spoken, under `$XDG_RUNTIME_DIR/ufm`, so every view, thumbnail and
  file operation works on them without remote special-casing.
- **Search** over filenames, metadata and application entries, served by a
  D-Bus-activated indexer that exits when it has been idle. The same interface
  is what a start menu will use instead of its own application catalog.
- **File operations**: copy, cut and paste, rename, new folder, trash and
  delete, on `<Ctrl>C`/`<Ctrl>X`/`<Ctrl>V`, `F2`, `<Ctrl><Shift>N`, `Delete`
  and `<Shift>Delete`. Each one runs on a worker and reports when it is done,
  so a large folder never freezes the window; a paste that cannot place one row
  skips that row with a reason instead of abandoning the rest. Pasting onto a
  name the folder already has asks rather than guessing, showing both rows and
  offering replace, rename or skip for the whole batch; replacing a folder merges
  into the one already there.
- **One undo, and an honest one**: `<Ctrl>Z` puts the last operation back — a row
  out of the trash and back where it came from, a rename back to the name it had, a
  new folder taken away again. The trash is read from the freedesktop layout rather
  than through `GVfs`, so it works on a minimal install; a file a paste replaced is
  kept in a stash until the window forgets it, and a folder a paste merged into is
  named in the banner rather than quietly lost. One operation deep rather than a
  stack, because a record that no longer matches the disk is worse than none.
- **Desktop icons**: this program owns the model — what is on the desktop, where
  each icon sits, what happens when one is double-clicked — and
  `uflinux-desktop` draws it, so there is no second full-screen GTK process.
- **Live folders**: the folder on screen and the columns beside it are watched, so
  a file saved in an editor, a download that finished, or a folder deleted from a
  terminal appears without reloading. Watching is capped at what a window can see,
  and a window whose watches the kernel refused simply behaves the way it did
  before: it updates when asked to.
- **Back is instant**: the last 64 folders are kept in memory, so revisiting one
  repaints instead of walking the directory again — the same path a mounted share
  would otherwise pay a round trip for. Whatever changes a folder drops its entry,
  which is why watching and caching are the same feature seen from two sides.
- **Picture previews**: images are decoded at the size of the cell on one worker,
  started when the first folder needs one, with a queue that drops the oldest
  request when a scroll outruns the decoder — so the cost of previews is the
  viewport's and not the folder's. The cache follows the freedesktop thumbnail
  spec, so pictures already drawn by other software are read rather than decoded,
  and the desktop provider and the file manager share one implementation of the
  key and the freshness rule. Off with one click in the view menu.
- **Properties** on `<Alt>Return`: what a thing is, where it lives, when it
  changed, its mode bits, owner and group — every row drawn from what the listing
  already holds, so the dialog appears at once. A folder's total is the one fact
  that has to be counted, so it says it is counting and the number arrives when a
  bounded walk finishes on a worker; a walk that stopped early says *at least*
  rather than stating a total it did not finish computing.
- **A right-click menu that says what this folder allows**: the rows are built from
  the selection and from where the click landed, so `Extract` appears only over
  something unpackable and `Paste` only when the clipboard has something for this
  folder. Each row activates the window's own action, which is why it is sensitive
  exactly where the header menu and the keyboard are; `Menu` and `<Shift>F10` open
  the same menu over the focused row.
- **Open With…**: the list of programs is the desktop database's, not a second list
  kept here, so the chooser cannot disagree with what the system believes about a
  file — and a type nothing is registered for is named out loud rather than answered
  with an empty dialog. It works on a member of an archive too: the one member is
  written out to a cache that forgets what is a week old, and the program is handed
  that.
- **A preview on `Space`**: one row, one popover, no detour. The facts come from the
  listing, so it is instant; a picture is one request to the same thumbnail pipeline
  the rows use, decoded at the largest size the cache's `large` band holds; and a text
  file shows its opening bytes, read on a worker that stops at 64 KiB so a 2 MiB
  document costs no more than a 2 KiB one. The same key closes it.
- **Progress you can watch and stop**: a copy, move, delete or extract puts a strip
  above the listing naming the item it is on and how far along it is — bytes when every
  source reported a size, rows when one did not — and reports at most ten times a
  second however many files are moving. One `Cancel` stops everything running, in the
  middle of a `GIO` call and in the middle of an archive alike, and a cancelled extract
  leaves no half-built folder behind.

## Layout

| Path | What it is |
| --- | --- |
| `crates/ufm-core` | Location model, entry metadata, sorting, config, well-known paths |
| `crates/ufm-vfs` | `DirReader`, the local reader, directory cache, cancellation, inotify watcher |
| `crates/ufm-archive` | Archive table of contents, per-format readers, extraction |
| `crates/ufm-mount` | Mount table, per-protocol mount plans, credential store, stale-mount sweep |
| `crates/ufm-index` | Tantivy schema, writer, crawler, query builder, `.desktop` parser |
| `crates/ufm-search` | The `org.uflinux.Search1` wire format, index config, `Exec` expansion |
| `crates/ufm-desktop-ipc` | The `org.uflinux.FileManager.Desktop1` protocol and desktop model |
| `crates/uflinux-files-mount` | The privileged mount helper `pkexec` runs as root |
| `crates/ufm-thumb` | The thumbnail cache: key, freshness rule, decode. GLib and GdkPixbuf, no GTK |
| `crates/uflinux-files` | The GTK4 UI and the mount runner |
| `crates/uflinux-files-provider` | The desktop-icons provider: GLib and GIO, no GTK |
| `crates/uflinux-searchd` | The indexer daemon |
| `data/` | Desktop entry, icon, D-Bus activation, polkit policy and rules |
| `aports/uflinux-files/` | Alpine package recipe |
| `docs/` | Architecture, mounts, indexer, desktop integration, memory budget, validation |

Everything above `uflinux-files-mount` is pure Rust with no GLib in sight, which is
what lets it be built and tested anywhere; the three binaries and `ufm-thumb` are
where GLib enters, and only the file manager links GTK. The mount helper is a binary
and still pure Rust on purpose: it runs as root, so a small dependency graph is a
security property rather than a preference. The provider and `ufm-thumb` link GLib and
nothing above it, which is a memory property rather than a preference — see
[memory-budget.md](docs/memory-budget.md).

## Building

The workspace needs Rust 1.88 or newer. The library crates and the mount helper are
default members and build anywhere:

    cargo test
    cargo build --release

The two GLib binaries are not default members, so they need GTK 4.12 and GLib
development files (`gtk4.0-dev`, `gtk4-layer-shell-dev`, `glib-dev`, `gdk-pixbuf-dev`
on Alpine) and are built by the package recipe:

    cargo build --release --workspace

At runtime, everything optional is named in the user's language when it is
missing rather than failing quietly: `7z` for 7z archives, `cifs-utils` and
`nfs-utils` for those two protocols, `sshfs-fuse`, `rclone` or `davfs2` for the
rest.

## License

GPL-3.0-only. See `LICENSE`, and `docs/architecture.md` for the reasoning behind
the structure above.
