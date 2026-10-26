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
    plugin fs "$name" check >/dev/null && pass "$name check" || { flunk "$name check"; return; }
    plugin fs "$name" ls / >/dev/null && pass "$name ls" || flunk "$name ls"
    echo "host-payload-$name" > "$SCRATCH/up-$name.txt"
    plugin fs "$name" put "$SCRATCH/up-$name.txt" /up.txt && pass "$name put" || flunk "$name put"
    plugin fs "$name" get /up.txt - | grep -q "host-payload-$name" \
        && pass "$name get" || flunk "$name get"
    if [ "$shell" = 1 ]; then
        container exec "$name" /bin/sh -c "grep -q host-payload-$name /up.txt" \
            && pass "$name put visible in guest" || flunk "$name put visible in guest"
    fi
    # Each command's server copy deletes itself once running.
    left="$(plugin fs "$name" ls /root/.container-mount)"
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

# Stopped targets are refused, not booted.
container stop "$TAG-alpine" >/dev/null
if plugin fs "$TAG-alpine" check >/dev/null 2>&1; then
    flunk "stopped container accepted"
else
    pass "stopped container refused"
fi

out="$(plugin version --format json)"
echo "$out" | grep -q '"interface":1' && pass "version json" || flunk "version json ($out)"

[ "$fail" = 0 ] && log "all checks passed" || die "failures above"
