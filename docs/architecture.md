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
      ufm-thumb/         GLib + GIO + GdkPixbuf, no GTK
      uflinux-files/     GTK4 + GIO
      uflinux-files-provider/  GLib + GIO
      uflinux-searchd/   GLib + GIO

Everything above `ufm-thumb` has no `GLib` anywhere in its dependency graph.
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

`ufm-thumb` is where the thumbnail rules live for the same reason. The provider and
the file manager both draw pictures, and a cache the two disagree about is a cache
that is read twice and decoded twice, so the cache key, the freshness rule and the
decode are one crate that both binaries call. It stops at `GdkPixbuf` and does not
reach GTK, which is what lets the provider keep its budget; the UI asks for encoded
`PNG` bytes rather than a paintable, because a `Pixbuf` is neither `Send` nor `Sync`
and the work belongs on a worker. See [Picture previews](#picture-previews).

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

Three more spellings exist for collections that are not directories at all:
`computer://`, `recent://` and `starred://`, plus `search://<id>` for the results
of one query. They are views over data the other locations already describe, not a
second filesystem, and none of them is ever handed to a `DirReader` — see
[The My Computer view](#the-my-computer-view).

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

Unpacking registers a job, so a large container reports members as they land
rather than once at the end, and can be stopped. The library's progress callback
counts members, which is the only unit an archive can count in before it is
walked; the count of a subtree is known before the walk starts, so the fraction is
real from the first member. A cancelled extract is reported as stopped rather than
as failed, and the folder it was building is removed, which is the same promise the
helper path makes.

## Mounts

Nothing here speaks SMB, NFS or FTP. It plans a mount and runs it. See
`docs/mounts.md` for the protocols, the privilege split and the credential
rules.

## The My Computer view

`computer://` is not a directory, and pretending otherwise is what made the
location print `not a valid location` before the view existed: the location was
handed to the local reader like any other, the reader refused it because there is
no path to open, and the window showed the refusal where a set of drives should
have been. What is behind `computer://` is a set of *capacities* — the home
folder, the system drive, a desktop folder that may or may not exist, and whatever
the user has mounted — and a directory listing has no way to say
`46.2 GiB free of 232 GiB`.

So the view is a presentation, not a reader: the third child of the window's mode
stack, beside the grid and the tree table. `navigate()` keeps all of its
bookkeeping for a virtual location — history, sidebar selection, button
sensitivity — and only the tail differs: it short-circuits before any worker
exists, so no read is started for something that cannot be read.

The cards are assembled by one pure function, `computer::cards(records, state,
hints)`, which takes the mount table, a way to ask a record's state and a way to
ask a path's capabilities, and returns plain `Card` values. The closures are the
point. The window's state sits behind a `RefCell` that a card builder would
otherwise have to borrow while painting, and the rules worth testing — which
places always appear, when a desktop folder becomes a card, what a share whose
kernel entry has gone missing says about itself — do not need a display, or a
window, or a mount. The mount table is re-read before each repaint: a share that
died while the window was open is a card that should say `Disconnected` rather
than a badge that says `Connected` about an empty mount point.

A share whose filesystem reports no size gets a card with the mount point where
the capacity would go, and no bar, rather than a bar stuck at zero. A share that
is gone is listed but not walked into; `can_open` follows the state, so activating
a dead card does nothing rather than opening an empty folder and looking like a
bug in the listing.

The two things a card can do — open, disconnect — travel as hooks set once at
construction rather than per repaint, because `set_cards` throws every widget away
and rebuilding a handler each time would mean the handler a click reaches is the
one from the last repaint. Disconnecting runs on a worker with the same layout the
mount dialog uses, and reports through `Event::Unmounted`, which is also what
repaints the cards: the view and the mount table are kept in step by the same
event that keeps the sidebar in step.

A card sets its own accessible name, and so does a sidebar row, for a reason that is
only visible from outside the process. GTK names a container that has no explicit name
by concatenating every label in its subtree, which for a card means the drive, its
mount point, a usage fraction and a capacity fused into one announcement that changes
as the numbers change. `accessible::Property::Label` on the `FlowBoxChild` — and on
the `ListBoxRow` in `ui/panel.rs`, which has the same shape and the same problem —
replaces that with the one string a reader needs, and leaves the rest as separate
nodes below it. Neither widget can be activated through AT-SPI either way: the bridge
gives a `list item` an empty action list, so rows are activated by keyboard and the
name is there to be read rather than to be pressed.

`recent://` is the other collection that is not a directory, and it is deliberately
the boring case: `recently-used.xbel` is parsed into a list of paths, each one is
`stat`ed through the reader the window already has, and the result is handed to the
ordinary listing. Recent files *are* files, so only the source of the list is
unusual. `starred://` has the same shape and its own section below.

## Stars

A star is not a file attribute. Nothing on disk says that a row was starred, so
the fact lives in `starred.json` and the flag on an `Entry` is a rendering of it,
set on the way into the widget tree. `ufm_core::starred` owns the file — load,
save, `star`/`unstar`/`toggle`, and the two sweeps that keep the list honest when
rows are deleted (`forget`) or a folder is removed underneath them (`prune`) — and
knows nothing about GTK. The window holds one `Starred`, loaded once at startup,
so a listing is stamped against memory rather than against a file read per folder.

Stamping is the reason the flag is a bit and not a lookup in the row: a factory
that asked the store per row would run a linear scan per row per repaint, and
`Starred::stamp` does the same work once for the whole listing. It *sets* the flag
rather than only setting it, so a re-stamp against a list that no longer carries a
star takes the badge off — the only way an un-star can be undone on screen without
re-reading the folder, which would not settle anything anyway because the
filesystem never knew.

That is also why `Row`, `View` and `Columns` each have a `restamp`. Rows are
otherwise immutable, and the cheaper-looking option — handing the view a fresh
`Vec<Entry>` — throws away the selection, which a user who just pressed `Ctrl+T`
or `Delete` would feel immediately. `restamp` rewrites the `Entry` each row already
holds and then re-runs the existing bind, so the widgets stay where they are and
the badge follows the store. The three copies of a listing (the window's state, the
model behind the grid, and each column pane's cache) all have to be told, or the
badge disagrees with itself between one repaint and the next.

One action serves both directions, and each row decides its own way: a selection of
three holding two stars stars the plain row and leaves the others alone, which is
the only reading that never destroys a star the user put somewhere else. The menu
label is the opposite decision and is made per menu, from whether *every* selected
row is starred, so `Unstar` never half-undoes a selection. Note that `toggle`
reports the state afterwards rather than whether anything moved — counting its
answer would count the stars put on and miss every star taken off, which shows up
as an un-star that does not survive a restart.

The collection is assembled from the store rather than read from a folder, so it
says its own item count, and a star whose location no longer resolves is counted
into the status line (`1 item · 1 not available`) rather than dropped silently — a
share that is simply not mounted right now is not a lost star, and a row that says
it is unavailable says something truer than no row at all. An empty collection
shows a banner instead of an empty view, which is the difference between "nothing
starred yet" and a window that looks broken. Deleting rows takes their stars with
them: an operation reports the locations it destroyed, and the store forgets them,
so the collection never accumulates rows that can only ever be dead.

The badge itself is an `Image` over the row's icon in an `Overlay`, because
gtk4-rs 0.10 has no binding for `GtkBadge` — the widget exists in GTK, the
generated binding does not. It is built invisible in both factories and shown by
painting, so a row that is not starred carries one hidden widget rather than a
layout that changes when a star is added. Being decoration, it is a
`Presentation` with `State::Hidden` tracking its visibility: without that an AT
client is handed an icon whose name is `starred-symbolic` in the middle of every
starred row, which is a container's worth of noise to say one fact the row's own
name would carry better. Both factories route it by CSS class rather than by
position, so the code that reads a row back apart to repaint it cannot mistake the
badge for the icon — the two are the same widget type, and one of them is the thing
the row is named for.

## Bookmarks

A bookmark is a location the user asked to keep a row for, and it is the one piece of
sidebar state that is not derived: the XDG folders are found, the mounts come from the
mount table, the two collections are computed, and a bookmark exists only because some-
body said it should. `ufm_core::config::Bookmarks` owns the file — load, save, `add`,
`remove`, `move_item` — and, like the star list, knows nothing about GTK. `add` dedupes
by location and appends, so the order in the file is the order the user asked for and
re-bookmarking a folder cannot produce the same row twice.

The window holds one `Bookmarks`, loaded once, for the same reason as the stars: the
sidebar is built from memory rather than from a file read per navigation. The store is
in the window rather than in the panel because a bookmark is drawn *and* written, and
the panel that draws it has no opinion about where the list came from — `set_places`
takes a `Vec<Place>` in the order it was handed and draws a heading wherever the group
changes, which is all a sidebar needs to know.

That last rule is what decides where bookmarks go. A heading is drawn on every change
of group, so bookmarks appended after the standard places would open a second `Folders`
heading below `Trash` — a sidebar that says the same word twice and files the user under
it. `Place::with_bookmarks` therefore splices them before the first row that is not a
folder, which is both the one-heading answer and the place a person looking for a
bookmark looks. The order among bookmarks is the user's; the position of the block is
not, and is the kind of thing no user has ever asked to change.

`Place` has a `Bookmark` variant rather than reusing `Folder(PathBuf)` so that a
bookmark the user renamed can say the name they gave it; the record travels with the
row, and `display()` falls back from that name to the folder's last component. The
record's `icon` field is deliberately unread: a themed icon name that turns out to be
missing renders as GTK's fallback, which in a sidebar of fifteen rows is a blank square
rather than a hint. Every bookmark row is a folder until there is a UI for choosing.

One action serves both directions, like the star, and the label is decided per menu
from whether the folder on screen is already in the store. The condition is the folder
in the title bar rather than the selection: a bookmark names a place to return to, and a
row inside a folder is not that folder. That also makes the command work where a
selection-dependent one would not — over an empty folder, or with three rows picked — and
it is why the row appears in a menu opened over the background as well as over a row.
Changing it rebuilds the sidebar wholesale, which is what `set_places` already does for
mounts, and re-highlights the current place afterwards because a rebuild drops the
selection. Nothing is re-read from disk: unlike a star, which changes what a listing
says about itself, a bookmark changes only the sidebar.

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
operation needs a thread of its own beyond the one running it. The message and
the refresh arrive together, which creates one ordering problem worth naming: a
clean read of a folder hides the banner, and an operation that touched the
folder on screen causes exactly such a read. So the message is held until after
the re-read has been set in motion and put back by the event answering it —
otherwise "Moved 1 item" would be hidden a fraction of a second after it
appeared, and the user who moved a row would get no answer at all. Navigation
clears the held message; a read failure replaces it, being the newer fact about
the folder.

An outcome also carries the locations it destroyed, and only for a discard: a
row moved out of reach can be undone, so its star stays, and a row sent to the
trash is still the same row when it comes back. What is gone for good has to be
named, because state the filesystem does not hold cannot be re-derived from it —
the star list forgets exactly those locations, and nothing else.

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

## Progress and cancellation

A copy that takes twenty seconds and says nothing for nineteen of them reads as a
freeze, and a transfer the user cannot stop is one the user cannot trust. Both are
the same problem — an operation that outlived the event that started it — so both
are solved by one piece of state: a table of running jobs, held on the main thread,
in `ui/jobs.rs`.

A job is registered before its worker is spawned, so a job exists even if the
thread never starts, and it is identified by a number rather than by a name. The
worker reports through a `Reporter` that sends `Progress` events down the same
self-pipe a directory listing uses, and the reporter drops a report unless a
hundred milliseconds have passed since the last one. That throttle is the reason
the main loop stays responsive during a transfer: a copy of ten thousand small
files would otherwise send ten thousand wake-ups to redraw a bar that can show a
change about ten times a second. The first report always goes through, so a job
appears immediately, and totals travel with each report rather than being asked
for, because a worker that knows its total already knows it at the first member.

Progress is counted in one of two units, and the difference is not cosmetic. A
delete of nine rows counts rows. A copy counts bytes when every source reported a
size, and falls back to rows when any of them did not — a share that will not say
how large a file is can still say that it copied one. Mixing the two would produce
a bar that moves and a tally that does not add up, so a job picks one unit and
reports in it, and the label reads `512 KiB of 1.0 MiB` or `4 of 9` accordingly. A
job whose total is unknown shows an idle bar and a label with no fraction, which is
honest in a way a fake percentage is not.

Cancellation is a pair, because the two halves of the codebase read cancellation
differently. GIO calls take a `GCancellable`; `ufm-archive` and the listings take
`ufm_vfs::Cancel`. A `Brake` holds both and is pulled by pulling both, so no call
site has to know which kind it is given. Each running job holds the brakes handed
to it, and one hidden action, `cancel`, pulls every brake on the table. It is
disabled while the table is empty, which is what makes the strip's visibility and
the action's sensitivity the same fact reported twice rather than two facts that
can disagree.

What the user sees is one strip above the listing, showing the oldest job: what it
is doing, which item it is on, how far along it is, and a cancel button. The strip
is hidden when the table is empty, and the table is the only reason it is shown —
there is no timer, no fade, and no state to expire. Showing the oldest job rather
than the newest is a deliberate loss of information: a user watching a queue wants
to know when the wait ends, and the count of what is behind it is in the label.

What is deliberately not there yet is a stack. One operation per window can be
undone at a time, and the reasons are in [Undoing an operation](#undoing-an-operation)
below.

## Undoing an operation

`ui/undo.rs` keeps one record per window and offers it back once. The record is
not a log of what the worker did but a description of the effect, in three
forms: rows that went to the trash, rows that changed place, and rows that were
not there before. Those three cover every operation the window offers, and each
has an inverse that does not need the original to still be around: fetch the
entry out of the trash, move the row back to the name it had, or take the new
row away again.

The trash is read rather than asked. `GIO`'s `trash:` backend belongs to `GVfs`,
which a minimal system has no reason to have installed, so the module reads the
freedesktop layout directly: one pass over a trash's `info/`, each `.trashinfo`
sidecar decoded to say where its row came from, the newest sidecar winning when
two name the same origin. The `Path=` line is percent-decoded into bytes rather
than into text, which is what lets a row whose name is not valid `UTF-8` still
come back. A row whose entry cannot be matched is not forgotten: undo names it
and says it is not in the trash, rather than reporting a row it never found.

A replacement is the interesting case, because the operation destroyed bytes. A
step that replaces a plain file moves the old file into a stash under the cache
before the paste lands, and undo moves it out again — which is why the effect is
applied first and the displaced rows restored after: the pasted row has to vacate
the name before the row that used to hold it can go back into it. A folder is
never stashed. Merging writes into the folder rather than over it, so moving it
aside would lose the rows it already held, and a tree can be as large as the one
that replaced it. What a merge cost is therefore said out loud in the operation's
own banner — `replaced box, whose old contents are gone` — and the operation
reports a partial success rather than a clean one. A copy that merged and nothing
else leaves nothing to undo, and the button stays insensitive rather than
offering to delete the copy.

One deep, and spent by the attempt: the record is taken out of the table before
the work starts, because an undo that failed partway has usually changed part of
the folder and a second attempt would be working from a description that no
longer matches the disk. Undoing is not itself undoable — the outcome it reports
carries no record — and a newer operation replaces the record, sweeping the stash
the older one was keeping. That last rule is what keeps a long session from
quietly filling the disk with files nobody asked for back.

Undo runs as a job like the operations it reverses: a row on the strip, a brake,
and the same cancel button. It never overwrites either. Every restore asks for a
free name through the same rule the paste planner uses, so an undo that would
have destroyed something the user put there in the meantime fails on that row and
says so instead of quietly replacing it.

## Search

`uflinux-searchd` owns the two Tantivy indexes and serves
`org.uflinux.Search1`. `ufm-search` holds the wire format, the index
configuration, the text extractors and the `Exec` expander — everything the
client and the daemon must agree on, so the two cannot drift. The file manager is
a client; it never opens the index. See `docs/search-indexer.md`.

## Picture previews

A folder of photographs and a folder of source files cost the same to list, so
previews are a feature that has to earn its place on a small machine. `ufm-thumb`
holds the rules — the freedesktop cache key, the freshness test, the size clamps and
the decode — and both binaries call it, so a thumbnail the desktop drew is read by
the file manager instead of decoded again.

The UI half is `ui/thumb.rs`: one worker, started on the first request rather than at
startup, and a queue capped at 64 requests, where a full queue drops the oldest. The
cap is the whole design in one number. A fast scroll through a folder of a thousand
pictures would otherwise queue a thousand decodes for cells that were on screen for
two frames, and the memory cost of a preview is not the encoded bytes but the decoded
pixbuf behind them.

Three rules keep it honest:

- **Only bytes cross the thread.** A `Pixbuf` is neither `Send` nor `Sync`, and
  neither is a `gtk::Image`, so a worker reads, decodes and re-encodes and hands back
  a `Vec<u8>`. The main thread decodes that — cheap, because the bytes are already
  the size of the cell — and paints it. Previews travel on the same wake-up pipe as
  listings; a second file descriptor waking the main loop for the same job would cost
  a syscall per picture to achieve nothing.
- **The widget carries its own token.** A recycled row is a recycled widget, and the
  answer to a question about a picture that scrolled away has to be recognisable as
  such. Each request stamps a generation number onto the widget with `set_data`; an
  answer whose token no longer matches is dropped. Overwriting the key drops the old
  value, so a widget that is bound a hundred times leaks nothing.
- **Refuse early.** A folder, a broken link, a member of an archive, a file over
  16 MiB, or anything whose MIME type does not start with `image/` never reaches the
  worker. The archive case is the one that bites: `Location::as_path()` answers with
  the container, so gating on it would preview `holiday.zip` by its own picture and
  call that a preview of the folder. The gate is `Location::is_real()`.

Previews are a preference (`thumbnails`, on by default) and a menu switch whose state
is that preference. Turning it off re-binds the rows rather than re-reading the
folder — the files on screen are the same files either way — which is the same
distinction the icon-size control makes.

## Properties

One dialog, on `<Alt>Return`, built almost entirely from the `Entry` the listing
already holds: the type, the folder it lives in, the timestamps, the mode string,
the owner and group. Nothing in that list costs a syscall beyond the `stat` the
listing already did, which is why the dialog appears the moment it is asked for
rather than after a read. The two lookups it does add are the password and group
files, via `getpwuid_r` and `getgrgid_r` with a stack buffer, because a dialog
that prints `1000` where another prints `panmourovaty (1000)` looks broken
whichever number is correct.

The mode string is rendered rather than stored, from the raw mode the local
reader already carries, and the type letter comes from the file-type bits with
the entry's kind behind them — a socket read through an archive backend has no
mode to read and still has a kind. The same `kind_label` backs the dialog's Type
row and the list view's Kind column, so the two cannot disagree about what a
thing is.

One row cannot come from the listing: a folder's total. `ufm_vfs::du::total`
walks it, on a worker, with the same temperament as the directory reader — an
explicit stack rather than recursion, a depth cap, an entry cap, cancellation
polled once per directory, symlinks counted and never followed, and a `partial`
flag set by whichever cap or error stopped the walk. The answer travels back on
the queue a listing uses, as `Event::Sized`, and the dialog registry is a
thread-local list keyed by the folder's path: an answer whose dialog has been
closed is dropped rather than reported, because closing a dialog quickly is
ordinary and not a failure.

Two wording rules carry the honesty. A `partial` walk says *at least* — a number
that is quietly a lower bound reads as a lie the moment a folder turns out to be
bigger. And the Contents row shows the child count the listing already had, so a
folder is never on screen saying it knows nothing while the walk that will know
everything is still running.

The dialog is deliberately not modal. Every other prompt in this program is,
because every other prompt is an obstacle: a name to type, a collision to
settle. A properties dialog is something being read, and reading the size of a
folder and then clicking into it is the ordinary sequence, which a modal would
turn into two. It is a plain `gtk::Window` added to the application rather than
an `ApplicationWindow`, which keeps it off the D-Bus like the other prompts, so
the window paths a test drives do not shift when a dialog opens.

## Context menus

One `Popup` per window, reused: its model is swapped for each opening and then
popped up over whatever was clicked. Because it is parented to the window, its
`win.` rows resolve against that window's own actions, so a menu row is sensitive
exactly where the header menu and the keyboard say the command is available.
There is no second copy of the sensitivity logic to fall out of agreement, and
pressing a row activates the same action its accelerator would.

Half of `ui/popup.rs` is policy and half is hit-testing, and the split is the
point. The policy is `rows_for`, a plain function from the selected entries plus
a `Scope` to a list of rows, tested without a display and shared by both
presentations. A row is left out rather than greyed when the command cannot work
here at all: `Extract` only when the selection is something we can unpack, `Open
With…` only when it is something a program could take — one file rather than a
folder, on disk or a member of an archive — and `Paste` only when the clipboard
has something for this folder, and the
folder-level rows — New Folder, Paste — only when the press happened in the
folder the window is actually in, because in a Miller column they would otherwise
name one pane and act on another. The folder-level rows sit in the row menus too
and not only over the background: the folder a row lives in is implied by the
row, and in a folder whose rows fill the view there is no empty space left to
right-click.

The hit-testing is the awkward half. `gtk_list_view_compute_item_for_point()` has
no binding in gtk4-rs 0.10, and a row's wrapper (`GtkListItem`,
`GtkColumnViewCell`) is not a widget, so `Widget::pick` cannot report one and
walking up from a picked widget cannot reach one either. So the row is identified
from *inside* it: every cell a factory builds carries a right-click gesture that
reads its own item's position when it fires — at press time, not setup time,
because cells are recycled and a position remembered at setup goes stale. Menus
over empty space come from a second gesture on the list widget itself, which
fires only when `pick()` reports nothing or the list, since a press over a row
picks a descendant of the row rather than the list.

The same kind of note applies to navigation history. `History::push` recognises a
location already visited by comparing it with the entry *under the cursor*, not
with the tip of the list. The two look equivalent and are not: after up and back,
the folder gone up from is still the tip while the cursor has moved off it, and a
push that then returns early leaves the cursor where it was — the window shows the
parent while the history believes it shows the child, and `Back` has nowhere to
go. The wrong branch only matters once something else has moved the cursor off
the tip, which is what the UI harness does on its way to checking that navigating
away closes an open menu.

## Opening a file with a program

Double-clicking asks the desktop for its default. That is usually the right
answer and sometimes the only possible one, and when it is wrong there has to be
a way to say so — which is what `ui/openwith.rs` is, behind `Open With…` in both
menus and no accelerator of its own.

The list of programs is not this program's to keep. `gio::AppInfo::all_for_type`
answers for the content type and `fallback_for_type` answers when nothing does,
which is the same pair `gio mime` reads, so the chooser cannot drift from what
the system believes about a file. The two lists overlap, so they are merged on
the desktop entry's id, and an entry that says `NoDisplay=true` is dropped: a
program that asked not to be listed in a menu asked not to be listed here either.
A content type nothing owns is said out loud rather than answered with an empty
dialog, because an empty dialog reads as a program that failed to load.

The content type comes from the `mime` the reader already worked out for the row,
and only then from a name-based guess. That order is what makes a member of an
archive open as the document it is: the archive reader knows `notes.txt` inside a
`.tar` is plain text, and a chooser that re-guessed from a path no filesystem
holds would not.

`gio::AppInfo` is neither `Send` nor `Sync`, so nothing about the chooser happens
off the main thread — the lookup, the drawing and the `launch_uris` call itself
all run where the widget tree does. The one thing that does leave the main thread
is materialising a member of an archive, since a program launched through the
desktop cannot be handed `archive://…!/notes.txt`. That is the same shape as every
other command here: a worker writes the member out and posts `Event::Materialised`
with a request id, and the pending requests are a thread-local list keyed by that
id holding a weak reference to the window that asked. An answer whose window was
closed is dropped, as it is for a folder total whose dialog went away.

Written-out members land in `paths::extracted_dir()` under a name the collision
rule would have produced anyway, and everything older than a week is swept when
the next one is written. A cache with no expiry is a leak with a folder icon: the
alternative to sweeping is either refusing to reuse what was already written or
growing without bound, and neither is what a machine with 2 GiB wants.

The chooser is a plain `gtk::Window` added to the application, like the name
prompt and the properties dialog, so it is not exported on the bus and the window
paths a test drives do not shift when it opens. Its rows are buttons whose child
is a box — `gtk::Button` has no image property in this `GTK` — so the name a
screen reader reads is set explicitly rather than inherited from a label.

## Previewing a file

`Space` over one row opens a popover that says what the row holds. The point is
the answer without a detour: opening a document means waiting for an application
to start and Properties means a dialog and a walk, and a user deciding which of
six files to open wants neither. `ui/preview.rs` is one `gtk::Popover` per
window, built closed and refilled on every opening — rebuilt rather than updated
in place, because what a preview holds depends so completely on the row that the
alternative is a widget tree with holes in it that every rule has to remember to
refill. The same key closes it again, and `autohide` gives a pointer user the
click-anywhere-else and `Escape` dismissals they already expect from every other
popover here.

The popover is enabled for exactly one row, like Properties: the question a
preview answers is about one thing, and a popover that tried to summarise six
files would be a report rather than a glance. It is enabled for a *folder* too,
which is the one place it differs from `Open With…` — a folder's size, date and
permissions are an answer, and the two actions share the one-row rule while
differing only on what a row may be.

Three parts, two of them borrowed. The header and the facts come from the
`Entry` the listing already carries, so the popover is instant on a folder of ten
thousand rows and costs no syscall beyond the `stat` the read already did. The
picture is one request to the window's thumbnail [`Pipeline`](#picture-previews),
which already owns the worker, the queue and the token table, and already drops
an answer whose widget has gone away — the ordinary fate of a preview dismissed
before its decode finished. The gate inside `Pipeline::request` is also the
preference: with picture previews switched off the request is refused and the
popover shows the facts alone, which is what the switch promises.

The text is the only part with machinery of its own, because it is the only part
that reads a file. It goes to a worker of its own and reports through the listing
queue as `Event::Preview`, since file I/O on the main thread is what this whole
program exists to avoid. The label its answer belongs to is held in a thread-local
table keyed by a token, and the window only as a key for `forget`, so an answer
that arrives after its popover was dismissed writes into a label that is in no
widget tree — one string copy, nothing else. The read stops at 64 KiB plus one,
so a 2 MiB document costs the same as a 2 KiB one, and a `NUL` byte in what was
read settles the question of whether this is text: `UTF-8` tolerates it, and every
real text file here is either `UTF-8` or something the user did not want a preview
of. A member of an archive gets the facts and no text pane; reading it would mean
opening the container on a worker while the view is browsing through it, which is
far too much work for a glance.

The picture is asked for at `ufm_thumb::MAX_SIZE`, which the cache files under the
spec's `large` band rather than the `normal` one the icons live in. That separation
is what makes the big request safe: both the popover and the icon pipeline paint
cached bytes at the size they were decoded for, so a preview reading an icon-sized
cache entry would paint a 16 px picture at thirty times its size and look like a
rendering bug rather than a cache one.

One detail took the whole of a debugging session to find. An autohide popover
anchors the grab that dismisses it on an outside click to the parent window's
keyboard focus, and a window holding none dismisses the popover the instant it
maps — not through `popdown`, which is why it survives `autohide` off, but inside
`GTK`'s own focus handling during the popup. A preview asked for from a focused
row never sees this, and neither does the context menu, which is a `PopoverMenu`
and anchors itself by focusing its first row. It appears the moment a dialog or
chooser has just closed over the window and taken its focus, which is exactly the
state the `UI` harness is in after the `Open With` section. So the action puts
focus back on whatever listing is on screen before opening, when the window holds
none — the state a real user is always in, and the reason the fix belongs to the
action rather than to the popover.

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
| `~/.config/uflinux/filemanager/bookmarks.json` | sidebar bookmarks, in the order the user put them |
| `~/.config/uflinux/filemanager/mounts.json` | mount table |
| `~/.config/uflinux/filemanager/credentials.json` | saved credentials, `0600` |
| `~/.config/uflinux/filemanager/mounts/*.conf` | per-mount credential files, `0600` |
| `~/.config/uflinux/filemanager/desktop.json` | desktop icon layout, provider only |
| `~/.config/uflinux/filemanager/starred.json` | starred locations, most recent first |
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
