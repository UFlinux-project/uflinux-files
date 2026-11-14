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
mkdir -p "$HOME" "$XDG_DATA_HOME" "$ROOT/tree/alpha/one" "$ROOT/tree/alpha/two"
: >"$ROOT/tree/alpha/one/note.txt"
export HOME XDG_DATA_HOME
# The path the window opens on. Two entries, so the status line has a number to say.
TARGET=$ROOT/tree/alpha

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
