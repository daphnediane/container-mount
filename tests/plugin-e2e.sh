#!/usr/bin/env bash
# End-to-end test of the `container mount` plugin path: no create-time
# flags, just `container copy` delivery, a `container exec` launch, and
# the apiserver vsock dial — against containers we didn't set up for it.
#
# Targets:
#   alpine        plain `container run`
#   amd64         an amd64 image (Rosetta; the arm64 server still runs)
#   scratch       FROM scratch with only a static binary, whose own
#                 environment defines CONTAINER_MOUNT_TOKEN (the exec
#                 env-merge trap — docs/design/container-internals.md)
#   machine       a `container machine`, by name (--machine; slow)
#
#   tests/plugin-e2e.sh [--machine] [--keep]
#
# Apple `container` on macOS only; skips cleanly without it.
# cspell:ignore nosuchuser lst

set -euo pipefail
cd "$(dirname "$0")/.."

MACHINE=0
KEEP=0
while [ $# -gt 0 ]; do
    case "$1" in
        --machine) MACHINE=1; shift ;;
        --keep) KEEP=1; shift ;;
        -h|--help) sed -n '2,16p' "$0"; exit 0 ;;
        *) echo "plugin-e2e: unknown argument $1" >&2; exit 2 ;;
    esac
done

log() { printf 'plugin-e2e: %s\n' "$*" >&2; }
die() { log "$*"; exit 1; }

[ "$(uname -s)" = Darwin ] || { log "not macOS — skipping"; exit 0; }
command -v container >/dev/null 2>&1 || { log "no container CLI — skipping"; exit 0; }
container system status >/dev/null 2>&1 || { log "container services not running — skipping"; exit 0; }

case "$(uname -m)" in
    arm64|aarch64) TARGET=aarch64-unknown-linux-musl ;;
    *) TARGET=x86_64-unknown-linux-musl ;;
esac
log "building $TARGET server and host binary"
cargo build --release --target "$TARGET" >/dev/null
cargo build >/dev/null
SERVER="target/$TARGET/release/container-mount"
CLIENT="target/debug/container-mount"

TAG="cm-plugin-e2e-$$"
SCRATCH="scratch/$TAG"
mkdir -p "$SCRATCH/image"
NAMES=()
cleanup() {
    [ "$KEEP" = 1 ] && return
    for n in "${NAMES[@]}"; do container delete -f "$n" >/dev/null 2>&1 || true; done
    [ "$MACHINE" = 1 ] && container machine delete "$TAG-machine" >/dev/null 2>&1 || true
    container image delete "$TAG-scratch" >/dev/null 2>&1 || true
    rm -rf "$SCRATCH"
}
trap cleanup EXIT

# --dev: the in-tree server build, not an installed one.
plugin() { "$CLIENT" --dev plugin "$@"; }

fail=0
pass() { log "PASS $*"; }
flunk() { log "FAIL $*"; fail=1; }

# Round trip through one target; `$2` is 1 when the image has a shell.
exercise() {
    local name="$1" shell="$2"
    plugin ls "$name:/" >/dev/null && pass "$name ls" || { flunk "$name ls"; return; }
    echo "host-payload-$name" > "$SCRATCH/up-$name.txt"
    plugin copy "$SCRATCH/up-$name.txt" "$name:/up.txt" && pass "$name copy in" || flunk "$name copy in"
    plugin cat "$name:/up.txt" | grep -q "host-payload-$name" \
        && pass "$name cat" || flunk "$name cat"
    plugin copy "$name:/up.txt" "$SCRATCH/down-$name.txt" \
        && cmp -s "$SCRATCH/up-$name.txt" "$SCRATCH/down-$name.txt" \
        && pass "$name copy out" || flunk "$name copy out"
    # Relative paths land in the default user's home.
    plugin copy "$SCRATCH/up-$name.txt" "$name:rel-$name.txt" \
        && plugin cat "$name:~/rel-$name.txt" | grep -q "host-payload-$name" \
        && pass "$name home-relative" || flunk "$name home-relative"
    plugin copy "$name:/up.txt" "$name:/up-copy.txt" \
        && plugin cat "$name:/up-copy.txt" | grep -q "host-payload-$name" \
        && pass "$name in-guest copy" || flunk "$name in-guest copy"
    if [ "$shell" = 1 ]; then
        container exec "$name" /bin/sh -c "grep -q host-payload-$name /up.txt" \
            && pass "$name copy visible in guest" || flunk "$name copy visible in guest"
    fi
    # Each command's server copy deletes itself once running.
    left="$(plugin ls "$name:/root/.container-mount")"
    [ -z "$left" ] && pass "$name no leftover server" || flunk "$name leftover: $left"
}

log "starting targets"
container run -d --name "$TAG-alpine" alpine:latest sleep 600 >/dev/null
NAMES+=("$TAG-alpine")
container run -d --platform linux/amd64 --name "$TAG-amd64" alpine:latest sleep 600 >/dev/null
NAMES+=("$TAG-amd64")

cp "$SERVER" "$SCRATCH/image/container-mount"
printf 'FROM scratch\nCOPY container-mount /container-mount\n' > "$SCRATCH/image/Dockerfile"
container build -t "$TAG-scratch" -f "$SCRATCH/image/Dockerfile" "$SCRATCH/image" >/dev/null
# The workload is the server itself, under the container's own token.
CONTAINER_MOUNT_TOKEN="$(od -An -N32 -tx1 /dev/urandom | tr -d ' \n')" \
    container run -d --name "$TAG-scratch" -e CONTAINER_MOUNT_TOKEN "$TAG-scratch" \
    /container-mount serve --listen tcp:127.0.0.1:1 --idle-secs 600 >/dev/null
NAMES+=("$TAG-scratch")

exercise "$TAG-alpine" 1
exercise "$TAG-amd64" 1
exercise "$TAG-scratch" 0

if [ "$MACHINE" = 1 ]; then
    log "creating a machine (slow)"
    container machine create -n "$TAG-machine" alpine:3.22 >/dev/null
    exercise "$TAG-machine" 0
fi

# Users: relative paths and new files follow --user, and ~USER works.
container exec "$TAG-alpine" adduser -D -u 1031 -h /home/tester tester >/dev/null 2>&1
plugin copy -u tester "$SCRATCH/up-$TAG-alpine.txt" "$TAG-alpine:mine.txt" \
    && [ "$(container exec "$TAG-alpine" stat -c %u:%g:%n /home/tester/mine.txt)" \
        = "1031:1031:/home/tester/mine.txt" ] \
    && pass "--user owns and homes the copy" || flunk "--user copy"
plugin cat "$TAG-alpine:~tester/mine.txt" | grep -q host-payload \
    && pass "~USER path" || flunk "~USER path"
plugin cat "tester@$TAG-alpine:mine.txt" | grep -q host-payload \
    && pass "USER@TARGET path" || flunk "USER@TARGET path"
# USER@ writes as that user: their home, their uid and primary group.
container exec "$TAG-alpine" addgroup -g 2000 extra >/dev/null 2>&1
container exec "$TAG-alpine" addgroup tester extra >/dev/null 2>&1
plugin copy "$SCRATCH/up-$TAG-alpine.txt" "root@$TAG-alpine:as-root.txt" \
    && [ "$(container exec "$TAG-alpine" stat -c %u:%g /root/as-root.txt)" = "0:0" ] \
    && pass "root@ owns as 0:0 in /root" || flunk "root@ ownership"
plugin copy "$SCRATCH/up-$TAG-alpine.txt" "tester@$TAG-alpine:as-tester.txt" \
    && [ "$(container exec "$TAG-alpine" stat -c %u:%g /home/tester/as-tester.txt)" = "1031:1031" ] \
    && pass "USER@ owns as the user's primary group" || flunk "USER@ ownership"
# Two users in one guest: staged through the host, owned by the second.
plugin copy "tester@$TAG-alpine:mine.txt" "$TAG-alpine:/root/from-tester.txt" \
    && [ "$(container exec "$TAG-alpine" stat -c %u /root/from-tester.txt)" = 0 ] \
    && pass "user-to-user copy" || flunk "user-to-user copy"
plugin ls -u nosuchuser "$TAG-alpine:" >/dev/null 2>&1 \
    && flunk "unknown --user accepted" || pass "unknown --user refused"

# Guest to guest, streamed through the host.
plugin copy "$TAG-alpine:/up.txt" "$TAG-amd64:/from-alpine.txt" \
    && plugin cat "$TAG-amd64:/from-alpine.txt" | grep -q "host-payload-$TAG-alpine" \
    && pass "guest-to-guest copy" || flunk "guest-to-guest copy"

# Directory copies, every direction.
A="$TAG-alpine"
T="$SCRATCH/tree"
mkdir -p "$T/sub/deep"
echo top > "$T/top.txt"
echo deep > "$T/sub/deep/d.txt"
chmod 750 "$T/sub"
ln -s ../top.txt "$T/sub/up-link"
mkfifo "$T/fifo"
# An existing directory receives the tree under its own name (cp -r).
container exec "$A" mkdir -p /in
plugin copy "$T" "$A:/in" 2>/dev/null \
    && [ "$(container exec "$A" cat /in/tree/sub/deep/d.txt)" = deep ] \
    && [ "$(container exec "$A" readlink /in/tree/sub/up-link)" = ../top.txt ] \
    && [ "$(container exec "$A" stat -c %a /in/tree/sub)" = 750 ] \
    && ! container exec "$A" test -e /in/tree/fifo \
    && pass "dir copy in (files, links, modes; FIFO skipped)" || flunk "dir copy in"

# Out: links that stay inside survive; escaping and absolute ones don't.
container exec "$A" sh -c 'mkdir -p /out/a && echo hi > /out/a/f && chmod 4755 /out/a/f &&
    ln -s ../a/f /out/a/inside && ln -s ../../etc/passwd /out/a/escape &&
    ln -s /etc/hosts /out/a/absolute && ln -s . /out/y && ln -s y/../etc /out/z'
mkdir -p "$SCRATCH/got"
plugin copy "$A:/out" "$SCRATCH/got" 2>"$SCRATCH/out.err"
G="$SCRATCH/got/out"
[ "$(cat "$G/a/f")" = hi ] && [ -L "$G/a/inside" ] && [ "$(cat "$G/a/inside")" = hi ] \
    && [ ! -e "$G/a/escape" ] && [ ! -L "$G/a/escape" ] && [ ! -L "$G/a/absolute" ] \
    && [ -L "$G/y" ] && [ ! -L "$G/z" ] \
    && grep -q "outside the copied tree" "$SCRATCH/out.err" \
    && pass "dir copy out confines links" || flunk "dir copy out ($(cat "$SCRATCH/out.err"))"
[ "$(stat -f %Lp "$G/a/f")" = 755 ] && pass "setuid dropped on the host" \
    || flunk "setuid kept ($(stat -f %Lp "$G/a/f"))"

# Never write through a host symlink at the destination (decoy target).
mkdir -p "$SCRATCH/trap/out/a"
echo decoy > "$SCRATCH/decoy"
ln -s "$SCRATCH/decoy" "$SCRATCH/trap/out/a/f"
plugin copy "$A:/out" "$SCRATCH/trap" >/dev/null 2>&1 \
    && flunk "wrote through a host symlink" \
    || { [ "$(cat "$SCRATCH/decoy")" = decoy ] && pass "host symlink not followed" \
        || flunk "decoy changed"; }

plugin copy "$A:/in/tree" "$A:/in/tree2" \
    && [ "$(container exec "$A" cat /in/tree2/sub/deep/d.txt)" = deep ] \
    && [ "$(container exec "$A" readlink /in/tree2/sub/up-link)" = ../top.txt ] \
    && pass "dir copy within a guest" || flunk "dir copy within"
plugin copy "$A:/in/tree" "$A:/in/tree/sub" >/dev/null 2>&1 \
    && flunk "copied a directory into itself" || pass "self-nesting refused"
plugin copy "$A:/in/tree" "$TAG-amd64:/from-alpine-tree" \
    && [ "$(container exec "$TAG-amd64" cat /from-alpine-tree/sub/deep/d.txt)" = deep ] \
    && pass "dir copy guest to guest" || flunk "dir copy guest to guest"

# Dispatch rules.
err="$(plugin lst "$TAG-alpine:" 2>&1 || true)"
echo "$err" | grep -q 'did you mean `list` or `ls`' && pass "typo hint" || flunk "typo hint ($err)"
err="$(plugin rm "$TAG-alpine:/up.txt" 2>&1 || true)"
echo "$err" | grep -q "not implemented yet" && pass "reserved verb" || flunk "reserved verb ($err)"
err="$(plugin -- ls 2>&1 || true)"
echo "$err" | grep -q "FSKit" && pass "-- makes a verb a TARGET" || flunk "-- ($err)"
err="$(plugin "$TAG-alpine" /tmp/mnt 2>&1 || true)"
echo "$err" | grep -q "FSKit" && pass "implied attach" || flunk "implied attach ($err)"
plugin copy a b >/dev/null 2>&1 && flunk "local-to-local accepted" || pass "local-to-local refused"
[ "$(plugin list --format json)" = "[]" ] && pass "list json" || flunk "list json"

# Stopped targets are refused, not booted.
container stop "$TAG-alpine" >/dev/null
if plugin ls "$TAG-alpine:/" >/dev/null 2>&1; then
    flunk "stopped container accepted"
else
    pass "stopped container refused"
fi

out="$(plugin --version --format json)"
echo "$out" | grep -q '"interface":0' && pass "version json" || flunk "version json ($out)"

[ "$fail" = 0 ] && log "all checks passed" || die "failures above"

