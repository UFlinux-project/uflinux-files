# Desktop icons

UFLinux already has one resident GTK4 process that owns the wallpaper and the
panel: `uflinux-desktop` (`org.uflinux.Desktop`, started `--daemon`). A second
full-screen process drawing an icon grid on top of it would cost a compositor's
worth of surfaces to paint a dozen icons. So the work is split along the line
where the knowledge already is:

| | owns | because |
| --- | --- | --- |
| `uflinux-desktop` | the pixels, the geometry, the clicks | it already has the layer-shell surface and the monitor layout |
| `uflinux-files-provider` | the filesystem, the layout, the actions | it already knows what is in the desktop directory and how to launch or trash it |

`ufm-desktop-ipc` is the seam: types and validation, no I/O, no GLib.

## The protocol

`org.uflinux.FileManager.Desktop1` at `/org/uflinux/FileManager/Desktop`. One JSON
string per direction, 256 KiB in, 2 MiB out, unknown fields rejected — the same
convention as `org.uflinux.Settings1` and `org.uflinux.Search1`, so a client that
speaks one of the three speaks all of them.

| Method | Why it is shaped this way |
| --- | --- |
| `Status` | version, directory, item count, and whether a renderer owns the renderer name |
| `GetModel` | the whole model; the desktop is tens of items, so there is no paging |
| `SetGeometry` | replaces the set of grids wholesale — a monitor that stopped being mentioned is a monitor that was unplugged |
| `Place` | id + three integers, flat, so a drag ends in a small message rather than a nested struct |
| `Launch`, `Reveal`, `ContextMenu`, `Trash` | the renderer forwards an intent and needs no knowledge of desktop entries, `MIME`, or the trash directory |
| `RequestThumbnail` | reply carries a file descriptor, so image bytes never sit in a D-Bus message |

Signals: `Changed` (a new generation — deliberately *not* the model itself, so two
changes back to back collapse into one repaint while a folder is being dragged
across the desktop) and `Progress` (`scanning`, `placing`, `done`, so a shell can
show a spinner only while the first model is being built).

Errors: `Invalid`, `NoItem`, `Busy`. `NoItem` is a name rather than a bare failure
because a renderer that raced a delete must be able to drop that one icon without
tearing down the grid.

## Rules both sides have to agree on

An **item is named by one path component**, never by a path. `Model::path_in` joins
it to the desktop directory and refuses anything that would leave it, so a renderer
cannot ask for a thumbnail of `~/.ssh/id_rsa` by sending an id with slashes in it.
255 bytes maximum, which is `NAME_MAX` on every filesystem the desktop directory can
live on.

A **cell names a monitor**. Two screens both have a cell at 0,0, and a layout that
cannot say which one an icon is on loses half the icons the first time a monitor is
unplugged — which is when losing them hurts most.

**Nothing is deleted by a geometry change.** An icon whose monitor vanished is moved
to a screen that still exists; only an icon that genuinely cannot fit anywhere is
left without a cell, and reported to the caller as such. A shell that hid icons
because their screen was temporarily off would look like it had eaten the user's
desktop.

A zero-sized grid is rejected rather than treated as an empty grid, because an empty
grid would silently swallow every icon on it. Caps: 8 monitors, 64 columns (a column
is 96 px at the largest icon size, so 64 is a 6K monitor at 1x), 512 rows, 1024 px
cell edge, 2048 items.

`Kind` decides the icon fallback and the double-click: `directory`, `file`,
`launcher`. A `.desktop` file runs `Exec` rather than opening as text — and only
becomes a `launcher` at all if GLib accepts the entry, which includes the `Exec`
program resolving, so an icon left behind by an uninstalled application is offered
as the text file it is rather than as something that fails when double-clicked. The
fallback names (`folder`, `text-x-generic`, `application-x-executable`) come from the
model so a renderer never invents its own and every desktop shows the same thing.

## The provider

`uflinux-files-provider` claims the bus name and does **not** link GTK — GLib and
GIO only, in a binary of its own. That is what keeps it at ~3 MiB resident: no window,
no widget tree, no CSS provider, no icon theme search beyond what a thumbnail needs.
It:

- watches `$XDG_DESKTOP_DIR` (falling back to `~/Desktop`) through the same
  coalescing watcher the file manager uses;
- owns slot assignment and persistence, in its **own** file,
  `<config dir>/desktop.json` — separate from the UI's preferences, so clearing UI
  settings does not scramble the desktop and vice versa, and because a slot needs a
  monitor to mean anything, which a layout keyed by name alone cannot express;
- writes that file whole, atomically, mode `0600`: it is not secret, but it is
  state, and a world-readable file in `$XDG_CONFIG_HOME` is a smell;
- does the actual move, launch, reveal and trash, so the trash directory and the
  confirmation rule live in one place.

`Layout::apply` is where a saved layout meets a directory that has changed since: an
icon that is gone loses its cell, an icon that is new gets one from the free-cell
scan, an icon that is still there goes back where it was. That asymmetry is the
point — the layout is a cache of the user's intent, not a record of which files
exist. The generation counter is carried across restarts so a renderer comparing
generations never sees one go backwards after a provider restart.

The file is a `BTreeMap` keyed by item id, so it is byte-identical after a round trip
and a diff of two versions says something meaningful.

### Coming down

Three things stop the loop: `SIGTERM`, `SIGINT`, and the session bus closing. The
third matters because a session that ends without stopping its services would
otherwise leave a process holding a name nobody can call and reading a directory
nobody looks at. Quitting on a closed bus is also what makes the shutdown orderly:
the loop stops while the connection is still open, so the service can withdraw its
object and release its name itself rather than being overtaken by GLib's teardown of
a dead connection.

That release has to be explicit. Neither the bus-name handle nor the registration
returned by GIO's Rust bindings has a `Drop` impl, so holding them releases nothing,
and the only binding that takes no callbacks aborts the process at logout — it
unwraps a connection GLib is allowed to pass as `NULL` once the bus is gone. So the
name is requested through the plain C entry point, whose two handlers are nullable,
and given back in `Drop`. `uflinux-searchd` does the same for the same reason, and
both are pinned by a test that reads their own source, because the failure only
appears when a session ends.

## The renderer patch in `uflinux-desktop`

`uflinux-desktop` claims `org.uflinux.FileManager.Desktop.Renderer` and uses
the wallpaper surfaces already mapped on each output:

1. Send `SetGeometry` for the allocated space above the panel, updating it as screens change.
2. Fetch `GetModel` and render the icons above the wallpaper.
3. Forward `Place` after a drag within the same monitor, `Launch` on double-click
   or Enter, and `ContextMenu` on a right-click. The provider currently opens the
   Desktop folder for `ContextMenu`. Request thumbnails for displayed icons only.
4. Fetch the model again on `Changed`. Re-register geometry when the provider restarts.

Thumbnails are **off by default on targets with 512 MiB or less**, where the model
answers `thumbnails: false` and every icon uses its fallback name. A thumbnail cache
is the largest single thing this feature could add, and an icon grid is still useful
without it. The edge a renderer is told to ask for is the saved size clamped into
128..=512 px, so a layout written by another version cannot ask this process for a
thumbnail it would not decode.

## Fallback without a renderer

If no renderer claims the interface, the desktop has no icons and no error: the provider
answers `Status` with `renderer: false` and waits. The answer for that case is the file
manager drawing the grid itself, in a layer-shell window of its own, behind the
`desktop-icons` feature — an extra build flag rather than a second package, so the
feature works on any compositor while the default install pays nothing for it. Run it as
`uflinux-files --desktop-icons`; a build without the feature refuses the flag by name and
exits `2` rather than opening an ordinary window and looking like it worked.

The fallback opens one background window per monitor with all four anchors and
`KeyboardMode::None`. It checks the provider version and waits two seconds
before trying to claim the renderer name. It exits if the resident desktop
already owns the name or takes it over while the fallback is running.
Grids are reported from each surface's own
allocation once it is mapped rather than from the monitor geometry, which is what makes a
panel's exclusive zone subtract itself for free, and they are recomputed on
`monitors-changed`. Columns are capped by the protocol's column cap and rows by its row
cap separately; they differ by eight times, because a screen is wide and short, and one
cap for both would report a grid the provider is entitled to refuse.

Input is what a background surface can actually receive: double-click `Launch`,
right-click `ContextMenu`, drag-end `Place` inside the monitor the drag started on.
`Changed` is the only signal subscribed to, and a generation already drawn is ignored;
`Progress` is left alone, since a spinner needs a focusable surface and this one has
none. Thumbnails are asked for every icon drawn rather than only the visible ones, which
is the one place the fallback is lazier-looking than the contract asks for — the reason
is that it has no scrolling, so "visible" and "drawn" are the same set here. What it does
not have: marquee selection, keyboard navigation, a drag image, cross-monitor drag, or a
live view. The keyboard is out because a surface that never takes focus receives no
key events, and pretending otherwise would mean stealing focus from the panel.

The 2 s figure is a timeout and not a poll: the provider starts on the bus's demand, and
a shell that is still starting should win without the file manager having committed to
painting a desktop it will have to stop painting.

## Cost

The model is tens of items and every field a renderer skips when absent is skipped on
the wire. The one payload that could be large — a thumbnail — travels as a file
descriptor. Budget: **≤12 MiB added inside `uflinux-desktop` for 200 icons** with
thumbnails off, and ≤10 MiB for the provider process.

The fallback pays for being a process of its own. Measured in a release build in the SDK
container, with headless `labwc`, the Cairo renderer and 200 icons on one screen: **57
MiB** proportional for the fallback, of which the widget tree is a small part — it is a
GTK4 process with a GL driver loaded, which is exactly what the provider was moved out of
(see `docs/memory-budget.md`), and the provider alongside it measures 3.1 MiB. With the GL
renderer instead of Cairo the same process is 156 MiB, the same multiple the window costs
for the same reason. That number is why the fallback is a flag and not the default: on a
machine where `uflinux-desktop` is running, which is every UFlinux session, the ownership request is declined and it exits without mapping a surface.
