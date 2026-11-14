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
  skips that row with a reason instead of abandoning the rest.
- **Desktop icons**: this program owns the model — what is on the desktop, where
  each icon sits, what happens when one is double-clicked — and
  `uflinux-desktop` draws it, so there is no second full-screen GTK process.
- **Live folders**: the folder on screen and the columns beside it are watched, so
  a file saved in an editor, a download that finished, or a folder deleted from a
  terminal appears without reloading. Watching is capped at what a window can see,
  and a window whose watches the kernel refused simply behaves the way it did
  before: it updates when asked to.

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
| `crates/uflinux-files` | The GTK4 UI and the mount runner |
| `crates/uflinux-files-provider` | The desktop-icons provider: GLib and GIO, no GTK |
| `crates/uflinux-searchd` | The indexer daemon |
| `data/` | Desktop entry, icon, D-Bus activation, polkit policy and rules |
| `aports/uflinux-files/` | Alpine package recipe |
| `docs/` | Architecture, mounts, indexer, desktop integration, memory budget, validation |

Everything above `uflinux-files-mount` is pure Rust with no GLib in sight, which is
what lets it be built and tested anywhere; the three binaries are where GLib enters,
and only the file manager links GTK. The mount helper is a binary and still pure Rust
on purpose: it runs as root, so a small dependency graph is a security property rather
than a preference. The provider links GLib and nothing above it, which is a memory
property rather than a preference — see
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
