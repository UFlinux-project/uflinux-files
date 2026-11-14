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
export HOME XDG_DATA_HOME
# The path the window opens on. Two entries, so the status line has a number to say.
TARGET=$ROOT/tree/alpha

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
if describe paste | grep -qF '(false'; then
    ok "Paste is disabled while the clipboard is empty"
else
    fail "Paste is disabled while the clipboard is empty ($(describe paste))"
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
refresh
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
if ! python3 "$A11Y" select one >/dev/null 2>&1; then
    fail "a row can be selected through the accessibility tree"
else
    ok "a row can be selected through the accessibility tree"
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
if ! python3 "$A11Y" select one >/dev/null 2>&1; then
    fail "a row can be selected for the properties check"
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
want=$(printf '%s\n' - Shift+Control+N F2 Delete Shift+Delete Control+C Control+X Control+V Alt+Enter | sort)
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

# --- a second location opens a window in the running instance -----------------------
# `computer://` on a second invocation reaches the primary through its `open` signal and
# adds a window; the new window is titled for what it shows.
"$BIN" computer:// >"$ROOT/second.log" 2>&1 || fail "a second invocation exits non-zero"
has_frame "Computer" "a second invocation opens a Computer window in the running instance"

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
