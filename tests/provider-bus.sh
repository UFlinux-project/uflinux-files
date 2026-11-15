#!/bin/sh
# End-to-end check for `uflinux-files-provider`, run against a real session bus.
#
# This is the part of the desktop-icons feature that a unit test cannot reach: the
# provider's whole job is to answer D-Bus calls, and the unit tests deliberately drive
# the dispatch with a core that has no connection at all. Everything up to the wire is
# tested there; what the wire itself does — the introspection data being accepted by a
# real client, the interface name being claimable, a refusal arriving as a real D-Bus
# error, the name being released on `SIGTERM` — is only visible with a bus running.
#
# Run it from a checkout, with the binary already built:
#
#   cargo build -p uflinux-files-provider
#   tests/provider-bus.sh target/debug/uflinux-files-provider
#
# With no session bus around — which is the case in a build chroot or a container — it
# starts a private bus for itself. Needs `dbus-run-session` and `gdbus` either way (both
# from `glib-tools` on Alpine). It exits non-zero on the first answer that is not what the
# protocol says, so it can be chained onto the test run rather than read by a person.
#
# `crates/uflinux-files-provider/tests/bus.rs` is what chains it, and turns a missing tool
# into exit `2` — a skip, printed as one. That is the only way this can be both a gate and
# honest about a build chroot with no bus tools installed: a check that reports nothing is
# worse than one that reports it ran nothing.
set -eu

BIN=${1:-target/debug/uflinux-files-provider}
IFACE=org.uflinux.FileManager.Desktop1
OBJ=/org/uflinux/FileManager/Desktop

command -v dbus-run-session >/dev/null || { echo "need dbus-run-session" >&2; exit 2; }
command -v gdbus >/dev/null || { echo "need gdbus" >&2; exit 2; }
[ -x "$BIN" ] || { echo "no binary at $BIN — run cargo build -p uflinux-files-provider" >&2; exit 2; }

# A bus of our own rather than the developer's, so a failure cannot leave anything
# attached to a running session and so the check works in a chroot.
if [ -z "${DBUS_SESSION_BUS_ADDRESS:-}" ]; then
    exec dbus-run-session "$0" "$BIN"
fi

ROOT=$(mktemp -d)
trap 'rm -rf "$ROOT"' EXIT INT TERM
mkdir -p "$ROOT/runtime" "$ROOT/home/.config"
chmod 700 "$ROOT/runtime"
# The directory being served, and the path a refusal quotes back: `Launch` and
# `ContextMenu` fail with the `file://` form of what they tried to open, so the checks
# below need it spelled the same way. Spelled by hand rather than asked of GIO, because
# this is a shell and the prefix of a `file://` URI of an absolute path is not a secret.
DESKTOP=$ROOT/home/Desktop
mkdir -p "$DESKTOP"
# Three entries: two to place and one to remove, and an image for the thumbnail request to
# decode. The names below are the ids the calls send.
: >"$DESKTOP/a.txt"
: >"$DESKTOP/b.txt"
# A 64x64 PNG, spelled in octal so the fixture needs no image tool and no network. The
# size is not arbitrary: a source under a quarter of the edge asked for is refused as too
# small to fill a cell, and the edge this provider advertises is 128.
printf '\211\120\116\107\015\012\032\012\000\000\000\015\111\110\104\122\000\000\000\100\000\000\000\100\010\006\000\000\000\252\151\161\336\000\000\000\224\111\104\101\124\170\234\355\320\061\021\000\060\020\303\260\360\047\375\205\241\241\036\264\373\274\333\356\147\323\001\132\003\164\200\326\000\035\240\065\100\007\150\015\320\001\132\003\164\200\326\000\035\240\065\100\007\150\015\320\001\132\003\164\200\326\000\035\240\065\100\007\150\015\320\001\132\003\164\200\326\000\035\240\065\100\007\150\015\320\001\132\003\164\200\326\000\035\240\065\100\007\150\015\320\001\132\003\164\200\326\000\035\240\065\100\007\150\015\320\001\132\003\164\200\326\000\035\240\065\100\007\150\015\320\001\132\003\164\200\326\000\035\240\065\100\007\150\017\103\210\341\322\155\330\361\022\000\000\000\000\111\105\116\104\256\102\140\202' \
    >"$DESKTOP/pic.png"
PNG=$DESKTOP/pic.png

XDG_RUNTIME_DIR=$ROOT/runtime
HOME=$ROOT/home
export XDG_RUNTIME_DIR HOME

# One request, one reply. The body is a single JSON string and the interface declares one
# `s` argument, so the body goes on the command line as it is: `gdbus` parses each
# argument as a value of the declared type, and a string is one of the few types whose
# text form tolerates the JSON quotes being left alone. Wrapping the body in a tuple —
# which is how a reply is printed, and a tempting thing to copy — does not work, because
# then the argument is a tuple where a string was declared and the call never arrives.
# The `|| :` is what lets a refusal be captured into a variable: `gdbus` exits non-zero on
# a D-Bus error, and under `set -e` an assignment whose command substitution failed would
# end the run at the first refusal — which is half of what is being checked. Every check
# below reads the text, so the status has nothing left to say.
call() {
    gdbus call --session -d "$IFACE" -o "$OBJ" -m "$IFACE.$1" "$2" 2>&1 || :
}

introspect() {
    gdbus introspect --session --xml -d "$IFACE" -o "$OBJ" 2>&1
}

expect() {
    label=$1
    want=$2
    got=$3
    case $got in
    *"$want"*) echo "ok   $label" ;;
    *)
        echo "FAIL $label"
        echo "     wanted a reply containing: $want"
        echo "     got: $got"
        exit 1
        ;;
    esac
}

# Wait for one thing to show up in what the signal monitor saw. Nothing can be assumed to
# have arrived by the time a reply has: one signal is emitted inside the call that caused
# it, another reaches the bus only after the file watcher's debounce, and the monitor is a
# separate process that flushes when it likes.
seen() {
    i=0
    while [ "$i" -lt 100 ]; do
        if grep -q "$1" "$SIGNALS" 2>/dev/null; then return 0; fi
        i=$((i + 1))
        sleep 0.1
    done
    return 1
}

"$BIN" >"$ROOT/provider.log" 2>&1 &
pid=$!
# The provider reads the directory before it takes the name, so by the time the object
# answers the first model exists. The loop covers process start, not the read.
i=0
while [ "$i" -lt 100 ]; do
    if introspect >/dev/null 2>&1; then break; fi
    i=$((i + 1))
    sleep 0.1
done

xml=$(introspect)
expect "the interface is claimable" "interface name=\"$IFACE\"" "$xml"
expect "every method is introspectable" "<method name=\"RequestThumbnail\">" "$xml"
expect "the signals are introspectable" "<signal name=\"Changed\">" "$xml"

# The other half of the provider's job is talking first. A renderer never polls the
# model; it waits for `Changed` to notice a file and for `Progress` to know a read is in
# flight. Nothing below can see either signal from a reply, so a client attaches now —
# before the first call that changes anything, since the first one is the grid and the
# grid is what a renderer repaints on.
SIGNALS=$ROOT/signals.log
gdbus monitor --session --dest "$IFACE" --object-path "$OBJ" >"$SIGNALS" 2>&1 &
mon=$!
# The line `gdbus monitor` prints once it has attached to the name. Waiting for it is
# what keeps a slow start from losing the first signal rather than losing the check.
i=0
while [ "$i" -lt 100 ]; do
    grep -q "owned by" "$SIGNALS" 2>/dev/null && break
    i=$((i + 1))
    sleep 0.1
done

# `Status` before any renderer has talked to us: nobody is listening.
expect "Status reports no renderer yet" '"renderer":false' \
    "$(call Status '{"method":"status"}')"

# A grid, then the model — the order a renderer uses on startup.
expect "SetGeometry is accepted" '"reply":"ok"' \
    "$(call SetGeometry '{"method":"set-geometry","monitors":[{"monitor":0,"columns":4,"rows":4,"cell_w":96,"cell_h":96}]}')"
# A grid with no cells in it is a renderer with a bug, and a monitor named twice is not
# two screens but a contradiction about one. Both are refused rather than normalised away
# in silence, because a desktop that quietly kept the old layout is a bug report that
# cannot be reproduced.
expect "a grid with no cells is refused" "$IFACE.Error.Invalid" \
    "$(call SetGeometry '{"method":"set-geometry","monitors":[{"monitor":0,"columns":0,"rows":0,"cell_w":0,"cell_h":0}]}')"
expect "one monitor named twice is refused" "$IFACE.Error.Invalid" \
    "$(call SetGeometry '{"method":"set-geometry","monitors":[{"monitor":0,"columns":4,"rows":4,"cell_w":96,"cell_h":96},{"monitor":0,"columns":4,"rows":4,"cell_w":96,"cell_h":96}]}')"
expect "the refused grids did not replace the good one" '"columns":4' \
    "$(call GetModel '{"method":"get-model"}')"
model=$(call GetModel '{"method":"get-model"}')
expect "GetModel carries every entry" '"id":"a.txt"' "$model"
expect "the model carries the grid" '"columns":4' "$model"
expect "every item names a fallback icon" '"icon"' "$model"

# `RequestThumbnail` is the one method whose reply refers to something outside the
# message, so it is the one place the fd handover can be seen. What the answer may be
# depends on the machine — a build image with no PNG encoder in gdk-pixbuf is entitled to
# the icon fallback, which is why the model advertises `thumbnails` at all — so the check
# is that the request is answered with a thumbnail, and that a picture came with its
# descriptor. A bus error here would be the failure: it would mean an optional feature was
# allowed to fail a desktop icon.
thumb=$(call RequestThumbnail "{\"method\":\"request-thumbnail\",\"path\":\"$PNG\",\"size\":128}")
case $thumb in
*'"format":"png"'*)
    expect "a picture names the descriptor it travels on" '"fd":0' "$thumb"
    ;;
*'"format":"icon"'*)
    echo "skip a picture over the bus: this machine's pixbuf cannot encode a PNG, and the"
    echo "     fallback is the answer the protocol says to give in that case"
    ;;
*)
    echo "FAIL a thumbnail request is answered with a thumbnail"
    echo "     wanted a reply, not an error"
    echo "     got: $thumb"
    exit 1
    ;;
esac
# A path outside the directory being served is the provider's own policy rather than the
# machine's, so this half is checked outright: without it the method is a way to make a
# small process decode any image on the box.
expect "a path off the desktop is answered with an icon" '"format":"icon"' \
    "$(call RequestThumbnail '{"method":"request-thumbnail","path":"/etc/passwd","size":128}')"

# An id that is not on the desktop arrives as the protocol's own D-Bus error rather than
# a reply, which is what lets a renderer drop one icon without tearing down the grid.
expect "an unknown id is a D-Bus error" "$IFACE.Error.NoItem" \
    "$(call Place '{"method":"place","id":"gone.txt","monitor":0,"col":1,"row":2}')"
# A body naming a different method than the one it arrived on is refused too: the method
# the call came in on is part of the request, not a hint.
expect "a body on the wrong method is refused" "$IFACE.Error.Invalid" \
    "$(call Place '{"method":"status"}')"

# A real move.
expect "a move is accepted" '"reply":"ok"' \
    "$(call Place '{"method":"place","id":"a.txt","monitor":0,"col":2,"row":3}')"
expect "the move is in the model" '"col":2' \
    "$(call GetModel '{"method":"get-model"}')"

# The placement has to be on disk before anything else happens to this process.
saved=$ROOT/home/.config/uflinux/filemanager/desktop.json
[ -f "$saved" ] || {
    echo "FAIL no layout file at $saved"
    exit 1
}
expect "the layout holds the moved icon" '"col": 2' "$(cat "$saved")"
mode=$(stat -c '%a' "$saved" 2>/dev/null || stat -f '%Lp' "$saved")
[ "$mode" = 600 ] || {
    echo "FAIL the layout file is mode $mode, not 600"
    exit 1
}
echo "ok   the layout file is written at mode 600"

# `Trash` goes through GIO, so the file lands in the trash rather than vanishing, and the
# provider notices through its own watcher rather than because the caller said so.
expect "trash is accepted" '"reply":"ok"' \
    "$(call Trash '{"method":"trash","id":"b.txt"}')"
[ ! -e "$DESKTOP/b.txt" ] || {
    echo "FAIL the trashed file is still in the desktop directory"
    exit 1
}
# The count falls when the watcher's debounce expires, so this waits rather than assuming
# a number of milliseconds. Two entries are left: the moved text file and the picture.
i=0
while [ "$i" -lt 100 ]; do
    status=$(call Status '{"method":"status"}')
    if printf '%s' "$status" | grep -q '"items":2'; then break; fi
    i=$((i + 1))
    sleep 0.1
done
expect "the provider noticed the delete" '"items":2' "$status"
expect "Status now reports a renderer" '"renderer":true' "$status"

# Everything above reads replies. The client that attached before the first call has been
# reading the other half the whole time, and this is the only place either signal is
# checked: a provider that answered every call but never spoke first would leave a desktop
# that shows whatever was on disk when it started and never notices a file arriving.
seen '"phase":"scanning"' || true
expect "a read announces itself as a read" '"phase":"scanning"' "$(cat "$SIGNALS")"
seen '"phase":"done"' || true
expect "a read says how it ended" '"phase":"done"' "$(cat "$SIGNALS")"
seen '"reason":"a.txt moved"' || true
expect "the move is announced with what moved" '"reason":"a.txt moved"' "$(cat "$SIGNALS")"
seen '"items":2' || true
expect "the delete is announced too" '"items":2' "$(cat "$SIGNALS")"

# `Launch` and `ContextMenu` are the last two methods, and the two that end up outside
# this process: both ask the desktop for an application, and whether one exists to handle a
# text file is a fact about the machine running the fixture rather than about this code. So
# the check is the disjunction: either the call was accepted, or it was refused and
# `Status.note` carries the operating system's own words — the only channel a person would
# ever see. What is not allowed is a refusal that says nothing, which is what a desktop
# whose double-click does nothing looks like from the outside.
#
# `ContextMenu` first, because a note is only ever replaced and never cleared: checked in
# this order, the note below can only have come from this refusal. The URI is matched with
# the colon after it, which is the difference between the folder and a file inside it —
# `ContextMenu` opens the folder, and that is the half of a right-click this protocol can
# actually deliver.
menu=$(call ContextMenu '{"method":"context-menu","id":"a.txt"}')
case $menu in
*'"reply":"ok"'*) echo "ok   a context menu either ran or explained itself" ;;
*"$IFACE.Error.Invalid"*)
    expect "a refused context menu names the folder it tried to show" \
        "cannot open file://$DESKTOP:" "$(call Status '{"method":"status"}')"
    ;;
*)
    echo "FAIL a context menu is answered with a reply or a refusal"
    echo "     got: $menu"
    exit 1
    ;;
esac
launched=$(call Launch '{"method":"launch","id":"a.txt"}')
case $launched in
*'"reply":"ok"'*) echo "ok   a launch either ran or explained itself" ;;
*"$IFACE.Error.Invalid"*)
    expect "a refused launch names the file it tried to open" \
        "cannot open file://$DESKTOP/a.txt:" "$(call Status '{"method":"status"}')"
    ;;
*)
    echo "FAIL a launch is answered with a reply or a refusal"
    echo "     got: $launched"
    exit 1
    ;;
esac
# An id that is not on the desktop is refused before anything is attempted, whatever the
# machine has installed, so a renderer cannot be talked into launching a path by naming it.
expect "a launch of an unknown id is a D-Bus error" "$IFACE.Error.NoItem" \
    "$(call Launch '{"method":"launch","id":"gone.txt"}')"

# `SIGTERM` is how a session stops the provider. The name has to go back, or a provider
# restarted during an upgrade would find the bus still claiming it.
kill "$mon" 2>/dev/null || true
kill -TERM "$pid"
wait "$pid" 2>/dev/null || true
if introspect >/dev/null 2>&1; then
    echo "FAIL the interface is still owned after SIGTERM"
    exit 1
fi
echo "ok   SIGTERM releases the name"

cat "$ROOT/provider.log"
echo "provider-bus: all checks passed"
