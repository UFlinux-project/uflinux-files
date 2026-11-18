# The search indexer

`uflinux-searchd` is a headless GLib/GIO service that owns two Tantivy indexes
and answers questions about them over D-Bus. It is activated by the bus on the
first query and exits after `idle_exit_secs` (120 by default) with no clients, so
on a machine where nobody searches it costs nothing.

Two indexes, not one:

| Index | Contents | Why separate |
| --- | --- | --- |
| `files` | filenames, metadata, opt-in text | a file query should never rank a `.desktop` file |
| `apps` | `Applications` entries that are actually launchable | an application result must always be startable |

Both are built from the same schema (`ufm_index::schema::schema(content)`), which
is what lets one query layer serve both. The `content` flag is a real choice: an
index built without the field is a smaller index answering a strictly smaller set
of questions, and `Fields` reports the field absent so the query layer stops
offering body matches — no caller has to know which one it opened.

## The schema

| Field | Type | Why |
| --- | --- | --- |
| `path` | raw text, stored, indexed | the key; deleting a record is one term query |
| `name` | tokenised + positions, stored, fast (`raw`) | what a query matches; the fast field is for sorting |
| `name_prefix` | n-grams 2..=5, not stored | type-ahead without a prefix query over the whole vocabulary |
| `parent` | facet | scoping to a folder is one term query |
| `kind` | facet, stored | a row must know whether it is a directory without a re-stat |
| `mime` | raw text, stored, indexed | a type filter must be a query, or the total shown is the pre-filter total |
| `size` | i64 fast, stored | display and sorting |
| `modified` | date fast, stored, indexed | sorting by mtime |
| `hidden` | bool fast, stored, indexed | must be *indexed*: a term query on a non-indexed field only works with scoring off |
| `content` | text + positions, stored | extracted text, or an entry's `GenericName`/`Comment` |
| `labels` | multi-valued raw text | `Categories` for apps; filtering is a term query |

`path` has **no** fast field on purpose. A text fast field stores a second term
dictionary, which for paths would nearly double the index, and nothing sorts by
path.

Folder scoping uses a `TermQuery` on the `parent` facet rather than a
`FacetQuery`, because Tantivy 0.27 has no `FacetQuery` — and does not need one: a
facet term already carries every ancestor, so "inside `~/Documents`" is a single
term.

## Cost control

The budget is 24 MiB resident idle, 64 MiB while indexing, on a machine with 512
MiB total.

- The index is opened with `MmapDirectory::open_with_madvice(dir, Advice::Random)`.
  `Random` is the advice that lets the kernel reclaim those pages first, which is
  the right trade for an index that is read once per query. (memmap2 has no
  `Advice::DontNeed`.)
- The writer budget is `memory_budget_per_thread`, whose hard floor in Tantivy
  0.27 is 15 MB. That floor, not a chosen number, is the default.
- `LogMergePolicy` merges at 8 segments and refuses a merge that would produce a
  segment over 1,000,000 documents, because merging a folder rename's worth of tiny
  segments early costs I/O that a low-end machine would rather spend on the UI.
- One indexing worker and one merge thread. A crawl is I/O bound on a slow disk,
  not CPU bound, and each tantivy worker costs its own eagerly allocated table.
- `ReloadPolicy::Manual` with one `Searcher` refreshed explicitly by
  `IndexHandle::refresh()`. Automatic reloading keeps a second generation of
  segment readers alive.
- No background warming: the daemon would rather pay for one segment on the first
  query after a commit than hold every block in memory.
- Commits every 2 s or every 512 buffered operations, whichever comes first. The
  writer takes the directory's write lock, so it exists only while something is
  being indexed; between runs the daemon holds an index and a searcher and nothing
  else, which is what makes the idle budget reachable.
- A record is written delete-then-add on its `path` term. The path is the key a hit
  is opened with, and a second document for the same path would show the same file
  twice in the results.

## What gets indexed

Default roots: `$HOME`, depth-limited to 12, never crossing filesystems, with
`.cache`, `.git`, `.hg`, `.svn`, `node_modules`, `.Trash`, `Trash` and `.mozilla`
pruned. Hidden files are indexed and marked hidden, because leaving them out would
make `.bashrc` unfindable, which is not what "hidden" means.

Content is **off until the user turns it on for a folder**, capped at 2 MiB per
file. A file over the cap is still indexed by name. PDF and Office text come from
external `pdftotext` and `unzip -p` rather than a Rust parser, so the daemon's
resident cost does not include a PDF library it uses twice a day.

Two deliberate omissions: no stop-word list and no stemmer. Tantivy's stop-word
lists are English-only, so enabling them would silently drop words for a Khmer or
French user, and stemming a file name does more harm than good.

## Keeping up

`ufm_vfs::watch::Watcher` — inotify, coalesced, 300 ms debounce, capped at 16
watches. Past the cap, the overflowing directories are reported as needing a
rescan rather than watched, and a bounded periodic partial rescan covers them.
The descriptor count stays bounded on a home directory with hundreds of thousands
of directories, which is the case that breaks an unbounded watcher.

A change inside a watched directory is a `resync` of that directory: re-read it,
delete the records under it, add what is there now. That is cheaper than tracking
individual events per file and it cannot drift, because the directory listing is
the truth.

## Running

The process is one GLib main loop, one indexing worker and one Tantivy merge
thread. Nothing else runs in the background: a tick every 12 s is what services
both the exit clock and the periodic rescan, so an idle daemon wakes up a hundred
times an hour and does no work.

It owns `org.uflinux.Search1` with `ALLOW_REPLACEMENT`, which is what lets a
newer daemon take over during an upgrade rather than refusing to start.

The daemon takes no arguments, because a service started by the bus cannot be
given any — with one exception: `uflinux-searchd --status` does not serve at all.
It asks a running daemon the `Status` question, prints the answer, and exits.
That is what makes an installed daemon debuggable without a bus browser, and it
is the check `tests/files-ui-smoke.sh` runs when the two binaries sit side by
side. Only the first argument is read, so a stray argument later on still gets a
daemon rather than a printed status and an immediate exit.

**The exit clock.** A request re-arms it, and so does a commit — a client that
asked for a reindex and then stopped talking is still waiting for the result, and
exiting under it would lose the work it paid for. The clock is held while the
engine is busy or the queue is non-empty, so the daemon never exits mid-crawl.
When it does fire, the reason goes to stderr with both numbers, because a daemon
that vanishes from `systemctl --user status` with no explanation looks like a
crash. The decision takes an injected clock rather than reading one, so the whole
rule is tested without sleeping.

**The queue.** Tasks are deduplicated — two requests for the same directory are
one resync — and capped at 64. Past the cap the oldest `resync` is evicted, since
a resync re-reads the directory and so absorbs whatever the dropped one would
have found, while a whole-tree crawl would be lost work. A `Reindex` that cannot
be queued is refused with the current depth rather than dropped silently: the
caller asked for something and is owed an answer.

**Method cross-check.** The method name GIO reports is compared against the
`method` field inside the body, and a mismatch is refused. Without it, a client
that posts a `status` body to `SearchFiles` would get a status reply where a
result list belongs, which reads as a broken search rather than a broken client.

**Failures are reported, not signalled.** A crawl that fails sets the status note
and emits no `Changed`, because nothing was indexed: a client that re-queried
after a `Changed` it did not need would re-run a search whose results had not
moved. The note surfaces on the next `Status`, which is also where the UI shows
why a folder is not searchable.

**The periodic rescan.** inotify covers at most 16 directories, so a home
directory with more watched folders than that always has some that are not
watched. Every 300 s the daemon compares what it wanted to watch against what it
actually got, re-reads the difference, and re-checks the application directories.
That bounds how stale an unwatched folder can go rather than pretending it cannot.

## The protocol

`org.uflinux.Search1` at `/org/uflinux/Search`. One JSON string in, one JSON
string out, as `org.uflinux.Settings1` does — adding a method does not mean adding
a signature, and validation happens in one place.

| Method | Body | Answer |
| --- | --- | --- |
| `SearchFiles` | a query | `Search` |
| `SearchApps` | text, labels, order, page | `Search` |
| `Status` | `{}` | `Status` |
| `Reindex` | optional root | `Ack`, then `Progress`/`Changed` |
| `SetRoots` | absolute paths | `Ack` |

Signals: `Changed` (something was indexed, with a monotonic generation, so a client
that saw a larger one already has this one) and `Progress` (a crawl says what phase
it is in and how far it has got; `total` is 0 while unknown, which is most of the
time).

`dispatch` never touches the writer, so a query cannot block behind a crawl — it
reports `Unavailable` instead, which is retryable. `Reindex` and `SetRoots` are
refused while a crawl holds the writer, because a second crawl would fail on the
directory lock and report that instead of the truth. `SetRoots` replaces the whole
set rather than adding to it.

Three rules, all of them because the caller is another process:

1. **Unknown fields are rejected.** A client that sends `mimetype` where `mimes`
   is expected would otherwise get an unfiltered answer and no explanation, which
   looks like a bug in the search rather than a typo in the client.
2. **Every request field has a default**, so `{"method":"status"}` is complete.
3. **Both directions are bounded.** 256 KiB in, refused outright. 2 MiB out, and
   an oversized reply is *shrunk* rather than refused — snippets first, then the
   page — because the alternative is telling a user that a search for `a` failed.
   `truncated` says which.

Ranking puts an exact name match first, then BM25. `folders_first` is applied
within a page. Paging is 200 by default and 1000 at most — a larger page is a client
bug, not a large answer — and `total` is the count for the whole request, not the
page, so a scope selector and a type filter can show a real number.

## Application search, and the start menu handoff

The application index is built by `ufm_index::apps`, a hand-parsed INI subset —
not `Gio::DesktopAppInfo`, which would put a GLib type system inside the crate
whose whole argument is a 24 MiB idle footprint. It parses only what decides
whether an entry is offered and how it is found: `Type`, `Name`, `GenericName`,
`Comment`, `Keywords`, `Categories`, `NoDisplay`, `Hidden`, `OnlyShowIn`,
`NotShowIn`, `TryExec`, `Exec`, `Icon`. Only the first group counts, so a
`[Desktop Action x]`'s `Name` cannot masquerade as the application name.
`Name[locale]` is ignored on purpose: the component that knows the UI locale is
the one that displays the name.

Roots are `$XDG_DATA_HOME/applications` then each `$XDG_DATA_DIRS/applications`,
deduplicated by desktop id so a user's copy replaces the system one. An entry
that is `NoDisplay`, `Hidden`, excluded by `NotShowIn`, or whose `TryExec` is not
installed never reaches the index rather than being indexed and filtered — so
**every result is launchable**, and the caller does not have to check. The reasons
for skipping are tracked separately from read failures: a directory of desktop
files routinely holds a `mimeinfo.cache` or a `.directory`, and none of those is a
problem with the index.

An application record carries `kind: file`, `size: 0` and **no** `mime`. The type
of a `.desktop` file is not the type of what it opens, and an empty field makes a
type filter match nothing here rather than everything. `GenericName` and `Comment`
go in as body text, not as labels — "disk usage" has to find the entry whose
comment merely mentions it, and a label is a filter, not free text. `Categories`
and `Keywords` become the labels.

The resident Start menu sends `SearchApps` and `SearchFiles` for the current
query. It uses the returned desktop-entry paths to look up localized labels,
icons and launch metadata in its GIO catalog. Local application matches
supplement the ranked indexed matches. File and folder results are shown below
applications and use the default URI handler. Start searches file names and
refreshes the affected result group when the daemon emits `Changed`.

`SearchApps` answers with body matching forced on and hidden-exclusion forced off:
body text is the point of an application search, and entries under
`/usr/share/applications` are never dotfiles, so the filter clause is free.

## The UI side

`Ctrl+Shift+F` opens a bar under the header: an entry, a scope selector — current
folder, home, everywhere, applications — and a status line. The bar debounces by
120 ms, so typing a path does not queue a query per keystroke, and an empty query
with a scope is legal and means "everything here, filtered and sorted", which is
what makes the same widget serve as a folder view over a network share with
thousands of entries.

The bar does not own a connection. It asks for one on the first query and keeps it
after that, which is also what starts the daemon: there is no name watcher in
`gio` 0.21, and no pre-flight check either, because D-Bus activation makes the
call itself the check and a failed call carries the reason. A request that cannot
be answered becomes a banner naming the reason rather than an empty result list,
so an index that is unavailable is visibly different from an index with nothing to
find.

Answers arrive as one JSON body in a string, both ways. Each query is stamped with
a generation number, and an answer whose stamp is older than the current one is
dropped rather than painted — the failure this guards against is a slow
"everywhere" query landing on top of the folder the user has since opened. The
same rule covers putting the bar away, which bumps the generation so that nothing
in flight can repaint the window after the listing is already back.

`Changed` and `Progress` are subscribed once, on the connection, and kept for its
lifetime. `Progress` only drives the status line. `Changed` re-runs the current
query, and only while the bar is visible: a folder that is not on screen has no
status line to update, and re-querying on every commit while hidden would turn a
reindex into a query loop.

Results replace the listing in the flat presentations rather than becoming a
virtual folder. The rows are ordinary rows, so opening one, sorting, and the
in-folder filter all work on them without knowing where they came from. The
column panes are dropped for the duration — matches from all over the disk are not
a chain of folders — and the folder's own rows are kept aside rather than
discarded, so putting the bar away restores the window without re-reading it.

Application results arrive as `kind: file` on a `.desktop` path, and the row turns
that into an application: the mimetype is supplied as `application/x-desktop`
whatever the record says, so a desktop entry opens through the desktop's own rules
rather than in a text editor. Application activation uses desktop-entry handling for the selected path.
