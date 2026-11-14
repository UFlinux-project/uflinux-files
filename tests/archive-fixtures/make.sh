#!/bin/sh
# Regenerate the committed archive fixtures.
#
# The containers this project is tested in are minimal: the SDK image carries
# GNU tar and the four codecs, but neither `zip` nor the `7z` CLI. So the
# fixtures are committed as bytes rather than built by the test, and this
# script — run by hand on a workstation that has all three tools — is what
# produces them. The integration test in `crates/ufm-archive/tests/` reads the
# committed files and skips any container whose reader needs a program that is
# not installed, so a missing `7z` never turns a green suite red.
#
# Every container holds the same tree, listed in MEMBERS below, so the test
# asserts one set of names against all five:
#
#   notes.txt                  plain text, forty lines, big enough that a
#                              truncated container fails rather than silently
#                              reading short
#   data/numbers.csv           a second file, one level down
#   docs/readme.md             a directory with children of its own
#   docs/notes copy.txt        a space in the name
#   docs/prázdninový seznam!.txt
#                              non-ASCII, a space, and a `!` — the three
#                              characters most likely to be mangled by a shell
#                              or by a header field, and `!` is also the
#                              separator inside an `archive://` location
#   bin/run.sh                 mode 0755, to check permissions survive
#   .hidden                    a dotfile, which a listing must report and badge
#   outer.zip                  a container among the members, so the entry on
#                              the other side can be checked for the badge, the
#                              icon and the location a nested archive needs
#
# Reproducible by construction: a fixed clock, a fixed owner, and members named
# out rather than left to a directory walk. Regenerating must leave the tree
# byte-identical, and `md5sum tests/archive-fixtures/sample.* nested.tar` before
# and after is how to check that.
#
# Usage: sh tests/archive-fixtures/make.sh

set -eu

# `zip` and `7z` store a modification time as local wall-clock time with no zone,
# so the fixtures are only reproducible when the clock they read is fixed and the
# timezone is not the machine's own.
TZ=UTC
export TZ

here=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
OUT=$here
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT INT TERM

# The tree is built once and then packed by each format, so a difference between
# two fixtures is a difference in the container and nothing else.
root=$WORK/tree
mkdir -p "$root/data" "$root/docs" "$root/bin"

# Predictable text matters more than length: the test compares an extracted
# member against this same generator rather than embedding the bytes.
i=0
while [ "$i" -lt 40 ]; do
    i=$((i + 1))
    printf 'line %02d: the quick brown fox jumps over the lazy dog\n' "$i"
done >"$root/notes.txt"

printf 'id,name\n1,one\n2,dva\n3,tři\n' >"$root/data/numbers.csv"
printf '# Readme\n\nA fixture for the archive browser.\n' >"$root/docs/readme.md"
printf 'A name with a space.\n' >"$root/docs/notes copy.txt"
printf 'Názvy s diakritikou a vykřičníkem.\n' >"$root/docs/prázdninový seznam!.txt"
printf '#!/bin/sh\necho run\n' >"$root/bin/run.sh"
chmod 755 "$root/bin/run.sh"
printf 'hidden\n' >"$root/.hidden"

# A container among the members: the case where one entry on the other side of an
# archive is itself a folder. Deliberately fewer members than the outer tree, so a
# test can tell the two listings apart.
inner=$WORK/inner
mkdir -p "$inner/deep"
printf 'inner member\n' >"$inner/deep/inside.txt"
printf 'inner readme\n' >"$inner/README"

# The members of the outer tree, in the order they are to be stored. Named out
# rather than left to `zip -r` or `7z a .`, because both walk the tree and store
# what they find in `readdir` order, which is hash order on a fresh directory and
# differs between runs. A fixture whose bytes depend on directory hashing would
# drift on regeneration for a reason nobody reading this file could see.
members=$WORK/members
cat >"$members" <<'MEMBERS'
.hidden
bin/
bin/run.sh
data/
data/numbers.csv
docs/
docs/notes copy.txt
docs/prázdninový seznam!.txt
docs/readme.md
notes.txt
outer.zip
MEMBERS

# `zip -X` drops the extra fields carrying the local uid, gid and host info, which
# would otherwise change the bytes on every run. It keeps the external attributes,
# so the mode of `bin/run.sh` survives.
(cd "$inner" && find . -exec touch -h -t 202311142213.20 {} + && zip -q -X "$WORK/outer.zip" README deep/ deep/inside.txt)
cp "$WORK/outer.zip" "$root/outer.zip"

# Pin every timestamp in the outer tree now that it is complete. `tar` is given
# `--mtime` below, but `zip` and `7z` have no equivalent switch and take the
# clock, which would rewrite the fixtures on every regeneration for no reason a
# reviewer could read. This runs after the inner container is copied in, because
# `cp` stamps it with the current time and one member dated "now" makes every
# outer container differ between runs.
find "$root" -exec touch -h -t 202311142213.20 {} +

(cd "$root" && zip -q -X "$OUT/sample.zip" -@ <"$members")

# GNU tar records the mtime and the owner of each entry, so the fixtures are
# reproducible only with a fixed clock and a fixed owner. `--sort=name` removes
# the dependence on readdir order, and `--format=ustar` keeps the header shape the
# reader is documented to parse, including the `ustar` magic at offset 257. The
# tree is entered with `cd` rather than `-C` because the option list below is
# word-split by the shell and a path with a space would come apart.
tar_base="tar --format=ustar --sort=name --mtime=@1700000000 --owner=root --group=root --numeric-owner"
# shellcheck disable=SC2086
(cd "$root" && $tar_base -czf "$OUT/sample.tar.gz" .)
# shellcheck disable=SC2086
(cd "$root" && $tar_base -cJf "$OUT/sample.tar.xz" .)
# shellcheck disable=SC2086
(cd "$root" && $tar_base -cf "$OUT/nested.tar" .)

# `-ms=off` keeps solid-compression block boundaries out of the picture, so the
# fixture depends on the archive layout rather than on a packing heuristic that has
# changed between 7-Zip versions. The members are listed from the same file the zip
# uses, so the two containers hold the same tree by construction rather than by
# coincidence.
(cd "$root" && 7z a -bd -bso0 -bse0 -t7z -ms=off "$OUT/sample.7z" "@$members")

# Nothing here may be a partial write: a fixture missing a member would fail a test
# with a message about a name rather than about the generator.
for fixture in sample.zip sample.tar.gz sample.tar.xz sample.7z nested.tar; do
    if [ ! -s "$OUT/$fixture" ]; then
        echo "make.sh: $fixture was not written" >&2
        exit 1
    fi
done

ls -l "$OUT" | sed -n '2,$p'
