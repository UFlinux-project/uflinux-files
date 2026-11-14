#!/bin/sh
#
# End-to-end checks for the mount path that a unit test cannot make: a real
# `mount(8)` invocation, a real `/proc/self/mountinfo`, a real `fusermount3`, and
# a real `uflinux-files --sweep-mounts` process.
#
# The Rust tests cover the decisions — which argv a plan builds, which option is
# refused, what a sweep proposes — against a synthetic mount table and stub
# helpers. What they cannot show is that the helper's argv survives `exec`, that
# the kernel agrees a mount happened, that a dead FUSE daemon really does leave a
# mount point answering `ENOTCONN`, or that a password stays out of the process
# list while real children are running. Those are what is asserted here, and they
# are the four claims `docs/validation.md` makes about this file.
#
# Needs to be able to mount, which means one of:
#
#   podman run --cap-add=CAP_SYS_ADMIN --device /dev/fuse ... \
#       sh tests/mount-fixture.sh target/debug/uflinux-files target/debug/uflinux-files-mount
#   # the sweep half additionally wants: apk add sshfs fuse3 openssh-server
#
# Anything less skips rather than fails. A check that cannot mount proves nothing,
# and reporting that as a failure would bury the checks that can run.
#
# Exits 0 when every check that could run passed.

set -eu

BIN=${1:-target/debug/uflinux-files}
HELPER=${2:-target/debug/uflinux-files-mount}
PASS='S3cr3t-n0t-in-argv'
# The helper insists a share is mounted for a user and not for root, so it wants
# `PKEXEC_UID` when it runs setuid, and the mount point has to belong to that uid
# with mode 700. The fixture plays the part of pkexec: it sets the variable and
# hands the mount points to the uid it names.
FIXTURE_UID=${FIXTURE_UID:-$(id -u nobody 2>/dev/null || echo 65534)}
# `Share::slug()` of the two URIs below, so that the mount points are where the
# loader rebuilds them to be. `uri.rs` covers the slug rules themselves; here they
# are inputs, and a wrong one shows up as a sweep that finds nothing to do.
GONE_ID=localhost-gone-b1d14a
LIVE_ID=localhost-live-555d5b
CHECKS=0
SKIPPED=

fail() {
    printf 'not ok - %s\n' "$1" >&2
    [ $# -gt 1 ] && printf '%s\n' "$2" >&2
    exit 1
}

ok() {
    CHECKS=$((CHECKS + 1))
    printf 'ok - %s\n' "$1"
}

skip() {
    SKIPPED="$SKIPPED $1"
    printf 'skip - %s\n' "$1"
}

note() {
    printf '# %s\n' "$1"
}

[ -x "$BIN" ] || fail "the file manager binary is missing" "build it first, or pass it as \$1 (looked for at $BIN)"
[ -x "$HELPER" ] || fail "the mount helper is missing" "build it first, or pass it as \$2 (looked for at $HELPER)"
case "$(id -u)" in
    0) ;;
    *)
        skip "needs root: it installs a mount shim into /usr/bin and chowns mount points"
        note "no check ran; nothing here is certified"
        exit 0
        ;;
esac

WORK=$(mktemp -d) || fail "cannot make a working directory"
# Left behind on failure so a failed run can be read, and removed otherwise. The
# status is handed to the trap rather than read inside it: what a shell exits with
# once an EXIT trap has run is the status of the trap's own last command, and the
# last thing this does is usually a `umount` that found nothing to detach.
KEEP=1
cleanup() {
    status=$1
    for point in "$WORK"/*/*; do
        # Detaching something that is not mounted is the normal case here, and a
        # failing command under `set -e` ends the function before the exit below,
        # which would report the status of the last `umount` instead of the run's.
        if [ -d "$point" ]; then umount -l "$point" 2>/dev/null || :; fi
    done
    if [ "$KEEP" = 1 ]; then
        printf '# the working directory is left at %s\n' "$WORK" >&2
    else
        rm -rf "$WORK"
    fi
    exit "$status"
}
trap 'cleanup $?' EXIT

for dir in mnt conf net bin; do
    mkdir -p "$WORK/$dir"
done
chmod 700 "$WORK/mnt" "$WORK/conf"
LOG=$WORK/argv.log
: >"$LOG"
printf '%s\n' "$PASS" >"$WORK/secret"

# A mount point the helper will accept: owned by the uid it was told about, mode
# 700, empty, and not a symlink.
point() {
    mkdir -p "$WORK/mnt/$1"
    chown "$FIXTURE_UID" "$WORK/mnt/$1" ||
        fail "cannot hand a mount point to uid $FIXTURE_UID" \
            "run this outside a user namespace that maps only one uid, or set FIXTURE_UID"
    chmod 700 "$WORK/mnt/$1"
    printf '%s\n' "$WORK/mnt/$1"
}

# ---------------------------------------------------------------------------
# Can we mount at all, and can a mount be made stale?
# ---------------------------------------------------------------------------

PROBE=$WORK/mnt/probe
mkdir -p "$PROBE"
if ! mount -t tmpfs ufm-probe "$PROBE" 2>/dev/null; then
    skip "cannot mount tmpfs here: run this with CAP_SYS_ADMIN or in a mount namespace"
    note "no check ran; nothing here is certified"
    KEEP=0
    exit 0
fi
umount "$PROBE" 2>/dev/null || umount -l "$PROBE"
rmdir "$PROBE"
ok "this environment can mount"

# `Stale` needs a FUSE entry in the kernel's table whose daemon stopped
# answering, which only a real FUSE filesystem can produce. A synthetic
# mountinfo would test the parser, which the unit tests already do, so the sweep
# half skips rather than pretending. Installing what it needs happens before the
# mount shim exists, so that the package manager is not talking to a fake.
HAVE_SSHFS=
if command -v sshfs >/dev/null 2>&1 && command -v fusermount3 >/dev/null 2>&1; then
    HAVE_SSHFS=1
elif command -v apk >/dev/null 2>&1 &&
    apk add --no-cache sshfs fuse3 openssh-server openssh-keygen \
        openssh-client >/dev/null 2>&1 &&
    command -v sshfs >/dev/null 2>&1 && command -v fusermount3 >/dev/null 2>&1; then
    HAVE_SSHFS=1
fi
[ -n "$HAVE_SSHFS" ] || skip "the sweep needs sshfs and fusermount3 to make a mount genuinely stale"

# ---------------------------------------------------------------------------
# A `mount` that records what it was asked to do.
# ---------------------------------------------------------------------------
#
# The helper resolves `mount` and `umount` from a fixed list of directories and
# ignores `$PATH` — that is the point of it, so that a hostile `$PATH` cannot
# phrase the helper — so the shim has to go where the helper will actually look.
# Both directories are written because the list names both and because on some
# systems only one of them is real.
#
# The shim then mounts `tmpfs` whatever it was asked for. That is deliberate: what
# is being certified is the helper's checks, the exact argv it hands over, and
# that a mount the helper reports really reaches the kernel. Whether a real NFS
# server answers is the server's business, and a `mount.nfs` waiting on a host
# that does not exist would make the fixture slow rather than more honest. The
# sshfs half does use a real filesystem, and it is the half that has to.
REAL_MOUNT=$(command -v mount)
REAL_UMOUNT=$(command -v umount)
mkdir -p "$WORK/real"
cp "$REAL_MOUNT" "$WORK/real/mount"
cp "$REAL_UMOUNT" "$WORK/real/umount"
REAL_MOUNT=$WORK/real/mount
REAL_UMOUNT=$WORK/real/umount
shim() {
    cat >"$1" <<'SHIM'
#!/bin/sh
# The helper hands the verb over as the program it execs, not as an argument, so
# the record starts with the word it was invoked by. That is also what makes one
# file serve both jobs.
printf '%s\n' "$0" | sed 's|.*/||' >"$UFM_ARGV_LOG"
printf '%s\n' "$@" >>"$UFM_ARGV_LOG"
for arg in "$@"; do
    case $arg in
        *ufm-fail*)
            printf 'mount: the fixture asked for a failure\n' >&2
            exit 32
            ;;
    esac
done
case $0 in
    */mount)
        for arg in "$@"; do
            # Give the process-list scanner a window to look inside a live child.
            [ "$arg" = cifs ] && sleep 1
        done
        target=''
        while [ $# -gt 0 ]; do
            target=$1
            shift
        done
        # Owned like the directory it covers, so that a mount point which is
        # already a mount still reads as the caller's own: path lookup crosses
        # the mount and reports the mounted root rather than the directory under
        # it, which is what the helper's ownership check ends up looking at.
        exec @MOUNT@ -t tmpfs -o uid=$FIXTURE_UID,gid=$FIXTURE_UID,mode=700 \
            ufm-fixture "$target"
        ;;
esac
exec @UMOUNT@ "$@"
SHIM
    sed "s|@MOUNT@|$REAL_MOUNT|; s|@UMOUNT@|$REAL_UMOUNT|" "$1" >"$1.new"
    cat "$1.new" >"$1"
    rm -f "$1.new"
    chmod 755 "$1"
}
for dir in /usr/bin /bin; do
    [ -d "$dir" ] && [ -w "$dir" ] || continue
    shim "$dir/mount"
    shim "$dir/umount"
done
export UFM_ARGV_LOG=$LOG
export FIXTURE_UID
cp "$HELPER" "$WORK/bin/uflinux-files-mount"
PATH="$WORK/bin:$PATH"
export PATH
export UFLINUX_FILES_MOUNT_ROOT=$WORK/mnt
export UFLINUX_FILES_CONFIG_DIR=$WORK/conf
export UFLINUX_FILES_NETWORK_DIR=$WORK/net
export PKEXEC_UID=$FIXTURE_UID
ok "a mount shim is installed where the helper looks"

# ---------------------------------------------------------------------------
# 1. A mount the kernel agrees with, built from the argv the plan described.
# ---------------------------------------------------------------------------

NFS_POINT=$(point srv-export)
out=$("$WORK/bin/uflinux-files-mount" mount --type nfs --source srv:/export \
    --target "$NFS_POINT" --options noatime 2>&1) && code=0 || code=$?
[ "$code" = 0 ] || fail "a plain NFS mount should succeed" "exit $code: $out"
grep -q ' / '"$NFS_POINT"' ' /proc/self/mountinfo ||
    fail "the kernel does not list the mount the helper just made"
ok "the helper mounts, and /proc/self/mountinfo agrees"

# The recorded argv is the whole safety argument for having a helper at all:
# every word has to be one the plan chose, in the position the plan put it, and
# nothing parsed out of a user's URI may arrive somewhere `mount` would read it as
# a flag. Reading the log back as words rather than grepping it is what makes the
# positions part of the check.
set --
while IFS= read -r word; do set -- "$@" "$word"; done <"$LOG"
[ $# = 7 ] || fail "the helper handed mount the wrong number of arguments" \
    "expected 7, got $# : $(tr '\n' ' ' <"$LOG")"
case $2$3$4$5$6 in
    -tnfs-onoatime*) ;;
    *) fail "mount was not given the type and options the plan named" "$(tr '\n' ' ' <"$LOG")" ;;
esac
[ "$6" = srv:/export ] || fail "the source reached mount in the wrong position" "$(tr '\n' ' ' <"$LOG")"
[ "$7" = "$NFS_POINT" ] || fail "the mount point reached mount in the wrong position" "$(tr '\n' ' ' <"$LOG")"
ok "the argv handed to mount is exactly the one the plan described"

# ---------------------------------------------------------------------------
# 2. A failure leaves nothing half-mounted.
# ---------------------------------------------------------------------------

FAIL_POINT=$(point fail-share)
out=$("$WORK/bin/uflinux-files-mount" mount --type nfs --source srv:/ufm-fail \
    --target "$FAIL_POINT" --options noatime 2>&1) && code=0 || code=$?
[ "$code" = 1 ] || fail "a failing mount must exit 1, not $code" "$out"
grep -q ' / '"$FAIL_POINT"' ' /proc/self/mountinfo &&
    fail "a failed mount left a kernel entry behind"
[ -d "$FAIL_POINT" ] || fail "a failed mount removed the mount point"
[ -z "$(ls -A "$FAIL_POINT")" ] || fail "a failed mount left something in the mount point"
ok "a failed mount leaves an empty directory and no kernel entry"

# ---------------------------------------------------------------------------
# 3. A password never reaches the process list.
# ---------------------------------------------------------------------------
#
# Scanned from a background loop rather than after the fact: the window in which a
# secret is exposed is while the mount child is running, and a check that looks
# afterwards would pass even if the password had been in argv the whole time. The
# scanner reads the secret from a file, because a `grep` handed it as an argument
# would be one more process advertising it.

CRED=$WORK/cred.conf
cp "$WORK/secret" "$CRED"
chmod 600 "$CRED"
chown "$FIXTURE_UID" "$CRED"
CIFS_POINT=$(point srv-share)

leaked=$WORK/leaked
: >"$leaked"
(
    while :; do
        for proc in /proc/[0-9]*/cmdline; do
            # `cat` rather than a redirection: a process can exit between the glob
            # and the read, and a failed redirection is the shell's own error,
            # which no redirection of stderr would silence.
            cat "$proc" 2>/dev/null | tr '\0' ' ' | grep -q -f "$WORK/secret" &&
                printf '%s\n' "$proc" >>"$leaked"
        done
    done
) &
SCANNER=$!
"$WORK/bin/uflinux-files-mount" mount --type cifs --source '//srv/share' \
    --target "$CIFS_POINT" --options "username=ada,credentials=$CRED" >/dev/null 2>&1 || true
kill "$SCANNER" 2>/dev/null || true
wait "$SCANNER" 2>/dev/null || true
[ ! -s "$leaked" ] || fail "the password appeared in a process's command line" "$(cat "$leaked")"
grep -q -- "$PASS" "$LOG" && fail "the password was passed to mount in argv" "$(cat "$LOG")"
ok "a password mount keeps the secret out of every /proc/*/cmdline"

# `pass=` is the option that would have put it there, and the helper refuses it
# before anything runs. Checked here as well as in the unit test because the
# refusal has to hold for a request that arrived through argv.
out=$("$WORK/bin/uflinux-files-mount" mount --type cifs --source '//srv/share' \
    --target "$CIFS_POINT" --options "username=ada,pass=$PASS" 2>&1) && code=0 || code=$?
[ "$code" = 2 ] || fail "pass= should be refused outright, not run" "exit $code: $out"
case "$out" in
    *credentials*) ;;
    *) fail "the refusal does not name the option to use instead" "$out" ;;
esac
ok "an option that would put a password in argv is refused"

# A credentials file someone else can read is the same leak by another route. The
# mount point is still empty at this point, so the refusal cannot be the emptiness
# check answering first.
chmod 644 "$CRED"
out=$("$WORK/bin/uflinux-files-mount" mount --type cifs --source '//srv/share' \
    --target "$CIFS_POINT" --options "username=ada,credentials=$CRED" 2>&1) && code=0 || code=$?
[ "$code" = 2 ] || fail "a group-readable password file should be refused" "exit $code: $out"
chmod 600 "$CRED"
ok "a password file anyone can read is refused"

# A symlink in the mount root would let the caller aim root at a directory it does
# not own, which is what the mount-point rules exist to prevent.
ln -s "$NFS_POINT" "$WORK/mnt/sneaky"
out=$("$WORK/bin/uflinux-files-mount" mount --type nfs --source srv:/export \
    --target "$WORK/mnt/sneaky" --options '' 2>&1) && code=0 || code=$?
[ "$code" = 2 ] || fail "a symlink mount point should be refused" "exit $code: $out"
rm -f "$WORK/mnt/sneaky"
ok "a symlink mount point is refused rather than followed"

# Mounting over a mount that is already up is a leak rather than a remount, and
# the point is mounted from the check above.
out=$("$WORK/bin/uflinux-files-mount" mount --type nfs --source srv:/export \
    --target "$NFS_POINT" --options '' 2>&1) && code=0 || code=$?
[ "$code" = 0 ] || fail "mounting over a live mount should report success, not $code" "$out"
case "$out" in
    *already*) ;;
    *) fail "mounting over a live mount did not say so" "$out" ;;
esac
ok "mounting over a live mount says so instead of hiding the old one"

# ---------------------------------------------------------------------------
# 4. Unmounting really detaches.
# ---------------------------------------------------------------------------

for spec in "nfs srv:/export $NFS_POINT" "cifs //srv/share $CIFS_POINT"; do
    set -- $spec
    "$WORK/bin/uflinux-files-mount" unmount --type "$1" --source "$2" \
        --target "$3" --options '' >/dev/null ||
        fail "unmounting a live mount should succeed" "$3"
    grep -q ' / '"$3"' ' /proc/self/mountinfo &&
        fail "the kernel still lists a mount the helper unmounted" "$3"
    [ -d "$3" ] || fail "the helper removed a directory whose lifecycle it does not own" "$3"
    # The second call is the no-op, and `umount` says so on stderr. The exit code
    # is the claim being checked, not the wording.
    "$WORK/bin/uflinux-files-mount" unmount --type "$1" --source "$2" \
        --target "$3" --options '' 2>/dev/null ||
        fail "unmounting twice must be a no-op and not a failure" "$3"
done
ok "unmounting detaches, and doing it again is not an error"

# Clear the tree so that the sweep sees only what its own records point at.
rmdir "$NFS_POINT" "$CIFS_POINT" "$FAIL_POINT" ||
    fail "a mount point was still busy after a successful unmount"

# ---------------------------------------------------------------------------
# 5. The sweep: a stale mount goes, a live one stays.
# ---------------------------------------------------------------------------

if [ -z "$HAVE_SSHFS" ]; then
    note "the sweep was not exercised, so the stale-mount path is uncertified"
else
    # A local sshd, so the FUSE daemon has a real server to talk to and killing it
    # is what a crashed daemon looks like from the file manager's side.
    KEYDIR=$WORK/ssh
    mkdir -p "$KEYDIR" /run/sshd "$HOME/.ssh"
    [ -f /etc/ssh/ssh_host_ed25519_key ] ||
        ssh-keygen -q -t ed25519 -f /etc/ssh/ssh_host_ed25519_key -N ''
    ssh-keygen -q -t ed25519 -f "$KEYDIR/id" -N ''
    cat "$KEYDIR/id.pub" >>"$HOME/.ssh/authorized_keys"
    chmod 600 "$HOME/.ssh/authorized_keys"
    /usr/sbin/sshd -D >/dev/null 2>&1 &
    SSHD=$!
    sleep 1

    # `-f` keeps the daemon in the foreground, which is what makes its pid the
    # shell's background job and therefore something the fixture can kill without
    # also killing the other mount. The mount points are made beforehand rather
    # than inside the call: a command substitution in the arguments of a
    # backgrounded command leaves `$!` pointing at the subshell, and killing that
    # would orphan the daemon instead of stopping it.
    GONE_POINT=$(point "$GONE_ID")
    LIVE_POINT=$(point "$LIVE_ID")
    ssh_serve() {
        dir=$WORK/remote-$1
        mkdir -p "$dir"
        printf 'from the other side\n' >"$dir/probe.txt"
        sshfs -f -o "StrictHostKeyChecking=no,UserKnownHostsFile=/dev/null,IdentityFile=$KEYDIR/id,IdentitiesOnly=yes,LogLevel=ERROR" \
            "localhost:$dir" "$2" &
    }
    ssh_serve gone "$GONE_POINT"
    GONE=$!
    ssh_serve live "$LIVE_POINT"
    LIVE=$!
    sleep 1
    grep -q ' / '"$WORK"'/mnt/'"$LIVE_ID"' ' /proc/self/mountinfo ||
        fail "sshfs mounted but the kernel does not list it"
    cat "$WORK/mnt/$LIVE_ID/probe.txt" >/dev/null ||
        fail "the sshfs mount does not serve its contents"
    ok "sshfs mounts a directory over a real connection and serves it"

    # The stored mount points are ones that no longer exist, because the mount
    # root is per boot and the loader is supposed to rebuild them from the id
    # rather than trust what is on disk.
    NOW=$(date +%s)
    OLD=$((NOW - 4000))
    cat >"$WORK/conf/mounts.json" <<JSON
{"version":1,"mounts":[
 {"id":"$GONE_ID","share":{"protocol":"sftp","host":"localhost","port":null,"user":null,"tls":false,"resource":"/srv/gone","options":[]},"mountpoint":"/run/another/boot/$GONE_ID","autostart":false,"mounted_at":$OLD,"last_used":$OLD,"last_error":null},
 {"id":"$LIVE_ID","share":{"protocol":"sftp","host":"localhost","port":null,"user":null,"tls":false,"resource":"/srv/live","options":[]},"mountpoint":"/run/another/boot/$LIVE_ID","autostart":false,"mounted_at":$OLD,"last_used":$OLD,"last_error":null}
]}
JSON

    # Kill one FUSE daemon without detaching. The kernel keeps the entry and the
    # mount point starts answering `ENOTCONN`, which is exactly what a dead sshfs
    # leaves behind and the state a sweep exists to notice.
    kill -9 "$GONE" || fail "the fixture could not kill its own FUSE daemon"
    wait "$GONE" 2>/dev/null || true
    sleep 1
    grep -q ' / '"$WORK"'/mnt/'"$GONE_ID"' ' /proc/self/mountinfo ||
        fail "killing the FUSE daemon did not leave a stale entry to find"
    ls "$WORK/mnt/$GONE_ID" >/dev/null 2>&1 &&
        fail "a dead FUSE daemon should make its mount point unreadable"
    ok "a dead FUSE daemon leaves a mount point that does not answer"

    # A password file whose share is not mounted is a leftover, and so is an empty
    # directory with no record behind it. A non-empty one is not, because it may
    # hold someone's files.
    mkdir -p "$WORK/conf/mounts"
    cp "$WORK/secret" "$WORK/conf/mounts/gone-share.conf"
    chmod 600 "$WORK/conf/mounts/gone-share.conf"
    mkdir -p "$WORK/mnt/orphan-empty" "$WORK/mnt/orphan-full"
    printf 'x\n' >"$WORK/mnt/orphan-full/kept.txt"

    report=$("$BIN" --sweep-mounts --json)
    printf '%s\n' "$report" >"$WORK/sweep.json"
    grep -q '"action":"detach"' "$WORK/sweep.json" ||
        fail "the sweep did not detach the mount that does not answer" "$report"
    grep -q '"action":"rmdir"' "$WORK/sweep.json" ||
        fail "the sweep did not remove an empty mount point with no share behind it" "$report"
    grep -q '"action":"rm"' "$WORK/sweep.json" ||
        fail "the sweep did not remove a leftover password file" "$report"
    grep -q 'orphan-full' "$WORK/sweep.json" ||
        fail "a non-empty orphan should be reported, not silently kept" "$report"
    ok "the sweep reports a stale mount, an orphan directory and a leftover password"

    # The live share is idle past any reasonable limit and still must survive:
    # without `--idle-after` the sweep only clears what is provably dead, and a
    # share the kernel says is mounted is not that.
    grep -q ' / '"$WORK"'/mnt/'"$LIVE_ID"' ' /proc/self/mountinfo ||
        fail "the sweep detached a share that is mounted and merely idle" "$(cat "$WORK/sweep.json")"
    grep -q "$LIVE_ID" "$WORK/sweep.json" &&
        fail "a live mount was offered for detaching without being asked to" "$(cat "$WORK/sweep.json")"
    ok "a live mount is left alone when only leftovers were asked for"

    # Everything the report claimed to have done has to have happened, and nothing
    # else: a sweep that lies about a detach leaves a share the user cannot eject.
    [ ! -e "$WORK/mnt/$GONE_ID" ] || fail "the stale mount point was left behind"
    grep -q ' / '"$WORK"'/mnt/'"$GONE_ID"' ' /proc/self/mountinfo &&
        fail "the stale kernel entry outlived the sweep that detached it"
    [ ! -e "$WORK/conf/mounts/gone-share.conf" ] ||
        fail "the leftover password file outlived the sweep"
    [ ! -e "$WORK/mnt/orphan-empty" ] || fail "an empty orphan directory outlived the sweep"
    [ -f "$WORK/mnt/orphan-full/kept.txt" ] ||
        fail "the sweep deleted a file inside a directory it only reported on"
    grep -q "\"$LIVE_ID\"" "$WORK/conf/mounts.json" ||
        fail "the live share disappeared from the table" "$(cat "$WORK/conf/mounts.json")"
    grep -q "\"$GONE_ID\"" "$WORK/conf/mounts.json" &&
        fail "a detached share is still listed as though it were connectable" \
        "$(cat "$WORK/conf/mounts.json")"
    ok "the table on disk matches what is actually mounted"

    # Asked about idleness this time, so the live share is offered and detached.
    # This is the path a session manager takes when the last window closes, and
    # for a FUSE share it goes through `fusermount3` rather than the privileged
    # helper, so it has to work with no pkexec and therefore no prompt.
    report=$("$BIN" --sweep-mounts --json --idle-after 1)
    printf '%s\n' "$report" >"$WORK/sweep2.json"
    grep -q "$LIVE_ID" "$WORK/sweep2.json" ||
        fail "an idle mounted share was not offered for detaching" "$report"
    grep -q 'not been used' "$WORK/sweep2.json" ||
        fail "the reason given does not say why the share was offered" "$report"
    grep -q ' / '"$WORK"'/mnt/'"$LIVE_ID"' ' /proc/self/mountinfo &&
        fail "the report says a detach happened but the mount is still there" "$report"
    [ ! -e "$WORK/mnt/$LIVE_ID" ] ||
        fail "an idle share was detached but its mount point was left behind"
    ok "an idle share is detached without privilege and its mount point with it"

    # The user's own files, left where they were.
    rm -rf "$WORK/mnt/orphan-full"

    # The plain form of the option, which is what a session manager runs, has
    # nothing left to say once the tree is clean.
    "$BIN" --sweep-mounts >"$WORK/sweep3.txt" || fail "a sweep over a clean tree should succeed"
    grep -q 'nothing to clean up' "$WORK/sweep3.txt" ||
        fail "a clean tree should be reported as clean" "$(cat "$WORK/sweep3.txt")"
    ok "a clean tree is reported as clean"

    kill "$SSHD" 2>/dev/null || true
fi

# ---------------------------------------------------------------------------
# 6. Nothing outside the sandbox was touched.
# ---------------------------------------------------------------------------

[ -e "$HOME/.config/uflinux/filemanager/mounts.json" ] &&
    fail "the run wrote to the real configuration directory"
ok "nothing was written outside the sandbox"

KEEP=0
if [ -n "$SKIPPED" ]; then
    note "skipped:$SKIPPED"
fi
printf '# %d checks passed\n' "$CHECKS"
