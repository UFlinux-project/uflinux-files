# Memory budget

The reference target is an **installed x86_64 system with 512 MiB, one display and
no swap** — the same machine the distro's own numbers are quoted against. Live
images load an APK-based root into RAM and need more. High resolutions, physical GPU
drivers and other architectures need separate measurements.

These are native reference targets, not emulated timing guarantees.

## Where the numbers come from

The file manager shares a session with the panel, the start menu, a terminal and
whatever the user opened. Every MiB here is a MiB taken from that. So each number
below is a consequence of a decision already made elsewhere, not a wish:

| Process | Budget | Why it is that size |
| --- | --- | --- |
| `uflinux-files`, one window, 2k-entry folder | **≤90 MiB PSS** | GTK4 + a widget per *visible* row. `GtkGridView`/`GtkColumnView` build widgets lazily, so the cost is the viewport, not the directory. |
| same, column view + thumbnails, peak | ≤120 MiB | up to 6 columns of widgets plus a 256-entry thumbnail LRU at 128 px. |
| `uflinux-files --provider` | **≤10 MiB PSS** | GLib and GIO only. No window, no widget tree, no CSS provider. This is the process that makes desktop icons cost something, so it is the one worth keeping small. |
| `uflinux-searchd`, idle | **≤24 MiB PSS** | an index and a searcher, and no writer: the writer holds the directory's write lock, so it exists only during a run. |
| `uflinux-searchd`, indexing | ≤64 MiB | 15 MB of that is tantivy's own writer floor, allocated eagerly; one worker, one merge thread. |
| Desktop icons added inside `uflinux-desktop` | **≤12 MiB for 200 icons** | the model is tens of items; thumbnails off by default at ≤512 MiB. |

Latency, same machine:

- cold usable frame **≤1.5 s**, warm **≤500 ms**;
- opening a column of 5k entries, p95 **≤80 ms**;
- no frame over **33 ms** while scrolling 200 rows.

The last one is why sorting and filtering never happen on the main thread, and why a
directory is published in ~8 ms slices: a 20k-entry folder read in one block is a
freeze, and a freeze on open is what makes a file manager feel slow on a low-end
machine even when the total time is the same.

## What the design buys

**No tokio.** A GLib main context plus a bounded worker pool. A tokio runtime is
threads and heap in every process that uses it, and here every use is either an
external process (mounts), a synchronous reader (archives), or GIO's own D-Bus
transport.

**No libadwaita.** Application CSS using `@theme_*` tokens only, so the theme still
follows `adw-gtk-theme` without linking a second toolkit's widgets.

**Thumbnails decoded at display size.** `gdk_pixbuf_new_from_stream_at_scale` rather
than decode-then-scale: a 12 MP phone photo decoded whole is 48 MB of pixel buffer for
a 128 px square. The disk cache follows the freedesktop thumbnail spec, so thumbnails
written by other software are reused and vice versa.

**The index is mmap'd with `Advice::Random`.** The pages of an index that is read once
per query are the pages the kernel should reclaim first. `MmapDirectory` also reports
its own cache statistics, which is what `Status` answers with — a daemon that cannot
say how much it is holding is a daemon you have to restart to find out.

**The directory cache is capped at 64 directories / 20 000 entries / 30 s.** A cache
with no eviction policy is a memory leak with a good reputation; the TTL is short
because a stale folder listing is worse than a fast one.

**The archive TOC cache holds 8 containers.** A TOC is the one structure worth keeping
— rebuilding it means re-reading the whole archive — and eight is more than anyone has
open at once.

**The watcher holds 16 inotify watches** with a 300 ms debounce. Past the cap,
directories are marked for a bounded partial rescan instead of watched. This is the
case that breaks an unbounded watcher: a home directory with hundreds of thousands of
directories, where one watch per directory is a file-descriptor limit and a kernel
slab the size of the rest of the process.

## Measuring

`/proc/<pid>/smaps_rollup`, the `Pss:` line — the same accounting the distro uses, and
the only one that is fair when several desktop processes map the same GTK and Mesa
libraries. `smem` is fine for looking at it by hand; the gate reads `smaps_rollup`
because it needs no dependencies in the test image.

`scripts/files-memory-gate.py` reads a JSON report and fails over the numbers above.
It evaluates measurements and does not manufacture them, so a report must carry:

- `environment`: renderer, compositor, scale, machine, available memory;
- at least three independent `sessions`, so a median means something;
- per session: `settled_pss_bytes` (after start, before interaction),
  `exercised_pss_bytes` (after the scenario), `warmed_pss_bytes` and
  `cycled_pss_bytes` (after 100 repetitions of it), plus the file-descriptor count
  before and after the cycles.

The gate fails on PSS over budget, on PSS retained across 100 cycles growing by more
than 5 MiB, and on a file-descriptor count that grew. The last two catch the failures
that a single sample cannot: a leak that only shows up after the user has done the
same thing a hundred times, and a watch or handle that is opened per folder and never
closed. A desktop-icons provider that gains a descriptor per directory viewed is
exactly the bug that check exists for.

`tests/files-memory-bench.py` produces that report for the six scenarios: provider
only, cold window, warm window, 5k-entry column view, search against a 200k-document
index, and the desktop-icons model at 200 items.

## What is deliberately not budgeted

The external programs — `7z`, `bsdtar`, `pdftotext`, `rclone`, `mount.davfs`,
`sshfs`, the codecs — are not counted here. They run as children, they are what makes
a 24 MiB daemon able to read a PDF, and their cost is paid by whoever asked. A
thumbnailer for video is left to the freedesktop thumbnailer spec for the same reason:
`ffmpegthumbnailer` is 30 MB of shared libraries that a machine without video files
should not pay, and a spec-compliant thumbnailer is one the whole desktop can use.
