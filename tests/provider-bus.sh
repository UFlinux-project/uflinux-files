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
mkdir -p "$ROOT/runtime" "$ROOT/home/Desktop" "$ROOT/home/.config"
chmod 700 "$ROOT/runtime"
# A desktop with two entries, so the model has something to place and `Trash` has
# something to remove. The names below are the ids the calls send.
: >"$ROOT/home/Desktop/a.txt"
: >"$ROOT/home/Desktop/b.txt"

XDG_RUNTIME_DIR=$ROOT/runtime
HOME=$ROOT/home
export XDG_RUNTIME_DIR HOME

# One request, one reply. The body is a single JSON string and the interface declares one
# `s` argument, so the body goes on the command line as it is: `gdbus` parses each
# argument as a value of the declared type, and a string is one of the few types whose
# text form tolerates the JSON quotes being left alone. Wrapping the body in a tuple —
# which is how a reply is printed, and a tempting thing to copy — does not work, because
# then the argument is a tuple where a string was declared and the call never arrives.
call() {
    gdbus call --session -d "$IFACE" -o "$OBJ" -m "$IFACE.$1" "$2" 2>&1
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

# `Status` before any renderer has talked to us: nobody is listening.
expect "Status reports no renderer yet" '"renderer":false' \
    "$(call Status '{"method":"status"}')"

# A grid, then the model — the order a renderer uses on startup.
expect "SetGeometry is accepted" '"reply":"ok"' \
    "$(call SetGeometry '{"method":"set-geometry","monitors":[{"monitor":0,"columns":4,"rows":4,"cell_w":96,"cell_h":96}]}')"
model=$(call GetModel '{"method":"get-model"}')
expect "GetModel carries both icons" '"id":"a.txt"' "$model"
expect "the model carries the grid" '"columns":4' "$model"
expect "every item names a fallback icon" '"icon"' "$model"

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
[ ! -e "$ROOT/home/Desktop/b.txt" ] || {
    echo "FAIL the trashed file is still in the desktop directory"
    exit 1
}
# The count falls when the watcher's debounce expires, so this waits rather than assuming
# a number of milliseconds.
i=0
while [ "$i" -lt 100 ]; do
    status=$(call Status '{"method":"status"}')
    if printf '%s' "$status" | grep -q '"items":1'; then break; fi
    i=$((i + 1))
    sleep 0.1
done
expect "the provider noticed the delete" '"items":1' "$status"
expect "Status now reports a renderer" '"renderer":true' "$status"

# `SIGTERM` is how a session stops the provider. The name has to go back, or a provider
# restarted during an upgrade would find the bus still claiming it.
kill -TERM "$pid"
wait "$pid" 2>/dev/null || true
if introspect >/dev/null 2>&1; then
    echo "FAIL the interface is still owned after SIGTERM"
    exit 1
fi
echo "ok   SIGTERM releases the name"

cat "$ROOT/provider.log"
echo "provider-bus: all checks passed"
