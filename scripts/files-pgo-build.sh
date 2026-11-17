#!/bin/sh
# Build every shipped executable with PGO followed by BOLT.
set -eu

root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
cd "$root"
host=$(rustc -vV | sed -n 's/^host: //p')
target=${1:-$host}
case "$target" in
    x86_64-*-linux-*|aarch64-*-linux-*) ;;
    *) echo "PGO+BOLT requires an x86_64 or aarch64 Linux target: $target" >&2; exit 2 ;;
esac
if [ "$target" != "$host" ]; then
    echo "PGO+BOLT training requires a native build ($host), got $target" >&2
    exit 2
fi
if [ -n "${CARGO_ENCODED_RUSTFLAGS:-}" ]; then
    echo "Use RUSTFLAGS instead of CARGO_ENCODED_RUSTFLAGS with cargo-pgo" >&2
    exit 2
fi
if [ "$(id -u)" -eq 0 ]; then
    echo "PGO+BOLT training must run as an ordinary user" >&2
    exit 2
fi
for tool in cargo cargo-pgo cargo-auditable llvm-bolt merge-fdata labwc dbus-run-session gdbus timeout python3; do
    command -v "$tool" >/dev/null || { echo "PGO+BOLT requires $tool" >&2; exit 2; }
done

# Isolate both profile sets from ordinary builds and other architectures.
base=${CARGO_TARGET_DIR:-$root/target}
mkdir -p "$base"
base=$(CDPATH='' cd -- "$base" && pwd)
stage=$base/pgo-bolt/$target
work=$stage
mkdir -p "$work/cargo-wrapper"
# cargo-pgo invokes `cargo build` internally. Keep the package's dependency
# audit section in every compilation, including the final PGO+BOLT binary.
UFLINUX_PGO_CARGO=$(command -v cargo)
export UFLINUX_PGO_CARGO
cat >"$work/cargo-wrapper/cargo" <<'WRAPPER'
#!/bin/sh
if [ "${1:-}" = build ]; then
    exec "$UFLINUX_PGO_CARGO" auditable "$@"
fi
exec "$UFLINUX_PGO_CARGO" "$@"
WRAPPER
chmod +x "$work/cargo-wrapper/cargo"
PATH="$work/cargo-wrapper:$PATH"
export PATH
export CARGO_INCREMENTAL=0
# BOLT needs symbols and relocations until its final rewrite. abuild strips
# the installed artifact later. cargo-pgo adds --emit-relocs for the BOLT steps.
export CARGO_PROFILE_RELEASE_STRIP=none
# Alpine enables packed relative relocations by default. LLVM 22 BOLT has
# RELR addend rewriting bugs; use ordinary RELA relocations for the entire
# pipeline, overriding an earlier packaging flag without dropping other flags.
RUSTFLAGS="${RUSTFLAGS:-} -Clink-arg=-Wl,-z,nopack-relative-relocs"
export RUSTFLAGS
# musl on AArch64 ignores DT_FINI, but BOLT prefers it over DT_FINI_ARRAY.
# Suppress the unused legacy tag so BOLT hooks the supported fini array and
# actually flushes its profiles. Keep this flag in every compilation stage.
case "$target" in
    aarch64-*-linux-musl*)
        RUSTFLAGS="${RUSTFLAGS:-} -Clink-arg=-Wl,-fini,0"
        export RUSTFLAGS
        ;;
esac
cargo pgo info
binaries="uflinux-files uflinux-files-provider uflinux-searchd uflinux-files-mount"
for name in $binaries; do
    work=$stage/$name
    # Discard profiles from earlier invocations before any new training.
    rm -rf "$work/pgo-profiles" "$work/bolt-profiles"
    set -- --frozen --target "$target" --target-dir "$work" -p "$name" --bin "$name"
    if [ "$name" = uflinux-files ]; then
        set -- "$@" --features desktop-icons
    fi
    binary=$work/$target/release/$name
    train() {
        if [ "$name" = uflinux-files ]; then
            timeout 180 sh tests/files-pgo-train.sh "$1"
        else
            timeout 180 dbus-run-session python3 tests/files-pgo-services.py "$name" "$1"
        fi
    }
    cargo pgo build -- "$@"
    LLVM_PROFILE_FILE="$work/pgo-profiles/$name-%m-%p.profraw" train "$binary"
    if ! find "$work/pgo-profiles" -name '*.profraw' -type f -size +0c | grep -q .; then
        echo "PGO training produced no profiles for $name" >&2
        exit 1
    fi
    cargo pgo bolt build --with-pgo -- "$@"
    train "$binary-bolt-instrumented"
    if ! find "$work/bolt-profiles/$name" -name '*.fdata' -type f -size +0c | grep -q .; then
        echo "BOLT training produced no profiles for $name" >&2
        exit 1
    fi
    cargo pgo bolt optimize --with-pgo -- "$@"
    train "$binary-bolt-optimized"
done
# Publish only after all four rewritten executables pass their workloads.
mkdir -p "$base/release"
for name in $binaries; do
    install -m755 "$stage/$name/$target/release/$name-bolt-optimized" "$base/release/$name"
done
echo "PGO+BOLT binaries: $base/release"
