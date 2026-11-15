#!/bin/sh
# End-to-end check for the `uflinux-files` window, run against a real Wayland
# compositor with no physical display.
#
# This is the part of the interface that a widget test cannot reach. The unit tests
# build a widget and ask it a question directly; they never find out whether the
# whole window comes up, whether the three view modes actually swap the widget that
# is on screen, whether the accessibility tree a screen reader would see matches the
# widgets the code built, or whether the window paints anything at all. Those are the
# questions this script answers, and they can only be answered with a compositor
# running and the real binary under it.
#
# Run it from a checkout, with the binary already built:
#
#   cargo build -p uflinux-files
#   tests/files-ui-smoke.sh target/debug/uflinux-files
#
# It brings up its own headless `labwc` and its own session bus, so it leaves nothing
# attached to a running desktop and works in a container. It needs `labwc`, `grim`,
# `gdbus` and `dbus-run-session` (all in the SDK image; `labwc`/`grim` from the wlroots
# and wayland packages, the rest from `glib-tools`), and Python 3 with the AT-SPI
# typelib for reading the tree. Any of those missing makes it exit 0 with a SKIP line
# rather than fail: a build host with no compositor is not a broken program.
#
# The assertions are on the accessibility tree — the interface a screen reader sees,
# which is the closest thing to a public contract the interface has — plus one probe
# that the window actually painted a variety of colours, so a window that is present
# in the tree but blank on screen still fails.
set -eu

BIN=${1:-target/debug/uflinux-files}
HERE=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
A11Y="$HERE/files-ui-a11y.py"

# Everything the compositor and the tree reader need, checked up front so the script
# can decline cleanly instead of failing halfway through with a message about a socket.
for tool in labwc grim gdbus dbus-run-session python3; do
    command -v "$tool" >/dev/null || { echo "SKIP ui smoke: need $tool"; exit 0; }
done
python3 -c 'import gi; gi.require_version("Atspi", "2.0")' 2>/dev/null || {
    echo "SKIP ui smoke: need the AT-SPI typelib (at-spi2-core)"
    exit 0
}
[ -x "$BIN" ] || { echo "no binary at $BIN — run cargo build -p uflinux-files" >&2; exit 2; }

# A bus of our own rather than the developer's, so a failure cannot leave a window or a
# name attached to a running session. Re-exec under a private bus when there is none,
# before anything is created: `dbus-run-session` sets its own `XDG_RUNTIME_DIR`, and the
# one this script makes for the Wayland socket has to be the one that survives.
if [ -z "${DBUS_SESSION_BUS_ADDRESS:-}" ]; then
    exec dbus-run-session "$0" "$BIN"
fi

ROOT=$(mktemp -d)
trap 'rm -rf "$ROOT"' EXIT INT TERM
# The runtime directory is where `labwc` puts the Wayland socket and where the app looks
# for it. Overriding the one the bus provided is deliberate: both halves read the same
# variable, so they agree without anything being passed along.
XDG_RUNTIME_DIR=$ROOT/xdg
mkdir -p "$XDG_RUNTIME_DIR"
chmod 700 "$XDG_RUNTIME_DIR"
export XDG_RUNTIME_DIR
WAYLAND_DISPLAY=wayland-0
WLR_BACKENDS=headless
WLR_RENDERER=pixman
export WAYLAND_DISPLAY WLR_BACKENDS WLR_RENDERER

# A home of our own, so the sidebar and the indexer see a fixture rather than the
# machine's real files. The tree under it is what the window is pointed at.
HOME=$ROOT/home
XDG_DATA_HOME=$HOME/.local/share
mkdir -p "$HOME" "$XDG_DATA_HOME" "$ROOT/tree/alpha/one" "$ROOT/tree/alpha/two" \
    "$ROOT/tree/one"
# A folder called `one` beside the one the window opens on, holding a file of its
# own. The collision checks below copy `alpha/one` onto `tree/one`, which is the
# only way to reach the conflict prompt from a fixture; what each side holds is
# what lets those checks tell a merge from an overwrite. The window still opens on
# a folder of two entries, so the count the status line has to say is unchanged.
printf kept >"$ROOT/tree/one/keep.txt"
printf new >"$ROOT/tree/alpha/one/arrived.txt"
# The star list goes under `$XDG_CONFIG_HOME`, and naming it here rather than
# leaving it to default from `HOME` is what makes the file's path below a fact of
# the fixture rather than a guess about the default. It points inside the home
# this script already threw away, so starring leaves nothing behind.
XDG_CONFIG_HOME=$HOME/.config
export HOME XDG_DATA_HOME XDG_CONFIG_HOME
# The path the window opens on. Two entries, so the status line has a number to say.
TARGET=$ROOT/tree/alpha
# A desktop for the desktop icons to sit on. The provider reads `$HOME/Desktop` when
# `user-dirs.dirs` says nothing, which it does here, so naming the folder is enough.
# The two names are ones nothing else in the fixture uses, so a tree match on them can
# only be the desktop answering.
DESKTOP_DIR=$HOME/Desktop
mkdir -p "$DESKTOP_DIR"
: >"$DESKTOP_DIR/desk-one.txt"
: >"$DESKTOP_DIR/desk-two.txt"

# One network share the user connected to in some earlier session, written where
# the mount table lives so the application finds a share already listed before it
# opens a window. `nfs://nas/videos2024`: the id is the share's slug, which is its
# host, the last part of its path, and six digits of its hash. The mount point in
# the record is a lie on purpose — the loader replaces it with one under
# `$UFLINUX_FILES_MOUNT_ROOT`, and that folder is never created, so the kernel
# reports no mount there and the row is drawn for a share that is listed without
# being connected. Naming the mount root rather than leaving it to default from
# `$XDG_RUNTIME_DIR` keeps the fixture out of the session's real runtime directory.
MOUNT_FILE=$XDG_CONFIG_HOME/uflinux/filemanager/mounts.json
MOUNT_ID=nas-videos2024-a71bee
UFLINUX_FILES_MOUNT_ROOT=$ROOT/mnt
export UFLINUX_FILES_MOUNT_ROOT
mkdir -p "$XDG_CONFIG_HOME/uflinux/filemanager"
cat >"$MOUNT_FILE" <<EOF
{"version":1,"mounts":[{"id":"$MOUNT_ID","share":{"protocol":"nfs","host":"nas",
"port":null,"user":null,"tls":false,"resource":"/videos2024","options":[]},
"mountpoint":"/nowhere","autostart":false}]}
EOF

# A real container to unpack, in a folder of its own. Its own folder is what keeps
# the checks above about how many entries the window opens on honest, and a real
# archive rather than a file named like one is what lets the extract check tell a
# command that ran from one that was merely sensitive.
PACK=$ROOT/pack
mkdir -p "$PACK"
cp "$HERE/archive-fixtures/nested.tar" "$PACK/bundle.tar"

# labwc on the headless backend, pixman renderer: no GPU, no DRM, no X. The Xwayland
# complaints it prints are noise and are not treated as failure.
labwc >"$ROOT/labwc.log" 2>&1 &
LAB=$!
# Wait for the socket rather than a fixed delay; a slow machine is not a broken one.
i=0
while [ "$i" -lt 100 ] && [ ! -S "$XDG_RUNTIME_DIR/wayland-0" ]; do
    i=$((i + 1))
    sleep 0.1
done
[ -S "$XDG_RUNTIME_DIR/wayland-0" ] || {
    echo "FAIL labwc never created a Wayland socket"
    sed 's/^/     labwc: /' "$ROOT/labwc.log" | grep -iv xwayland | head -10 || :
    exit 1
}

APP_LOG=$ROOT/app.log
# The primary instance, opened on the fixture. Later plain invocations reach this
# process through its `open` signal and add windows to it; `--new-window` is separate.
"$BIN" "file://$TARGET" >"$APP_LOG" 2>&1 &
APP=$!

WIN=/org/uflinux/FileManager/window/1

# One activation of a window action. The target is a GVariant tuple of targets and a
# tuple of hints; a parameterless action takes two empty tuples, a string action takes
# `[<'value'>]`.
act() {
    gdbus call --session --dest org.uflinux.FileManager --object-path "$WIN" \
        --method org.gtk.Actions.Activate "$@" >/dev/null 2>&1 || :
}

# The same, on a window other than the first. GTK numbers the windows it exports,
# so a second window is `window/2`; the archive check runs in one of those because
# the window opened on the command line is standing on a plain folder.
act_on() {
    path=$1
    shift
    gdbus call --session --dest org.uflinux.FileManager --object-path "$path" \
        --method org.gtk.Actions.Activate "$@" >/dev/null 2>&1 || :
}

# The tree, tab-separated `role<TAB>name<TAB>states`, refreshed on demand.
DUMP=
refresh() { DUMP=$(python3 "$A11Y" dump 2>/dev/null || true); }

# Wait until the tree contains a line whose name field is NAME, refreshing as it goes.
wait_named() {
    i=0
    while [ "$i" -lt 100 ]; do
        refresh
        if printf '%s\n' "$DUMP" | grep -qF "$(printf '\t%s\t' "$1")"; then return 0; fi
        i=$((i + 1))
        sleep 0.2
    done
    return 1
}

FAILED=0
ok() { echo "ok   $1"; }
fail() {
    echo "FAIL $1"
    FAILED=$((FAILED + 1))
}

# A node whose name field is NAME exists. Matching on the tab-delimited name rather than
# a whole line keeps the check independent of the role, which GTK renames more often
# than it renames a label.
has_named() {
    if wait_named "$1"; then ok "$2"; else fail "$2 (no node named '$1')"; fi
}

# A window whose title is NAME. Stronger than a name match anywhere in the tree: the
# sidebar is full of the same words a window is titled with, so "is there a node called
# Computer" says nothing about whether a Computer window was opened.
has_frame() {
    i=0
    while [ "$i" -lt 50 ]; do
        refresh
        if printf '%s\n' "$DUMP" | grep -qF "$(printf 'frame\t%s\t' "$1")"; then
            ok "$2"
            return 0
        fi
        i=$((i + 1))
        sleep 0.2
    done
    fail "$2 (no window titled '$1')"
}

# A window whose title is NAME is gone. The other half of `has_frame`, needed
# when a check below counts windows and an earlier one has to stop counting.
wait_no_frame() {
    i=0
    while [ "$i" -lt 50 ]; do
        refresh
        if ! printf '%s\n' "$DUMP" | grep -qF "$(printf 'frame\t%s\t' "$1")"; then
            return 0
        fi
        i=$((i + 1))
        sleep 0.2
    done
    return 1
}

# The states field of the first node named NAME, or empty.
states_of() {
    printf '%s\n' "$DUMP" | grep -F "$(printf '\t%s\t' "$1")" | head -1 | cut -f3
}

# --- the window comes up on the location it was given -------------------------------
if wait_named "$TARGET"; then
    ok "a window opens on the location named on the command line"
else
    fail "a window opens on the location named on the command line"
fi

# The sidebar is the same everywhere, so it is checked once, before the views change.
for place in Home "My Computer" Computer Recent Starred Trash; do
    has_named "$place" "the sidebar offers $place"
done

# --- the header buttons say where you can go ----------------------------------------
# At the start of a history there is nowhere to go back to, and the buttons are there
# but insensitive; up and reload always apply to a real folder, so they are sensitive.
if wait_named "Back"; then
    case $(states_of "Back") in
    *sensitive*) fail "Back is present but insensitive at the start of history" ;;
    *) ok "Back is present but insensitive at the start of history" ;;
    esac
else
    fail "Back is present but insensitive at the start of history"
fi
if wait_named "Up one folder"; then
    case $(states_of "Up one folder") in
    *sensitive*) ok "Up one folder is sensitive on a real folder" ;;
    *) fail "Up one folder is sensitive on a real folder (states: $(states_of "Up one folder"))" ;;
    esac
else
    fail "Up one folder is sensitive on a real folder"
fi

# --- the grid, which is the mode a window opens in ----------------------------------
has_named "one" "the grid shows the folder's entries"
has_named "2 items" "the status line counts the entries"

# --- the list view ------------------------------------------------------------------
act mode "[<'list'>]" "[]"
i=0
while [ "$i" -lt 50 ] && ! printf '%s\n' "$DUMP" | grep -qF "$(printf 'tree table\t')"; do
    refresh
    i=$((i + 1))
    sleep 0.2
done
if printf '%s\n' "$DUMP" | grep -qF "$(printf 'tree table\t')"; then
    ok "the list view replaces the grid with a tree table"
else
    fail "the list view replaces the grid with a tree table"
fi
refresh
if printf '%s\n' "$DUMP" | grep -qF "$(printf '\tName Size Type Modified\t')"; then
    ok "the list view has the four column headings"
else
    fail "the list view has the four column headings"
fi

# --- the Miller columns -------------------------------------------------------------
# Opening on `.../tree/alpha` and switching to columns renders one pane per path level,
# so the ancestor chain appears without any navigation.
act mode "[<'columns'>]" "[]"
i=0
panes=0
while [ "$i" -lt 50 ]; do
    refresh
    panes=$(printf '%s\n' "$DUMP" | grep -cF "$(printf 'scroll pane\t')" || true)
    [ "${panes:-0}" -ge 2 ] && break
    i=$((i + 1))
    sleep 0.2
done
if [ "${panes:-0}" -ge 2 ]; then
    ok "the columns view shows one pane per path level ($panes panes)"
else
    fail "the columns view shows one pane per path level (got ${panes:-0})"
fi
# The pane for the folder in the title bar has to be full the moment it appears.
# Building a chain leaves the deepest pane to a listing the window is expected to
# be reading, which is true while navigating and false when the user only changed
# how the folder already on screen is drawn — so the rows have to be handed over,
# and a column that painted empty until the next folder was opened would pass the
# check above and fail this one.
refresh
if printf '%s\n' "$DUMP" | grep -qF "$(printf '\tone\t')"; then
    ok "the deepest column holds the folder's own entries at once"
else
    fail "the deepest column holds the folder's own entries at once"
fi

# --- the file operations -------------------------------------------------------------
# The actions are wired to real work: a prompt, a worker, and a reload. Reading the
# answer straight off the fixture on disk is what makes this worth testing here rather
# than only in a unit test, because a unit test cannot show that the window noticed.
# `Describe` reports an action's enabled state, which is how a disabled action is told
# apart from one that was never registered.
describe() {
    gdbus call --session --dest org.uflinux.FileManager --object-path "$WIN" \
        --method org.gtk.Actions.Describe "$1" 2>/dev/null || true
}
# The same, on another window. Some commands are only available somewhere other
# than where the first window is standing.
describe_on() {
    gdbus call --session --dest org.uflinux.FileManager --object-path "$1" \
        --method org.gtk.Actions.Describe "$2" 2>/dev/null || true
}
# Whether a path is a window that is still there, rather than one that has been
# closed. GTK exports one object path per window and does not reuse a number, so a
# test that opens a window of its own has to find out which one it got. `Describe`
# answers for a live window and fails for a closed one, which is what makes a path
# safe to act on later: an action activated on a window that is gone is answered
# with a D-Bus error that `act_on` swallows, and the test would go on asserting
# about a change that was never made.
alive() {
    [ -n "$(describe_on "$1" star)" ]
}
if describe paste | grep -qF '(false'; then
    ok "Paste is disabled while the clipboard is empty"
else
    fail "Paste is disabled while the clipboard is empty ($(describe paste))"
fi
# Cancel belongs to the job strip, and the strip is hidden when nothing runs. An
# action that is registered but disabled and one that was never registered are
# told apart here by `Describe` answering at all; what the enabled state is
# supposed to become is the job's own test, which can start a transfer and watch.
if describe cancel | grep -qF '(false'; then
    ok "Cancel is disabled while nothing runs"
else
    fail "Cancel is disabled while nothing runs ($(describe cancel))"
fi

# --- undoing a discard ----------------------------------------------------------
# The whole feature end to end: a row thrown away, the trash it lands in, the
# sidecar that names where it came from, and the button that puts it back. The
# trash is checked as the freedesktop layout rather than as a `trash:` URI
# because reading the directory is what the undo actually does — `GIO`'s trash
# URI belongs to `GVfs`, which this container has no more than a minimal
# UFLinux install does, so an implementation built on the URI would pass a
# check like this only by accident of the image.
if describe undo | grep -qF '(false'; then
    ok "Undo is disabled before anything has been done"
else
    fail "Undo is disabled before anything has been done ($(describe undo))"
fi
python3 "$A11Y" select two >/dev/null 2>&1 || :
act trash "[]" "[]"
i=0
while [ "$i" -lt 50 ] && [ -e "$TARGET/two" ]; do
    i=$((i + 1))
    sleep 0.2
done
if [ ! -e "$TARGET/two" ]; then
    ok "Discard moves the selected row out of the folder"
else
    fail "Discard moves the selected row out of the folder ($TARGET/two is still there)"
fi
TRASHED=$XDG_DATA_HOME/Trash
if [ -d "$TRASHED/files/two" ] && [ -f "$TRASHED/info/two.trashinfo" ]; then
    ok "the row lands in the trash beside a sidecar naming its origin"
else
    fail "the row lands in the trash beside a sidecar naming its origin (files: $([ -d "$TRASHED/files/two" ] && echo y || echo n), info: $([ -f "$TRASHED/info/two.trashinfo" ] && echo y || echo n))"
fi
if describe undo | grep -qF '(true'; then
    ok "an operation makes Undo sensitive"
else
    fail "an operation makes Undo sensitive ($(describe undo))"
fi
act undo "[]" "[]"
i=0
while [ "$i" -lt 50 ] && [ ! -e "$TARGET/two" ]; do
    i=$((i + 1))
    sleep 0.2
done
if [ -e "$TARGET/two" ]; then
    ok "Undo moves the row back out of the trash"
else
    fail "Undo moves the row back out of the trash (no $TARGET/two)"
fi
if [ ! -e "$TRASHED/files/two" ] && [ ! -e "$TRASHED/info/two.trashinfo" ]; then
    ok "the entry leaves the trash with the row, rather than waiting to be emptied"
else
    fail "the entry leaves the trash with the row (files: $([ -e "$TRASHED/files/two" ] && echo y || echo n), info: $([ -e "$TRASHED/info/two.trashinfo" ] && echo y || echo n))"
fi
i=0
while [ "$i" -lt 50 ]; do
    refresh
    printf '%s\n' "$DUMP" | grep -qF "Moved back" && break
    i=$((i + 1))
    sleep 0.2
done
if printf '%s\n' "$DUMP" | grep -qF "Moved back"; then
    ok "the window says what it moved back"
else
    fail "the window says what it moved back"
fi
# One deep rather than a stack: the record is spent by the attempt, whether or
# not it worked, so the button goes insensitive the moment it is pressed.
if describe undo | grep -qF '(false'; then
    ok "Undo spends itself, rather than offering the same undo twice"
else
    fail "Undo spends itself, rather than offering the same undo twice ($(describe undo))"
fi

# The prompt is a window of its own, so it shows up as a frame. Its entry arrives with
# a suggested name already in it, which the check on the created folder confirms.
act new-folder "[]" "[]"
i=0
while [ "$i" -lt 50 ] && ! printf '%s\n' "$DUMP" | grep -qF "$(printf 'frame\tNew Folder\t')"; do
    refresh
    i=$((i + 1))
    sleep 0.2
done
if printf '%s\n' "$DUMP" | grep -qF "$(printf 'frame\tNew Folder\t')"; then
    ok "New Folder opens a prompt for the name"
else
    fail "New Folder opens a prompt for the name"
fi

# Pressing the button goes through the accessibility tree, which is the same controller
# path a click takes. The name it suggests is the one the module computes, so the folder
# appearing under that name checks the suggestion and the worker in one assertion.
python3 "$A11Y" press Create >/dev/null 2>&1 || true
MADE="$TARGET/Untitled folder"
i=0
while [ "$i" -lt 50 ] && [ ! -d "$MADE" ]; do
    i=$((i + 1))
    sleep 0.2
done
if [ -d "$MADE" ]; then
    ok "the prompt creates the folder it suggested"
else
    fail "the prompt creates the folder it suggested (no $MADE)"
fi
# The window notices through its own watcher, which costs a re-read — and after
# the undo above there is a re-read already in flight, whose answer may land
# before the new folder exists. So the check waits for the row rather than
# looking once: the listing does pick the folder up on its own, and on a loaded
# machine it simply takes a moment longer than one glance.
i=0
while [ "$i" -lt 50 ] && ! printf '%s\n' "$DUMP" | grep -qF "$(printf '\tUntitled folder\t')"; do
    refresh
    i=$((i + 1))
    sleep 0.2
done
if printf '%s\n' "$DUMP" | grep -qF "$(printf '\tUntitled folder\t')"; then
    ok "the listing picks the new folder up on its own"
else
    fail "the listing picks the new folder up on its own"
fi
if printf '%s\n' "$DUMP" | grep -qF "$(printf 'frame\tNew Folder\t')"; then
    fail "answering the prompt closes it"
else
    ok "answering the prompt closes it"
fi

# --- pasting onto a name that is taken ------------------------------------------------
# The collision prompt, reached the way a user reaches it: copy a folder, go up one
# level, and paste it where a folder of that name already lives. Two things make this
# harder to drive than the name prompt above, and both are worth knowing before
# adding to it.
#
# The first is the selection. There is no click to give, and a row answers `Action`
# and `Component` but not `Selection` — selection belongs to the container — so the
# probe's `select` command finds the row by name and asks the grid holding it. That
# drives the application's real selection, which matters because `Describe copy`
# reports the action enabled whether or not anything is selected: sensitivity cannot
# be the assertion here, and the effect has to be.
#
# The second is that activating `paste` does not return while the prompt is open; the
# prompt runs its own main-context loop, which is how a dialog used to work and what
# keeps the rest of the interface repainting. So the activation is backgrounded and
# the prompt is answered from here. The job's pid is kept rather than waiting for
# every background job, because `labwc` and the application are background jobs too
# and outlive this section.
if SELECT_RESULT=$(python3 "$A11Y" select one 2>&1); then
    ok "a row can be selected through the accessibility tree"
else
    fail "a row can be selected through the accessibility tree ($SELECT_RESULT)"
fi
act copy "[]" "[]"
act up "[]" "[]"
sleep 1
# Skip first, so the tree the replace run then changes is still the untouched one.
act paste "[]" "[]" &
PASTE=$!
has_frame "Replace this folder?" "pasting onto a taken name asks rather than renaming"
refresh
if printf '%s\n' "$DUMP" | grep -qF "is already in"; then
    ok "the prompt names the folder the two rows would land in"
else
    fail "the prompt names the folder the two rows would land in"
fi
python3 "$A11Y" press Skip >/dev/null 2>&1 || true
wait "$PASTE" || :
if [ ! -e "$ROOT/tree/one/arrived.txt" ]; then
    ok "answering Skip leaves the folder already there alone"
else
    fail "answering Skip leaves the folder already there alone (it merged anyway)"
fi
refresh
if printf '%s\n' "$DUMP" | grep -qF "$(printf 'frame\tReplace this folder?\t')"; then
    fail "answering the collision prompt closes it"
else
    ok "answering the collision prompt closes it"
fi

# The same question again, this time answered the other way. The clipboard survives a
# skipped paste for the same reason it survives a completed one: a copy can be pasted
# more than once. What is asserted now is the shape of a merged folder rather than
# just that something was written — the incoming file has to arrive *and* the file
# that was already there has to still be there, which is the difference between
# merging and replacing a folder wholesale.
act paste "[]" "[]" &
PASTE=$!
has_frame "Replace this folder?" "a skipped paste leaves the clipboard for another one"
python3 "$A11Y" press Replace >/dev/null 2>&1 || true
wait "$PASTE" || :
i=0
while [ "$i" -lt 50 ] && [ ! -f "$ROOT/tree/one/arrived.txt" ]; do
    i=$((i + 1))
    sleep 0.2
done
if [ -f "$ROOT/tree/one/arrived.txt" ] && [ -f "$ROOT/tree/one/keep.txt" ]; then
    ok "answering Replace merges into the folder already there"
else
    fail "answering Replace merges into the folder already there (arrived: $([ -f "$ROOT/tree/one/arrived.txt" ] && echo y || echo n), kept: $([ -f "$ROOT/tree/one/keep.txt" ] && echo y || echo n))"
fi

# Back to the folder the script opened the window on, which the checks below assume.
act back "[]" "[]"
has_frame "$TARGET" "Back returns to the folder the window came from"

# --- extracting an archive -----------------------------------------------------
# The one write the archive browser offers. Checked against a real container
# rather than a file named like one, because the assertion that matters is the
# folder the command leaves on disk: a menu row that merely became sensitive would
# pass a check that only looks at the accessibility tree. The command runs in a
# window opened on the archive itself, which is also where a user would ask for
# it — standing inside the thing to be unpacked.
if describe extract | grep -qF '(false'; then
    ok "Extract is disabled on an ordinary folder"
else
    fail "Extract is disabled on an ordinary folder ($(describe extract))"
fi
"$BIN" "archive://$PACK/bundle.tar!/" >"$ROOT/archive.log" 2>&1 ||
    fail "opening an archive exits non-zero"
i=0
while [ "$i" -lt 50 ] && ! printf '%s\n' "$DUMP" | grep -qF "$(printf 'frame\t%s' "$PACK")"; do
    refresh
    i=$((i + 1))
    sleep 0.2
done
if printf '%s\n' "$DUMP" | grep -qF "$(printf 'frame\t%s' "$PACK")"; then
    ok "an archive opens as a folder of its own"
else
    fail "an archive opens as a folder of its own (no frame for $PACK)"
fi
ARCH=/org/uflinux/FileManager/window/2
if describe_on "$ARCH" extract | grep -qF '(true'; then
    ok "Extract is sensitive while standing inside an archive"
else
    fail "Extract is sensitive while standing inside an archive ($(describe_on "$ARCH" extract))"
fi
act_on "$ARCH" extract "[]" "[]"
i=0
while [ "$i" -lt 100 ] && [ ! -f "$PACK/bundle/docs/readme.md" ]; do
    i=$((i + 1))
    sleep 0.2
done
if [ -f "$PACK/bundle/docs/readme.md" ]; then
    ok "Extract unpacks the container into a folder named after it"
else
    fail "Extract unpacks the container into a folder named after it (no $PACK/bundle)"
fi
refresh
if printf '%s\n' "$DUMP" | grep -qF "Extracted"; then
    ok "the window says what it extracted, in the folder it landed in"
else
    fail "the window says what it extracted, in the folder it landed in"
fi

# --- a change made by another program appears on its own -----------------------------
# The first window is still standing on `$TARGET`, so a file written there by the shell
# is a change this program did not make and cannot have anticipated. Nothing here
# touches the window: no reload, no navigation. The watcher has a debounce, so the
# wait is generous; what is being asserted is that the listing notices at all, which
# is the difference between a live folder and a photograph of one.
touch "$TARGET/live.txt"
if wait_named "live.txt"; then
    ok "a file created outside the program appears in the listing"
else
    fail "a file created outside the program appears in the listing (no live.txt)"
fi
rm -f "$TARGET/live.txt"
i=0
while [ "$i" -lt 50 ] && printf '%s\n' "$DUMP" | grep -qF "$(printf '\tlive.txt\t')"; do
    refresh
    i=$((i + 1))
    sleep 0.2
done
if printf '%s\n' "$DUMP" | grep -qF "$(printf '\tlive.txt\t')"; then
    fail "a file deleted outside the program disappears from the listing"
else
    ok "a file deleted outside the program disappears from the listing"
fi

# --- picture previews --------------------------------------------------------
# The one feature whose machinery is invisible from the accessibility tree: a row
# that got a picture and a row that fell back to the themed icon are the same node
# with the same name, because an image carries no label. So the assertion is made
# on the cache directory instead, which is where the freedesktop thumbnail spec
# says the answer has to land and what makes a preview outlive this window.
#
# Whether a decode is possible at all depends on the machine and not on this
# program: `GdkPixbuf` hands every format but `XPM` to a loader that runs out of
# process inside a sandbox, and a container that cannot create a pty cannot start
# one. The probe writes the picture and reports whether it can read one back, so a
# host with no loader reports a skip rather than a failure it did not cause.
probe=$(python3 "$HERE/files-ui-pixbuf.py" "$TARGET/photo.png" 2>/dev/null || echo no)
if [ "$probe" != ok ]; then
    echo "skip picture previews: this machine cannot decode an image"
else
    # The row arrives through the watcher, and the picture is encoded on a worker
    # afterwards, so both are waited for rather than slept on once.
    i=0
    cached=
    while [ "$i" -lt 100 ] && [ -z "$cached" ]; do
        cached=$(find "$HOME/.cache/thumbnails" -name '*.png' -size +0 2>/dev/null |
            head -1)
        [ -n "$cached" ] || {
            i=$((i + 1))
            sleep 0.2
        }
    done
    if [ -n "$cached" ]; then
        ok "a picture in the folder is decoded and written to the thumbnail cache"
    else
        fail "a picture in the folder is decoded and written to the thumbnail cache"
    fi

    # The menu switch. Its state is the preference, so `Describe` is the honest
    # place to look: a checkmark that disagreed with what the window does would be
    # worse than no switch at all. Turning it off and on again also exercises the
    # re-bind, whose mistakes would surface as a GLib critical below.
    if describe thumbnails | grep -qF '[<true>]'; then
        ok "picture previews are on to begin with"
    else
        fail "picture previews are on to begin with ($(describe thumbnails))"
    fi
    act thumbnails "[<false>]" "[]"
    if describe thumbnails | grep -qF '[<false>]'; then
        ok "the switch turns previews off and remembers it in its state"
    else
        fail "the switch turns previews off ($(describe thumbnails))"
    fi
    act thumbnails "[<true>]" "[]"
    if describe thumbnails | grep -qF '[<true>]'; then
        ok "the switch turns previews back on"
    else
        fail "the switch turns previews back on ($(describe thumbnails))"
    fi
fi

# --- the properties dialog ---------------------------------------------------------
# The dialog is built from what the listing already knows and then rewritten by a
# walk on a worker, so the interesting assertions are the two halves separately:
# that the facts available at once are on screen, and that the folder total arrives
# afterwards rather than being promised and never delivered. The total itself is
# not asserted digit by digit — the fixture grows a file here and loses one there
# as the checks above run — so the check is that the row stops saying it is
# counting and says what a finished walk says instead.
#
# One row is selected first, because the action is enabled for exactly one target
# and `Describe` is the only way to see that from outside: a menu row that was
# always sensitive would let a dialog open on nothing. The rows are matched on
# their key labels — `Location`, `Permissions` — because those words appear
# nowhere else in the interface, unlike the kind label, which the list view also
# prints per row whenever that mode is showing.
if SELECT_RESULT=$(python3 "$A11Y" select one 2>&1); [ "$?" -ne 0 ]; then
    fail "a row can be selected for the properties check ($SELECT_RESULT)"
elif describe properties | grep -qF '(true'; then
    ok "Properties is sensitive with one row selected"
else
    fail "Properties is sensitive with one row selected ($(describe properties))"
fi
act properties "[]" "[]"
has_frame "Properties" "Properties opens a dialog of its own"
has_named "Location" "the dialog says where the thing lives"
has_named "Permissions" "the dialog reports the mode bits"
# The mode string itself, read off the fixture rather than hard-coded: the umask
# of whoever ran the script decides whether a folder is 755 or 750, and a check
# that guessed would fail for the wrong reason. Matching a `label` node rather
# than any node keeps the assertion inside the dialog, because the window's own
# frame is titled with the same path the Location row prints.
MODE=$(stat -c '%A' "$TARGET/one" 2>/dev/null || echo '')
if [ -n "$MODE" ] && wait_named "$MODE"; then
    ok "the dialog prints the mode bits the folder actually has"
else
    fail "the dialog prints the mode bits the folder actually has (no $MODE)"
fi
# The walk finishes on a fixture this small in milliseconds, but it runs on a
# worker and the row belongs to the UI thread, so it is waited for. The pattern
# is built with `printf` because a shell single-quoted `\t` is two characters and
# `grep -E` would read it as a `t`; the shape to look for is a size, the word
# `in`, a count, and the word `file`, which no other label in the interface says.
total=$(printf '^label\t[^\t]* in [0-9][^\t]* file')
i=0
while [ "$i" -lt 50 ] && ! printf '%s\n' "$DUMP" | grep -qE "$total"; do
    refresh
    i=$((i + 1))
    sleep 0.2
done
if printf '%s\n' "$DUMP" | grep -qE "$total"; then
    ok "the folder total replaces the counting placeholder"
else
    fail "the folder total replaces the counting placeholder"
fi
# The dialog's own Close, pressed inside its frame: a folder window's titlebar
# carries a button labelled `Close` too, and an unscoped press would reach that
# one first and take the window down with the dialog.
python3 "$A11Y" press Close Properties >/dev/null 2>&1 || true
i=0
while [ "$i" -lt 50 ] && printf '%s\n' "$DUMP" | grep -qF "$(printf 'frame\tProperties\t')"; do
    refresh
    i=$((i + 1))
    sleep 0.2
done
if printf '%s\n' "$DUMP" | grep -qF "$(printf 'frame\tProperties\t')"; then
    fail "Close dismisses the dialog"
else
    ok "Close dismisses the dialog"
fi

# --- the context menu ----------------------------------------------------------------
# The menu is built from the selection, so the row selected for the properties check
# above is the one it is asked about. What is worth asserting is not that a popover
# exists but that the rows drawn are the ones the policy says belong to one selected
# folder, and that pressing one runs the window's own command rather than a private
# copy of it — which is what the header menu and the keyboard already promise.
#
# The menu is opened by its action rather than by a right-click because the harness has
# no pointer to inject, and the action is the honest way in: it is what the Menu key
# does, it reads the same selection and the same folder, and it points the menu at the
# focused row instead of at a cursor.
#
# Rows are identified by accelerator, not by label. `GTK` 4.22 rebuilt `GtkPopoverMenu`
# on a widget tree, and the rows it produces answer `menu item` with an empty accessible
# name — the label is drawn by a child that is not itself exposed — so a test cannot
# reach a row by the text on it. What a row does carry is the chord installed for the
# action it names, which is a handle on both the row and the accelerator table — the two
# together are what a keyboard user sees. The `-` is the Open row, which has no
# accelerator: plain `Return` already activates a row, so a chord for the same command
# would be a second name for one thing.
act context-menu "[]" "[]"
if ! python3 "$A11Y" menu >/dev/null 2>&1; then
    fail "the context menu opens over the selected row"
elif [ -z "$(python3 "$A11Y" menu 2>/dev/null)" ]; then
    fail "the context menu opens over the selected row (no rows in it)"
else
    ok "the context menu opens over the selected row"
fi
# The whole row set, compared as a set rather than row by row: a menu that grew a row
# nobody asked for is as much a change as one that lost a row, and the comparison says
# so in one line. `Control+V` is in it because the copy that fed the collision prompt is
# still on the clipboard — a copy can be pasted more than once — and `Control+E` is not,
# because the window is standing on an ordinary folder. The two together are the point of
# the assertion: the rows track what the window can actually do right now, which is also
# what keeps the header menu honest.
want=$(printf '%s\n' - Shift+Control+N Control+T Control+D F2 Delete Shift+Delete Control+C Control+X Control+V Alt+Enter | sort)
got=$(python3 "$A11Y" menu 2>/dev/null | sort)
if [ "$got" = "$want" ]; then
    ok "the menu holds exactly the rows for one folder, each carrying its chord"
else
    fail "the menu holds exactly the rows for one folder, each carrying its chord (got $(printf '%s' "$got" | tr '\n' ' '))"
fi
# The negative half, spelled out because it is the half a menu gets wrong: the window is
# standing on an ordinary folder, so there is nothing to unpack and the row must not be
# offered at all rather than offered and inert.
if printf '%s\n' "$got" | grep -qF 'Control+E'; then
    fail "the menu leaves out what the selection cannot do (Extract offered)"
else
    ok "the menu leaves out what the selection cannot do"
fi
# Pressing a row goes through the accessibility tree, so the assertion covers the whole
# path: the model button, the `win.` action it names, and the prompt the action opens.
# Cancelling rather than creating keeps the fixture as the checks below expect it.
python3 "$A11Y" menu-press Shift+Control+N >/dev/null 2>&1 || true
if wait_named "Folder name:"; then
    ok "pressing a menu row runs the command it names"
else
    fail "pressing a menu row runs the command it names (no prompt)"
fi
python3 "$A11Y" press Cancel "New Folder" >/dev/null 2>&1 || true
i=0
while [ "$i" -lt 50 ] && printf '%s\n' "$DUMP" | grep -qF "$(printf 'frame\tNew Folder\t')"; do
    refresh
    i=$((i + 1))
    sleep 0.2
done
# An open menu over a folder that is no longer on screen is a menu of commands for
# somewhere the user has left, so navigation closes it. The menu is the only node of its
# role in the tree — a closed popover has no item widgets, which is what made the check
# above able to say the menu had opened at all — so counting them is the assertion.
refresh
if printf '%s\n' "$DUMP" | grep -qE "^menu"; then
    fail "pressing a menu row closes the menu"
else
    ok "pressing a menu row closes the menu"
fi
act context-menu "[]" "[]"
if python3 "$A11Y" menu 2>/dev/null | grep -qF 'F2'; then
    act up "[]" "[]"
    i=0
    while [ "$i" -lt 50 ]; do
        refresh
        printf '%s\n' "$DUMP" | grep -qE "^menu" || break
        i=$((i + 1))
        sleep 0.2
    done
    if printf '%s\n' "$DUMP" | grep -qE "^menu"; then
        fail "navigating away closes an open menu"
    else
        ok "navigating away closes an open menu"
    fi
    act back "[]" "[]"
else
    fail "navigating away closes an open menu (the menu would not open a second time)"
fi
# The last check of this section re-reads the tree rather than reusing the dump the
# check above waited on, because the dump it would otherwise reuse was taken while the
# window was still on the parent folder.
i=0
while [ "$i" -lt 50 ] && ! printf '%s\n' "$DUMP" |
    grep -qF "$(printf 'frame\t%s\t' "$TARGET")"; do
    refresh
    i=$((i + 1))
    sleep 0.2
done
if ! printf '%s\n' "$DUMP" | grep -qF "$(printf 'frame\t%s\t' "$TARGET")"; then
    fail "the window is back on the folder the menu checks began from ($(printf '%s\n' "$DUMP" |
        grep -E '^frame' | cut -f1,2 | tr '\n' ' '))"
else
    ok "the window is back on the folder the menu checks began from"
fi

# --- opening a file with a program ------------------------------------------
# The chooser reads its list out of the desktop database, so the check has to
# install a program of its own: a container with no `MimeType=` anywhere has an
# honest empty list, and an empty list proves nothing about whether the lookup
# works. The desktop file is written into the private `$XDG_DATA_HOME` the whole
# script already runs with, so nothing is left behind and the machine's real
# applications are not disturbed.
#
# What is asserted is the whole chain rather than the dialog: that the action
# knows a folder cannot be opened this way, that it knows a plain file can, that
# the chooser names the registered program, and that pressing the row really
# runs it — which the marker file the `Exec=` line leaves behind shows. A dialog
# that listed a program but could not start it would pass every check that stops
# at the accessibility tree.
if ! command -v update-desktop-database >/dev/null; then
    echo "skip open with: this machine has no desktop database updater"
else
    # The `Exec=` line is a shell command rather than a bare `touch` because the
    # check wants two facts out of one launch: that a program ran at all, and that
    # the file it was handed is the one the row was standing on. The link's target
    # is the URI the chooser passed, so the assertion covers the argument and not
    # only the attempt. The whole command is one double-quoted desktop argument,
    # which is how the desktop-entry parser wants a shell command anyway.
    mkdir -p "$XDG_DATA_HOME/applications"
    printf '%s\n' \
        '[Desktop Entry]' \
        'Type=Application' \
        'Name=Probe Editor' \
        'Comment=Writes a marker where the ui smoke test can see it' \
        'Exec=/bin/sh -c "touch '"$ROOT"'/probe.marker; ln -sf %u '"$ROOT"'/probe.link"' \
        'NoDisplay=false' \
        'MimeType=text/plain;' \
        >"$XDG_DATA_HOME/applications/ufm-probe.desktop"
    update-desktop-database "$XDG_DATA_HOME/applications" 2>/dev/null || :

    # A folder is not something to open with an editor, and the row has to say so
    # rather than open a chooser with nothing to launch. The selection is set
    # explicitly rather than inherited from the checks above, because navigation
    # clears it and an action disabled for having no selection would pass this
    # check for the wrong reason.
    python3 "$A11Y" select one >/dev/null 2>&1 || :
    if describe open-with | grep -qF '(false'; then
        ok "Open With is disabled while a folder is the one selection"
    else
        fail "Open With is disabled while a folder is the one selection ($(describe open-with))"
    fi

    printf hello >"$TARGET/note.txt"
    if ! wait_named "note.txt"; then
        fail "a plain file appears so Open With can be asked about it"
    else
        ok "a plain file appears so Open With can be asked about it"
        if ! python3 "$A11Y" select note.txt >/dev/null 2>&1; then
            fail "Open With is sensitive with one plain file selected"
        elif describe open-with | grep -qF '(true'; then
            ok "Open With is sensitive with one plain file selected"
        else
            fail "Open With is sensitive with one plain file selected ($(describe open-with))"
        fi
        act open-with "[]" "[]"
        has_frame "Open With" "Open With opens a chooser of its own"
        # `Probe Editor` is a name nothing else in the interface says, so finding
        # it here can only mean the chooser read the desktop database and drew a
        # row for what it found.
        has_named "Probe Editor" "the chooser lists the program the desktop database registers"
        # The press goes into the chooser's subtree by name, because the main
        # window's titlebar has a Close button too and an unscoped press would
        # reach that one first.
        if ! python3 "$A11Y" press "Probe Editor" "Open With" >/dev/null 2>&1; then
            fail "the chooser's row can be activated"
        else
            i=0
            while [ "$i" -lt 50 ] && [ ! -f "$ROOT/probe.marker" ]; do
                i=$((i + 1))
                sleep 0.2
            done
            if [ -f "$ROOT/probe.marker" ]; then
                ok "pressing a row starts the program it names"
            else
                fail "pressing a row starts the program it names (no marker written)"
            fi
            # Which file the program was handed: the difference between a chooser
            # that opens the selected file and one that opens whatever the last row
            # it drew happened to be. The comparison is against a path rather than
            # the `file://` URI the chooser passed, because `GIO` turns a `file:`
            # URI back into a plain filename when it builds a desktop file's
            # command line — which is the right answer for a program that expects
            # an argument it can open, and is what the check reports honestly
            # rather than wishing it were a URI.
            handed=$(readlink "$ROOT/probe.link" 2>/dev/null || echo nothing)
            if [ "$handed" = "$TARGET/note.txt" ]; then
                ok "the program is handed the file that was selected"
            else
                fail "the program is handed the file that was selected (got: $handed)"
            fi
        fi
        # The dialog is closed again rather than left open: the frame count that
        # `--new-window` is checked against below is taken from this same bus, and
        # a chooser still on screen would be counted as a window the program
        # opened on its own.
        python3 "$A11Y" press Close "Open With" >/dev/null 2>&1 || :
        i=0
        while [ "$i" -lt 50 ] && printf '%s\n' "$DUMP" |
            grep -qF "$(printf 'frame\tOpen With\t')"; do
            refresh
            i=$((i + 1))
            sleep 0.2
        done
        if printf '%s\n' "$DUMP" | grep -qF "$(printf 'frame\tOpen With\t')"; then
            fail "the chooser closes when its Close button is pressed"
        else
            ok "the chooser closes when its Close button is pressed"
        fi
    fi
fi

# --- the right preview pane --------------------------------------------------
# The inspector is built from what the listing already carries. The text pane is what
# makes this checkable: its content is the file's own bytes, which appear
# nowhere else on screen, so finding the string proves the worker read the file
# and the answer found the label that asked for it.
#
# Unlike Open With, whose check sits just above and runs on this same selection,
# the command is enabled for a folder: a preview answers "what is this row", and a
# folder's size, date and permissions are an answer. The contrast is the point of
# the check — the two actions share the one-row rule and differ only on what a row
# may be, and a selection that disables one while enabling the other is the only
# way to see that from outside.
#
# The file is written here rather than reused from the checks above, because
# those sit behind a skip and this section does not depend on the desktop
# database.
#
# The archive window from the extract section is closed first. A preview
# belongs to the window the user is looking at, and an autohide popover on a
# window the compositor considers inactive dismisses itself the moment it maps
# — correct behaviour, and the reason the popover would otherwise appear to
# flash here: the open-with chooser left this window without focus, and the
# archive window still holding it. Closing the archive window hands focus back.
gdbus call --session --dest org.uflinux.FileManager \
    --object-path /org/uflinux/FileManager/window/2 \
    --method org.gtk.Actions.Activate win.close "[]" "[]" >/dev/null 2>&1 || :
sleep 1
python3 "$A11Y" select one >/dev/null 2>&1 || :
if describe preview | grep -qF '(true'; then
    ok "Preview is enabled for a folder, which has facts of its own to show"
else
    fail "Preview is enabled for a folder, which has facts of its own to show ($(describe preview))"
fi

printf preview-head-line >"$TARGET/preview.txt"
if ! wait_named "preview.txt"; then
    fail "a plain file appears so a preview can be asked of it"
else
    ok "a plain file appears so a preview can be asked of it"
    if ! python3 "$A11Y" select preview.txt >/dev/null 2>&1; then
        fail "Preview is sensitive with one plain file selected"
    elif describe preview | grep -qF '(true'; then
        ok "Preview is sensitive with one plain file selected"
    else
        fail "Preview is sensitive with one plain file selected ($(describe preview))"
    fi
    # The pane is visible by default and follows selection.
    has_named "preview-head-line" "the preview shows the opening text of the file"
    # The facts the listing already had, read off the fixture rather than
    # hard-coded. Nothing else in the interface prints a mode string: the
    # properties dialog that does is closed at this point.
    pmode=$(stat -c '%A' "$TARGET/preview.txt")
    refresh
    if printf '%s\n' "$DUMP" | grep -qF "$(printf '\t%s\t' "$pmode")"; then
        ok "the preview reports the mode bits the file actually has"
    else
        fail "the preview reports the mode bits the file actually has (no $pmode)"
    fi
    # Space closes the pane, then opens it again with the same selected file.
    act preview "[]" "[]"
    i=0
    while [ "$i" -lt 50 ] && printf '%s\n' "$DUMP" |
        grep -qF "$(printf '\tpreview-head-line\t')"; do
        refresh
        i=$((i + 1))
        sleep 0.2
    done
    if printf '%s\n' "$DUMP" | grep -qF "$(printf '\tpreview-head-line\t')"; then
        fail "the same key closes the preview again"
    else
        ok "Space closes the preview pane"
    fi
    act preview "[]" "[]"
    has_named "preview-head-line" "Space reopens the preview pane"
fi

# A picture preview asks the thumbnail pipeline for the largest size it allows,
# which the cache files under its own band rather than the one the row icons
# live in. The band is the assertion: a preview that shared an entry with an icon
# would paint a 16 px picture at thirty times its size, and look like a rendering
# bug rather than a cache one. The same probe as further up decides whether a
# decode is possible here at all.
if [ "$probe" != ok ]; then
    echo "skip preview picture: this machine cannot decode an image"
else
    python3 "$A11Y" select photo.png >/dev/null 2>&1 || :
    i=0
    big=
    while [ "$i" -lt 100 ] && [ -z "$big" ]; do
        big=$(find "$HOME/.cache/thumbnails/large" -name '*.png' -size +0 2>/dev/null |
            head -1)
        [ -n "$big" ] || {
            i=$((i + 1))
            sleep 0.2
        }
    done
    if [ -n "$big" ]; then
        ok "a preview picture is cached in the band of its own size"
    else
        fail "a preview picture is cached in the band of its own size"
    fi
    act preview "[]" "[]"
fi

# --- the search bar -----------------------------------------------------------------
# The bar opens and the field takes text. Whether it finds anything depends on the
# indexer, which is checked separately below; the field accepting input is the UI's job.
act search "[]" "[]"
i=0
while [ "$i" -lt 50 ] && ! printf '%s\n' "$DUMP" | grep -qF "$(printf 'entry\t')"; do
    refresh
    i=$((i + 1))
    sleep 0.2
done
if printf '%s\n' "$DUMP" | grep -qF "$(printf 'entry\t')"; then
    ok "the search action opens a field"
else
    fail "the search action opens a field"
fi
if python3 "$A11Y" type note >/dev/null 2>&1; then
    got=$(python3 "$A11Y" entry 2>/dev/null || true)
    if [ "$got" = "note" ]; then
        ok "the search field takes the text typed into it"
    else
        fail "the search field takes the text typed into it (field reads '$got')"
    fi
else
    fail "the search field takes the text typed into it"
fi

# --- the star -----------------------------------------------------------------
# The star list is a file, the badge is drawn from it, and the collection is
# assembled from it, so what is worth asserting end to end is that the file
# appears, that a window pointed at `starred://` shows the row it names, and that
# the same command takes the star back off. What the menu flips its label to —
# "Unstar" — is the popup's own decision and is covered by that module's tests; a
# menu row exposes its chord to an AT client rather than its text, so the tree
# here cannot tell the two rows apart even though a user can.
#
# The row starred is the plain file the Open With checks created, which is also
# the only row the search filter left on screen.
#
# A second window in this same instance, opened on the archive's folder before
# anything is starred. Its copy of the star list is therefore the empty one, which
# is the state the two checks below are about: a window holding a copy from before
# another window wrote has to apply its change to what the file says now, rather
# than saving its own copy over the top of it. The archive's folder is the place to
# open, because it holds a row the first window does not have.
"$BIN" "file://$PACK" >/dev/null 2>&1 || :
# Which path the new window answers to is GTK's business rather than the test's, so
# the paths are probed rather than assumed. The archive window from the extract
# check was `window/2` and is closed by now; taking the highest path that still
# answers is what finds the new window rather than a number that happens to be
# free.
SECOND=
i=0
while [ "$i" -lt 50 ] && [ -z "$SECOND" ]; do
    probe=11
    while [ "$probe" -gt 1 ]; do
        if alive "/org/uflinux/FileManager/window/$probe"; then
            SECOND=/org/uflinux/FileManager/window/$probe
            break
        fi
        probe=$((probe - 1))
    done
    i=$((i + 1))
    sleep 0.2
done
if [ -n "$SECOND" ]; then
    ok "a second invocation opens a window the bus can reach"
else
    fail "a second invocation opens a window the bus can reach"
fi
python3 "$A11Y" select note.txt >/dev/null 2>&1 || :
act star "[]" "[]"
STAR_FILE=$XDG_CONFIG_HOME/uflinux/filemanager/starred.json
i=0
while [ "$i" -lt 50 ] && [ ! -f "$STAR_FILE" ]; do
    i=$((i + 1))
    sleep 0.2
done
if [ -f "$STAR_FILE" ] && grep -qF "$TARGET/note.txt" "$STAR_FILE"; then
    ok "starring a row writes it to the star list"
else
    fail "starring a row writes it to the star list (no '$TARGET/note.txt' in $STAR_FILE)"
fi
# The star put on by the window whose copy of the list predates it. Saving that
# copy whole — which is what this code used to do — leaves the file holding
# `bundle.tar` and nothing else, so the row the first window starred is gone: a
# silent loss, one window over, of something the user made a moment ago.
if ! python3 "$A11Y" select bundle.tar >/dev/null 2>&1; then
    fail "the second window's own row can be selected"
else
    act_on "$SECOND" star "[]" "[]"
    i=0
    while [ "$i" -lt 50 ] && ! grep -qF "$PACK/bundle.tar" "$STAR_FILE"; do
        i=$((i + 1))
        sleep 0.2
    done
    if grep -qF "$TARGET/note.txt" "$STAR_FILE" &&
        grep -qF "$PACK/bundle.tar" "$STAR_FILE"; then
        ok "a star put on in a second window keeps the one the first window wrote"
    else
        fail "a star put on in a second window keeps the one the first window wrote"
    fi
    # The same command again, which is the half a merge of the two windows' copies
    # would get wrong: a merge puts this star back, because the window that never
    # saw its removal still has it. Applying the change to the file is what lets
    # taking a star off stick.
    act_on "$SECOND" star "[]" "[]"
    i=0
    while [ "$i" -lt 50 ] && grep -qF "$PACK/bundle.tar" "$STAR_FILE"; do
        i=$((i + 1))
        sleep 0.2
    done
    if grep -qF "$TARGET/note.txt" "$STAR_FILE" &&
        ! grep -qF "$PACK/bundle.tar" "$STAR_FILE"; then
        ok "taking a star off in one window leaves the other window's star alone"
    else
        fail "taking a star off in one window leaves the other window's star alone"
    fi
fi
# The second window goes here rather than being left open: what follows counts the
# rows a collection window holds, and a window still holding a listing of its own
# is one more frame in a tree that finds frames by name.
act_on "$SECOND" close "[]" "[]"
if ! wait_no_frame "$PACK"; then
    fail "the second window would not close ($SECOND)"
fi
# The collection, in a process of its own rather than in this one. A window in the
# primary instance would hold the copy of the list that instance loaded, which is
# true and worth knowing but is not what the collection is being asked here:
# whether a list read from disk becomes rows.
STAR_LOG=$ROOT/starred.log
"$BIN" --new-window starred:// >"$STAR_LOG" 2>&1 &
STAR=$!
has_frame "Starred" "starred:// opens the collection"
SFRAME=$(python3 "$A11Y" dump-frame Starred 2>/dev/null || true)
if printf '%s\n' "$SFRAME" | grep -qF "$(printf '\tnote.txt\t')"; then
    ok "the collection shows the starred row"
else
    fail "the collection shows the starred row (no row named 'note.txt')"
fi
# The count, which the collection has to say itself: it is assembled from the
# store rather than read from a folder, so nothing else in the window knows how
# many rows there are.
if printf '%s\n' "$SFRAME" | grep -qF "$(printf '\t1 item\t')"; then
    ok "the collection says how many rows it holds"
else
    fail "the collection says how many rows it holds"
fi
# The collection window closes before what follows rather than staying open. It
# is a process of its own, and a process started with `--new-window` never owns
# the bus name, so its actions are not reachable over D-Bus at all — and the row
# picker finds a node by name across every window, which with two of them
# holding `note.txt` is a coin toss.
kill "$STAR" 2>/dev/null || :
if ! wait_no_frame "Starred"; then
    fail "the collection window would not close"
fi
# The same command again, on the same row, now that the collection is out of the
# way. Starring is a toggle rather than two commands, so a second press has to
# leave the list without the row in it — and the file has to be rewritten, not
# merely left alone.
python3 "$A11Y" select note.txt >/dev/null 2>&1 || :
act star "[]" "[]"
if [ -f "$STAR_FILE" ] && grep -qF "$TARGET/note.txt" "$STAR_FILE"; then
    fail "the same command takes the star back off (the row is still listed)"
else
    ok "the same command takes the star back off"
fi
# An empty collection says so rather than showing a blank pane, which is the
# difference between "nothing starred yet" and a window that looks broken. Asked
# of a fresh process, so that nothing is left over from the list read above.
"$BIN" --new-window starred:// >"$ROOT/starred2.log" 2>&1 &
STAR2=$!
if wait_named "Nothing starred yet"; then
    ok "an empty collection says so"
else
    fail "an empty collection says so (no banner)"
fi
kill "$STAR2" 2>/dev/null || :
if ! wait_no_frame "Starred"; then
    fail "an empty collection says so (its window would not close)"
fi

# --- the bookmark ---------------------------------------------------------------
# A bookmark is one file and one row in the sidebar, so the two things worth
# asserting outside the unit tests are that the command writes the file and that
# the row it describes appears with the folder's name. The label the menu flips
# to — "Remove Bookmark" — is the popup's decision and covered by that module's
# tests; an AT client sees a menu row's chord rather than its text, so the tree
# here could not tell the two rows apart even though a user can.
#
# The folder bookmarked is the one the window is standing on, which is the fixture
# rather than a row inside it: a bookmark names a place to return to, and the
# window's own folder is the one thing a user cannot mistake for another.
BOOK_FILE=$XDG_CONFIG_HOME/uflinux/filemanager/bookmarks.json
act bookmark "[]" "[]"
i=0
while [ "$i" -lt 50 ] && [ ! -f "$BOOK_FILE" ]; do
    i=$((i + 1))
    sleep 0.2
done
if [ -f "$BOOK_FILE" ] && grep -qF "$TARGET" "$BOOK_FILE"; then
    ok "bookmarking the folder writes it to the bookmark list"
else
    fail "bookmarking the folder writes it to the bookmark list (no '$TARGET' in $BOOK_FILE)"
fi
# The row, read from this window's own subtree. The name is the folder's last
# component, which is what a bookmark with no name of its own takes; the header's
# path label says the same word inside a longer path, so the match is on a name
# field rather than a substring.
BFRAME=$(python3 "$A11Y" dump-frame "$TARGET" 2>/dev/null || true)
if printf '%s\n' "$BFRAME" | grep -qF "$(printf 'list item\talpha\t')"; then
    ok "the bookmark appears as a named row in the sidebar"
else
    fail "the bookmark appears as a named row in the sidebar (no list item named 'alpha')"
fi
# One heading above the folder rows, which is the reason bookmarks are spliced
# into the group rather than appended after it: a heading is drawn wherever the
# group changes, so appending would say "Folders" a second time below "Trash".
if [ "$(printf '%s\n' "$BFRAME" | grep -cF "$(printf '\tFolders\t')")" -eq 1 ]; then
    ok "the sidebar says Folders once"
else
    fail "the sidebar says Folders once ($(printf '%s\n' "$BFRAME" | grep -cF "$(printf '\tFolders\t')") headings)"
fi

# --- what a bookmark row's menu does ------------------------------------------------
# The rows a right-click offers are four window actions, and the tree reader can
# see an action even though it cannot see a pointer, so the menu is driven over the
# bus here. What is worth asserting end to end is the effect on the file: which
# rows a place gets, and that each one carries the URI it acts on, are the
# sidebar's own decision and covered by `panel.rs` and `popup.rs`.
URI=file://$TARGET
for row in rename-bookmark bookmark-up bookmark-down remove-bookmark; do
    if describe "$row" | grep -qF '(true'; then
        ok "a bookmark row's $row action is one the window answers"
    else
        fail "a bookmark row's $row action is one the window answers ($(describe "$row"))"
    fi
done

# A second bookmark first, on the folder above. Reordering needs two rows to trade
# places, and a row that is not the folder the window is standing in is what lets
# the removal below tell "the row that was clicked" from "the folder in the title
# bar" — the distinction the two commands exist apart for.
ONE_URI=file://$ROOT/tree
act up "[]" "[]"
act bookmark "[]" "[]"
act back "[]" "[]"
i=0
while [ "$i" -lt 50 ] && ! grep -qF "\"$ONE_URI\"" "$BOOK_FILE" 2>/dev/null; do
    i=$((i + 1))
    sleep 0.2
done
BFRAME=$(python3 "$A11Y" dump-frame "$TARGET" 2>/dev/null || true)
if printf '%s\n' "$BFRAME" | grep -qF "$(printf 'list item\ttree\t')"; then
    ok "a second bookmark appears as a row of its own"
else
    fail "a second bookmark appears as a row of its own (no list item named 'tree')"
fi
# The prompt, answered. Scoped to its own window on the way in and out: the folder
# window's search bar holds an entry too, and an unscoped walk reaches that one
# first, so an unscoped check would type a bookmark's name into a search field.
act rename-bookmark "[<'$URI'>]" "[]"
has_frame "Rename Bookmark" "a bookmark row asks for a name rather than a new folder"
prompt=$(python3 "$A11Y" entry "Rename Bookmark" 2>/dev/null || true)
if [ "$prompt" = "alpha" ]; then
    ok "the prompt starts from the name the row already shows"
else
    fail "the prompt starts from the name the row already shows (reads '$prompt')"
fi
if ! python3 "$A11Y" type Workspaces "Rename Bookmark" >/dev/null 2>&1; then
    fail "the bookmark prompt has a field of its own to type into"
else
    got=$(python3 "$A11Y" entry "Rename Bookmark" 2>/dev/null || true)
    if [ "$got" = "Workspaces" ]; then
        ok "the bookmark prompt takes the name typed into it"
    else
        fail "the bookmark prompt takes the name typed into it (reads '$got')"
    fi
fi
python3 "$A11Y" press Rename "Rename Bookmark" >/dev/null 2>&1 || true
i=0
while [ "$i" -lt 50 ] && ! grep -qF '"label": "Workspaces"' "$BOOK_FILE" 2>/dev/null; do
    i=$((i + 1))
    sleep 0.2
done
if grep -qF '"label": "Workspaces"' "$BOOK_FILE"; then
    ok "answering the prompt stores the name beside the bookmark"
else
    fail "answering the prompt stores the name beside the bookmark"
fi
if wait_no_frame "Rename Bookmark"; then
    ok "answering the bookmark prompt closes it"
else
    fail "answering the bookmark prompt closes it"
fi
has_named Workspaces "the sidebar row takes the name the user gave it"
# Where a bookmark sits is where its row is drawn, so a move has to show up as the
# two entries trading places in the file. The list is written one field per line,
# so the earlier bookmark is simply the earlier line — no parser needed to tell an
# order from a pair of positions.
order() {
    local tree_line work_line
    tree_line=$(grep -nF "\"$ONE_URI\"" "$BOOK_FILE" | head -1 | cut -d: -f1)
    work_line=$(grep -nF "\"$URI\"" "$BOOK_FILE" | head -1 | cut -d: -f1)
    tree_at=${tree_line:-}
    work_at=${work_line:-}
}
order
if [ -n "$tree_at" ] && [ -n "$work_at" ] && [ "$work_at" -lt "$tree_at" ]; then
    ok "a second bookmark is appended below the first"
else
    fail "a second bookmark is appended below the first (Workspaces line ${work_at:-none}, tree line ${tree_at:-none})"
fi
act bookmark-up "[<'$ONE_URI'>]" "[]"
i=0
while [ "$i" -lt 50 ]; do
    order
    [ -n "$tree_at" ] && [ -n "$work_at" ] && [ "$tree_at" -lt "$work_at" ] && break
    i=$((i + 1))
    sleep 0.2
done
if [ -n "$tree_at" ] && [ -n "$work_at" ] && [ "$tree_at" -lt "$work_at" ]; then
    ok "Move Up swaps a bookmark with the one above it"
else
    fail "Move Up swaps a bookmark with the one above it (tree line ${tree_at:-none}, Workspaces line ${work_at:-none})"
fi
act bookmark-down "[<'$ONE_URI'>]" "[]"
i=0
while [ "$i" -lt 50 ]; do
    order
    [ -n "$tree_at" ] && [ -n "$work_at" ] && [ "$work_at" -lt "$tree_at" ] && break
    i=$((i + 1))
    sleep 0.2
done
if [ -n "$tree_at" ] && [ -n "$work_at" ] && [ "$work_at" -lt "$tree_at" ]; then
    ok "Move Down puts it back where it came from"
else
    fail "Move Down puts it back where it came from (tree line ${tree_at:-none}, Workspaces line ${work_at:-none})"
fi
# Removal, aimed at the row rather than the window. The window is standing in
# `alpha`, so a command that acted on the title bar would empty the wrong entry.
act remove-bookmark "[<'$ONE_URI'>]" "[]"
i=0
while [ "$i" -lt 50 ] && grep -qF "\"$ONE_URI\"" "$BOOK_FILE" 2>/dev/null; do
    i=$((i + 1))
    sleep 0.2
done
if ! grep -qF "\"$ONE_URI\"" "$BOOK_FILE" 2>/dev/null; then
    ok "the sidebar menu removes the bookmark it was opened on"
else
    fail "the sidebar menu removes the bookmark it was opened on ($ONE_URI still in $BOOK_FILE)"
fi
if grep -qF "\"$URI\"" "$BOOK_FILE"; then
    ok "removing another row leaves the folder the window is standing in alone"
else
    fail "removing another row leaves the folder the window is standing in alone"
fi
refresh
if printf '%s\n' "$DUMP" | grep -qF "$(printf 'list item\ttree\t')"; then
    fail "the row goes out of the sidebar with its bookmark"
else
    ok "the row goes out of the sidebar with its bookmark"
fi

# The same command takes it back out, which is the half a toggle gets wrong: a
# second activation that only ever added would leave a row the user cannot
# remove. The file stays, emptied — a list with nothing in it is still a list.
act bookmark "[]" "[]"
i=0
while [ "$i" -lt 50 ] && grep -qF "$TARGET" "$BOOK_FILE" 2>/dev/null; do
    i=$((i + 1))
    sleep 0.2
done
if [ -f "$BOOK_FILE" ] && ! grep -qF "$TARGET" "$BOOK_FILE"; then
    ok "the same command takes the bookmark back out"
else
    fail "the same command takes the bookmark back out ($TARGET still in $BOOK_FILE)"
fi
refresh
if printf '%s\n' "$DUMP" | grep -qF "$(printf 'list item\talpha\t')"; then
    fail "the row goes away with the bookmark"
else
    ok "the row goes away with the bookmark"
fi

# --- a second location opens a window in the running instance -----------------------
# `computer://` on a second invocation reaches the primary through its `open` signal and
# adds a window; the new window is titled for what it shows.
"$BIN" computer:// >"$ROOT/second.log" 2>&1 || fail "a second invocation exits non-zero"
has_frame "Computer" "a second invocation opens a Computer window in the running instance"

# What is inside that window, read from its own subtree. An unscoped dump would
# pass a check for "Home" on the strength of the other window's sidebar, which is
# not what is being asked. The view is also the reason this invocation works at
# all: `computer://` is not a directory, so there is nothing for a reader to read,
# and until the view existed the location was handed to the local reader anyway.
CFRAME=$(python3 "$A11Y" dump-frame Computer 2>/dev/null || true)
if printf '%s\n' "$CFRAME" | grep -qF "$(printf '\tSystem\t')"; then
    ok "the My Computer view shows the system drive"
else
    fail "the My Computer view shows the system drive (no card named 'System')"
fi
# "Home" appears in both the sidebar and the preferred-folder shortcuts.
if [ "$(printf '%s\n' "$CFRAME" | grep -cF "$(printf '\tHome\t')")" -ge 2 ]; then
    ok "the My Computer view shows the home folder"
else
    fail "the My Computer view shows the home folder shortcut"
fi
# The one thing a folder listing could never say, and the reason the view exists:
# a card reports how full its filesystem is. The sidebar's Home row carries a
# usage bar too, so the check counts: one line is the sidebar, two means a card
# said it as well.
if [ "$(printf '%s\n' "$CFRAME" | grep -cF "free of")" -ge 2 ]; then
    ok "a card reports how full its filesystem is"
else
    fail "a card reports how full its filesystem is"
fi
# The share the fixture listed before the application started is a card too, named
# for the share rather than for the mount point it would have been attached to.
if printf '%s\n' "$CFRAME" | grep -qF "$(printf 'table cell\tvideos2024\t')"; then
    ok "a share the user listed is a card of its own on My Computer"
else
    fail "a share the user listed is a card of its own on My Computer (no card named 'videos2024')"
fi
# The same share is a sidebar row in this window as well. Both halves are needed
# before anything is taken away: a check that something disappeared is only worth
# what the check was worth that it was there, and this window asks for nothing
# below — it is the one that has to be told.
if printf '%s\n' "$CFRAME" | grep -qF "$(printf 'list item\tvideos2024\t')"; then
    ok "a window that did not list the share shows its row too"
else
    fail "a window that did not list the share shows its row too (no row named 'videos2024')"
fi
# And it says which state it is in. A share the kernel has no record of is neither
# broken nor connected, and a card that said neither would leave a user guessing
# about a row that is sitting in the sidebar.
if printf '%s\n' "$CFRAME" | grep -qF "$(printf '\tNot connected\t')"; then
    ok "a share that is listed but not connected says which it is"
else
    fail "a share that is listed but not connected says which it is"
fi
# The bug by name. Handing `computer://` to the local reader used to answer
# "not a valid location", printed as a banner in a window that then looked empty.
if printf '%s\n' "$CFRAME" | grep -qF "not a valid location"; then
    fail "Computer shows cards rather than a reader error"
else
    ok "Computer shows cards rather than a reader error"
fi
# The count in the bar counts cards. Left to the listing path it would have said
# "Empty folder" about a window showing two drives. Three cards are present:
# Desktop, the system drive, and the configured share. Home is a shortcut.
if printf '%s\n' "$CFRAME" | grep -qF "$(printf '\t3 items\t')"; then
    ok "the status line counts the cards rather than entries read from disk"
else
    fail "the status line counts the cards rather than entries read from disk"
fi
if grep -qF "not a valid location" "$ROOT/second.log"; then
    fail "opening Computer logs no reader error"
else
    ok "opening Computer logs no reader error"
fi

# --- the sidebar row and the card carry their own name ------------------------------
# Both are list items, and until the name was set on the item itself an AT client
# saw an anonymous row with a label somewhere inside it: nothing to announce, and
# nothing to tell one row from the next. The way to see the difference is to ask
# for the item by role and name together, which is what a screen reader reads.
#
# The Computer window is the one to ask, because it holds both halves: the sidebar
# and the cards. Read from its own subtree, so a row in the other window's sidebar
# cannot stand in for the one being checked.
PFRAME=$(python3 "$A11Y" dump-frame Computer 2>/dev/null || true)
if printf '%s\n' "$PFRAME" | grep -qF "$(printf 'list item\tHome\t')"; then
    case $(printf '%s\n' "$PFRAME" | grep -F "$(printf 'list item\tHome\t')" | head -1 | cut -f3) in
    *focusable*) ok "a sidebar row is a named item that can take focus" ;;
    *) fail "a sidebar row is a named item that can take focus (states: $(printf '%s\n' "$PFRAME" | grep -F "$(printf 'list item\tHome\t')" | head -1 | cut -f3))" ;;
    esac
else
    fail "a sidebar row is a named item that can take focus (no list item named 'Home')"
fi
# A card is named after the thing it stands for. Without an explicit name GTK
# collects the text of the whole subtree, which announced the drive as
# "Home /tmp/h 0.86545 125 GiB free of 929 GiB" — the capacity read out twice,
# the fraction read out as a number, and no way to tell two drives apart before
# the sentence ended. An exact-name match is what says the label won.
if printf '%s\n' "$PFRAME" | grep -qF "$(printf 'table cell\tHome\t')"; then
    ok "a card is named for its drive rather than for every label inside it"
else
    fail "a card is named for its drive rather than for every label inside it"
fi
# The capacity has not been hidden, only moved: it has to be a node of its own
# that a reader reaches after the name, and not part of the card's name. Both
# halves are needed, since dropping the capacity line would satisfy the first
# half by having nothing to find. The sidebar's usage bar renders the same
# phrase, so what is ruled out here is the phrase inside a card's name rather
# than the phrase anywhere in the window.
if printf '%s\n' "$PFRAME" | grep -qE "$(printf '^label\t')[^\t]*free of" \
    && ! printf '%s\n' "$PFRAME" | grep -qE "$(printf '^table cell\t')[^\t]*free of"; then
    ok "the capacity is a line of its own rather than part of a card's name"
else
    fail "the capacity is a line of its own rather than part of a card's name"
fi

# --- a share the user listed earlier, and what its row offers -----------------------
# The fixture wrote one record before the application started, for a share that is
# listed without being connected: its mount point was never created, so the kernel
# says nothing is mounted there. That is the state a share falls back into whenever
# a server goes away or a session ends, and the one where the row has the most to
# say for itself — so it is also the state worth driving over the bus.
#
# Which rows a right-click offers, and what each one carries, are the sidebar's own
# decision and covered by `panel.rs`. What is worth asserting here is that the
# commands behind those rows reach the file on disk and say what they did, which no
# unit test can see.
#
# Both commands below go to the folder window and are read back from its own subtree.
# The Computer window is left open on purpose: the table is a file every window reads,
# so a command run here has to repaint that window too, and the checks that it does
# are at the end of this section. What the share looked like over there — a row as
# well as a card — is checked above, while it was still listed.
#
# The four rows a mount row can offer. All four are registered on every window and
# none is ever disabled: what a share can do right now decides which rows are put in
# front of the user, not whether the command exists. An action that was never
# registered and one that is disabled are told apart here by `Describe` answering
# with `(true`.
#
# First that the folder window drew the row at all, since everything below is judged
# by the absence of it. The card was checked on My Computer above; this is the other
# half, and the reason the two are not the same check: a card is built once for one
# window, a sidebar row is rebuilt from the table every time the table changes.
WFRAME=$(python3 "$A11Y" dump-frame "$TARGET" 2>/dev/null || true)
if printf '%s\n' "$WFRAME" | grep -qF "$(printf 'list item\tvideos2024\t')"; then
    ok "a share the user listed is a row in the sidebar too"
else
    fail "a share the user listed is a row in the sidebar too"
fi
if printf '%s\n' "$WFRAME" | grep -qF "$(printf '\tNetwork\t')"; then
    ok "the share sits under a heading of its own"
else
    fail "the share sits under a heading of its own"
fi
for row in disconnect reconnect forget-share autostart-share; do
    if describe "$row" | grep -qF '(true'; then
        ok "a mount row's $row action is one the window answers"
    else
        fail "a mount row's $row action is one the window answers ($(describe "$row"))"
    fi
done
# Reconnect, on a share whose mount helper this container does not have. The point is
# not the failure — it is that the row did the work rather than reporting that it had
# done something, and that the reason got both said out loud and written down beside
# the record. That last part is what turns a card that silently does nothing into one
# that says why, and it only exists in a file on disk.
act reconnect "[<'$MOUNT_ID'>]" "[]"
i=0
WFRAME=$(python3 "$A11Y" dump-frame "$TARGET" 2>/dev/null || true)
while [ "$i" -lt 50 ]; do
    WFRAME=$(python3 "$A11Y" dump-frame "$TARGET" 2>/dev/null || true)
    printf '%s\n' "$WFRAME" | grep -qE "$(printf '^label\t')mount error:" && break
    i=$((i + 1))
    sleep 0.2
done
if printf '%s\n' "$WFRAME" | grep -qE "$(printf '^label\t')mount error:"; then
    ok "reconnecting a share reports what stopped it rather than pretending"
else
    fail "reconnecting a share reports what stopped it rather than pretending"
fi
if grep -qF "$MOUNT_ID" "$MOUNT_FILE" && grep -qF '"last_error": "mount error:' "$MOUNT_FILE"; then
    ok "a share that failed to reconnect keeps its reason beside the record"
else
    fail "a share that failed to reconnect keeps its reason beside the record"
fi
# Connect at login, which is the one row of the four whose whole job is a boolean in
# the record: nothing is mounted, nothing is detached, and the answer has to be in
# `mounts.json` before the process exits, because the next session reads that file and
# not this window. Pressed twice, so the check covers both directions of the flip and
# leaves the fixture as it found it — a record that does not connect at login, which is
# what the `forget-share` block below was written against.
act autostart-share "[<'$MOUNT_ID'>]" "[]"
i=0
while [ "$i" -lt 50 ]; do
    grep -qF '"autostart": true' "$MOUNT_FILE" && break
    i=$((i + 1))
    sleep 0.2
done
if grep -qF '"autostart": true' "$MOUNT_FILE"; then
    ok "marking a share connect at login is written to the table"
else
    fail "marking a share connect at login is written to the table ($(cat "$MOUNT_FILE"))"
fi
WFRAME=$(python3 "$A11Y" dump-frame "$TARGET" 2>/dev/null || true)
if printf '%s\n' "$WFRAME" | grep -qF "this share will connect at login"; then
    ok "marking a share connect at login says so in the window that asked"
else
    fail "marking a share connect at login says so in the window that asked"
fi
act autostart-share "[<'$MOUNT_ID'>]" "[]"
i=0
while [ "$i" -lt 50 ]; do
    grep -qF '"autostart": false' "$MOUNT_FILE" && break
    i=$((i + 1))
    sleep 0.2
done
if grep -qF '"autostart": false' "$MOUNT_FILE"; then
    ok "pressing the same row again takes the share back out of the next session"
else
    fail "pressing the same row again takes the share back out of the next session ($(cat "$MOUNT_FILE"))"
fi
WFRAME=$(python3 "$A11Y" dump-frame "$TARGET" 2>/dev/null || true)
if printf '%s\n' "$WFRAME" | grep -qF "this share will no longer connect at login"; then
    ok "unmarking a share says the opposite of what marking it said"
else
    fail "unmarking a share says the opposite of what marking it said"
fi
# Forget, which is the row that has to be careful: it drops the record without
# detaching anything, so it refuses a share the kernel still reports connected. This
# one is not connected, so it goes — out of the file, and out of the sidebar of the
# window that asked for it.
act forget-share "[<'$MOUNT_ID'>]" "[]"
i=0
while [ "$i" -lt 50 ] && grep -qF "$MOUNT_ID" "$MOUNT_FILE"; do
    i=$((i + 1))
    sleep 0.2
done
if ! grep -qF "$MOUNT_ID" "$MOUNT_FILE"; then
    ok "forgetting a share that is not connected takes it out of the table"
else
    fail "forgetting a share that is not connected takes it out of the table ($MOUNT_ID still in $MOUNT_FILE)"
fi
i=0
while [ "$i" -lt 50 ]; do
    WFRAME=$(python3 "$A11Y" dump-frame "$TARGET" 2>/dev/null || true)
    printf '%s\n' "$WFRAME" | grep -qF "videos2024 is no longer listed" && break
    i=$((i + 1))
    sleep 0.2
done
if printf '%s\n' "$WFRAME" | grep -qF "videos2024 is no longer listed"; then
    ok "forgetting a share says so in the window that asked"
else
    fail "forgetting a share says so in the window that asked"
fi
if printf '%s\n' "$WFRAME" | grep -qF "$(printf 'list item\tvideos2024\t')"; then
    fail "the row goes out of the sidebar with its share"
else
    ok "the row goes out of the sidebar with its share"
fi
# The heading goes too. One is drawn wherever the group changes, so a heading left
# behind over an empty group is a section of the sidebar with nothing in it.
if printf '%s\n' "$WFRAME" | grep -qF "$(printf '\tNetwork\t')"; then
    fail "the heading goes with the last share in it"
else
    ok "the heading goes with the last share in it"
fi
# Now the window that asked for nothing. The table is a file on disk that every
# window reads, so a share forgotten here has to have left that window's sidebar
# and card flow as surely as it left this one's — otherwise the two windows are
# two different claims about what the user has, and only one of them is a lie.
i=0
while [ "$i" -lt 50 ]; do
    CFRAME=$(python3 "$A11Y" dump-frame Computer 2>/dev/null || true)
    printf '%s\n' "$CFRAME" | grep -qF "$(printf 'list item\tvideos2024\t')" || break
    i=$((i + 1))
    sleep 0.2
done
if printf '%s\n' "$CFRAME" | grep -qF "$(printf 'list item\tvideos2024\t')"; then
    fail "forgetting a share takes its row out of the window that did not ask"
else
    ok "forgetting a share takes its row out of the window that did not ask"
fi
if printf '%s\n' "$CFRAME" | grep -qF "$(printf 'table cell\tvideos2024\t')"; then
    fail "forgetting a share takes its card out of the window that did not ask"
else
    ok "forgetting a share takes its card out of the window that did not ask"
fi
# The count in that window's bar counts what it is showing now. Two cards left:
# the desktop folder and the system drive. Home is in the shortcuts above.
if printf '%s\n' "$CFRAME" | grep -qF "$(printf '\t2 items\t')"; then
    ok "the other window's status line counts the cards it has left"
else
    fail "the other window's status line counts the cards it has left"
fi

# --- --new-window really is a separate process --------------------------------------
# Before: however many frames the primary has. After: one more, from a process that gave
# up being the single instance so it would not just focus the window already open.
refresh
before=$(printf '%s\n' "$DUMP" | grep -cF "$(printf 'frame\t')")
"$BIN" --new-window "file://$ROOT/tree" >"$ROOT/newwin.log" 2>&1 &
NEW=$!
i=0
after=$before
while [ "$i" -lt 50 ]; do
    refresh
    after=$(printf '%s\n' "$DUMP" | grep -cF "$(printf 'frame\t')" || true)
    [ "$after" -gt "$before" ] && break
    i=$((i + 1))
    sleep 0.2
done
if [ "$after" -gt "$before" ]; then
    ok "--new-window opens a window rather than focusing an open one"
else
    fail "--new-window opens a window rather than focusing an open one ($before frames)"
fi

# --- the window actually painted ----------------------------------------------------
# A window can be present in the accessibility tree and still be a blank rectangle. A
# screenshot with many distinct colours is the cheap way to tell the two apart; an empty
# desktop is one flat colour, a rendered folder is thousands.
if command -v grim >/dev/null; then
    grim -t ppm "$ROOT/frame.ppm" 2>/dev/null || :
    colors=$(python3 - "$ROOT/frame.ppm" <<'PY' 2>/dev/null || echo 0
import sys
data = open(sys.argv[1], "rb").read()
# Skip the P6 header: three whitespace-separated tokens, then raw RGB.
idx, fields = 0, 0
while fields < 3:
    while idx < len(data) and data[idx:idx + 1].isspace():
        idx += 1
    while idx < len(data) and not data[idx:idx + 1].isspace():
        idx += 1
    fields += 1
seen = set()
for i in range(idx, min(len(data), idx + 3_000_000) - 2, 3):
    seen.add((data[i], data[i + 1], data[i + 2]))
print(len(seen))
PY
)
    if [ "${colors:-0}" -gt 50 ]; then
        ok "the window painted a variety of colours ($colors distinct)"
    else
        fail "the window painted a variety of colours (only ${colors:-0} distinct)"
    fi
fi

# --- the desktop, drawn by whoever is there -----------------------------------------
# Optional: only when a provider binary is alongside the file manager. This is the only
# check that exercises `--desktop-icons`, and it has to accept two honest answers, because
# the feature behind the flag is optional — it pulls in a library the package does not
# always carry. A build without it refuses the flag by name and exits 2; a build with it
# claims the desktop and paints it. Either way the flag has to do something a person can
# read about, and neither branch is allowed to open an ordinary window.
PROVIDER=$(dirname "$BIN")/uflinux-files-provider
if [ -x "$PROVIDER" ]; then
    # The provider answers `renderer` from the single question it can ask itself — has
    # anything been asked of me at all — so it has to be started first and left alone
    # until the fallback speaks, or the fallback would be told a shell is drawing and
    # correctly stand down.
    "$PROVIDER" >"$ROOT/provider.log" 2>&1 &
    PROV=$!
    i=0
    while [ "$i" -lt 100 ]; do
        gdbus introspect --session --xml -d org.uflinux.FileManager.Desktop1 \
            -o /org/uflinux/FileManager/Desktop >/dev/null 2>&1 && break
        i=$((i + 1))
        sleep 0.1
    done

    DESK_LOG=$ROOT/desktop.log
    "$BIN" --desktop-icons >"$DESK_LOG" 2>&1 &
    DESK=$!
    # Either the desktop appears or the process gives up; a slow machine is not a broken
    # one, and a build without the feature gives up almost at once.
    painted=no
    i=0
    while [ "$i" -lt 50 ]; do
        if ! kill -0 "$DESK" 2>/dev/null; then break; fi
        refresh
        if printf '%s\n' "$DUMP" | grep -qF "$(printf 'frame\t%s\t' 'uflinux-files-desktop')"; then
            painted=yes
            break
        fi
        i=$((i + 1))
        sleep 0.2
    done

    if [ "$painted" = yes ]; then
        ok "--desktop-icons claims the desktop when nothing else draws it"
        has_named "desk-one.txt" "the desktop paints an icon the provider listed"
        if printf '%s\n' "$DUMP" | grep -qF "$(printf '\t%s\t' 'desk-two.txt')"; then
            ok "the desktop paints every icon the provider listed"
        else
            fail "the desktop paints every icon the provider listed"
        fi
    else
        # The status the process itself left behind. `|| :` would swallow it, and the
        # status is the half of this check that says which branch the build is in.
        if wait "$DESK" 2>/dev/null; then code=0; else code=$?; fi
        if [ "$code" -eq 2 ] && grep -q 'desktop-icons' "$DESK_LOG"; then
            ok "--desktop-icons is refused by name in a build without the feature"
        else
            fail "--desktop-icons is refused by name in a build without the feature (exit $code)"
            head -3 "$DESK_LOG" | sed 's/^/     /'
        fi
    fi
    kill "$DESK" 2>/dev/null || :
    kill "$PROV" 2>/dev/null || :
fi

# --- the indexer answers on the bus -------------------------------------------------
# Optional: only when a searchd binary is alongside the file manager. It exercises the
# `--status` probe, which is how an installed daemon is debugged without a bus browser.
SEARCHD=${UFLINUX_SEARCHD_BIN:-$(dirname "$BIN")/uflinux-searchd}
if [ -x "$SEARCHD" ]; then
    "$SEARCHD" >"$ROOT/searchd.log" 2>&1 &
    SD=$!
    i=0
    status=
    while [ "$i" -lt 50 ]; do
        status=$("$SEARCHD" --status 2>/dev/null || true)
        printf '%s' "$status" | grep -q '"version"' && break
        i=$((i + 1))
        sleep 0.2
    done
    if printf '%s' "$status" | grep -q '"version"'; then
        ok "the indexer answers Status on the session bus"
    else
        fail "the indexer answers Status on the session bus (got: $status)"
    fi
    kill "$SD" 2>/dev/null || :
fi

# --- nothing complained on the way --------------------------------------------------
# A GLib critical is a programming error, not a runtime condition, and the window looks
# fine while one is being printed — which is exactly why it has to be checked for.
#
# One message is let through, and it is worth saying why: `GTK4` 4.22 re-reads a
# toplevel's accessibility properties when a window goes away, and when an assistive
# technology client is attached at that moment it sometimes reaches a surface that is
# already gone, printing `gdk_wayland_toplevel_set_a11y_properties: assertion
# 'GDK_IS_WAYLAND_TOPLEVEL (toplevel)' failed`. It is a race inside `GTK`'s own teardown,
# it is not reproducible without a tree reader attached, and it appears for the connect
# dialog exactly as it does for a name prompt — so it says nothing about this program.
# What is left over is still a bug, and is counted.
GTK_TEARDOWN_NOISE=gdk_wayland_toplevel_set_a11y_properties
criticals=$(grep "CRITICAL" "$APP_LOG" 2>/dev/null | grep -vc "$GTK_TEARDOWN_NOISE" || true)
noise=$(grep "CRITICAL" "$APP_LOG" 2>/dev/null | grep -c "$GTK_TEARDOWN_NOISE" || true)
if [ "${criticals:-0}" -eq 0 ]; then
    if [ "${noise:-0}" -gt 0 ]; then
        ok "the window came up without a GLib critical of our own ($noise GTK teardown warning ignored)"
    else
        ok "the window came up without a single GLib critical"
    fi
else
    fail "the window came up without a GLib critical of our own ($criticals printed)"
    grep "CRITICAL" "$APP_LOG" | grep -v "$GTK_TEARDOWN_NOISE" | head -5 | sed 's/^/     /'
fi

kill "$NEW" 2>/dev/null || :
kill "$APP" 2>/dev/null || :
kill "$LAB" 2>/dev/null || :
wait 2>/dev/null || :

if [ "$FAILED" -ne 0 ]; then
    echo "$FAILED check(s) failed"
    exit 1
fi
echo "all ui smoke checks passed"
