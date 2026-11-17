#!/bin/sh
# Representative GUI training, also used to check the final BOLT executable.
# Run on a private bus and headless compositor; no desktop or AT-SPI needed.
set -eu
binary=$(realpath "$1")
if [ "${2:-}" != --session ]; then
    exec dbus-run-session sh "$0" "$binary" --session
fi
scratch=$(mktemp -d)
app=
compositor=
cleanup() {
    status=$?
    if [ "$status" -ne 0 ]; then
        cat "$scratch/app.log" "$scratch/compositor.log" >&2 || :
    fi
    [ -z "$app" ] || kill "$app" 2>/dev/null || :
    [ -z "$compositor" ] || kill "$compositor" 2>/dev/null || :
    rm -rf "$scratch"
}
trap cleanup EXIT
trap 'exit 1' HUP INT TERM
export XDG_RUNTIME_DIR="$scratch/runtime"
export XDG_CONFIG_HOME="$scratch/config"
export XDG_DATA_HOME="$scratch/data"
export XDG_CACHE_HOME="$scratch/cache"
export UFLINUX_FILES_CONFIG_DIR="$scratch/config/uflinux/filemanager"
export UFLINUX_FILES_INDEX_DIR="$scratch/cache/index"
export XDG_DATA_DIRS="$scratch/data"
export WAYLAND_DISPLAY=wayland-0
export WLR_BACKENDS=headless WLR_RENDERER=pixman
export GDK_BACKEND=wayland GSK_RENDERER=cairo GTK_A11Y=none
mkdir -p "$XDG_RUNTIME_DIR" "$XDG_CONFIG_HOME" "$XDG_DATA_HOME" "$XDG_CACHE_HOME" \
    "$scratch/home" "$scratch/tree/small" "$scratch/tree/large/subfolder"
chmod 700 "$XDG_RUNTIME_DIR"
printf 'PGO training\n' >"$scratch/tree/small/document.txt"
printf 'hidden\n' >"$scratch/tree/large/.hidden"
i=0
while [ "$i" -lt 64 ]; do
    printf 'File %s\n' "$i" >"$scratch/tree/large/document-$i.txt"
    i=$((i + 1))
done
cp "$(dirname -- "$0")/archive-fixtures/nested.tar" "$scratch/tree/bundle.tar"
# A valid thumbnail fixture, with no external image-generation dependency.
printf '%s' 'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+ip1sAAAAASUVORK5CYII=' \
    | base64 -d >"$scratch/tree/large/000-picture.png"
env HOME="$scratch/home" labwc -C "$XDG_CONFIG_HOME" >"$scratch/compositor.log" 2>&1 &
compositor=$!
i=0
while [ ! -S "$XDG_RUNTIME_DIR/$WAYLAND_DISPLAY" ]; do
    i=$((i + 1))
    if [ "$i" -ge 100 ] || ! kill -0 "$compositor" 2>/dev/null; then
        cat "$scratch/compositor.log" >&2
        exit 1
    fi
    sleep 0.1
done
window=/org/uflinux/Files/window/1
call() {
    method=$1
    shift
    gdbus call --session --dest org.uflinux.Files --object-path "$window" \
        --method "org.gtk.Actions.$method" "$@"
}
activate() {
    call Activate "$1" "${2:-[]}" '{}' >/dev/null
}
for location in "file://$scratch/tree/small" "file://$scratch/tree/large" \
    "archive://$scratch/tree/bundle.tar!/" computer:// trash://; do
    env HOME="$scratch/home" "$binary" "$location" >"$scratch/app.log" 2>&1 &
    app=$!
    i=0
    until call List >"$scratch/actions.log" 2>&1; do
        i=$((i + 1))
        if [ "$i" -ge 100 ] || ! kill -0 "$app" 2>/dev/null; then
            echo "GUI training could not reach org.uflinux.Files at $window ($location)" >&2
            if ! kill -0 "$app" 2>/dev/null; then
                if wait "$app"; then
                    app_status=0
                else
                    app_status=$?
                fi
                echo "GUI training process exited with status $app_status: $binary" >&2
                app=
            fi
            cat "$scratch/actions.log" "$scratch/app.log" >&2
            exit 1
        fi
        sleep 0.1
    done
    sleep 1
    for mode in list columns grid; do
        activate mode "[<'$mode'>]"
        sleep 0.5
    done
    call SetState hidden '<true>' '{}' >/dev/null
    activate refresh
    sleep 0.5
    if [ "$location" = "file://$scratch/tree/large" ]; then
        activate up
        sleep 0.5
        activate back
        sleep 0.5
        activate forward
        sleep 0.5
    fi
    # Normal shutdown runs the LLVM/BOLT atexit hooks that flush profiles.
    activate close
    if ! wait "$app"; then
        cat "$scratch/app.log" >&2
        exit 1
    fi
    app=
done
echo "GUI training passed: $binary"
