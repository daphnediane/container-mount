#!/usr/bin/env bash
# End-to-end test: run the musl guest server in a container and drive
# the host client against it — no container-distro involvement.
#
# Picks the first available engine: Apple `container` (Unix socket
# relay + code-signature peer check, the production shape), then
# `podman`, then `docker` (published TCP port). Skips cleanly when
# none is installed or usable.
#
#   tests/e2e.sh [--engine ENGINE] [--arch arm64|amd64] [--keep]
#
# Builds `target/<arch>-unknown-linux-musl/release/distrofs` and the
# debug host client as needed.

set -euo pipefail
cd "$(dirname "$0")/.."

ENGINE=""
ARCH=""
KEEP=0
while [ $# -gt 0 ]; do
    case "$1" in
        --engine) ENGINE="$2"; shift 2 ;;
        --arch) ARCH="$2"; shift 2 ;;
        --keep) KEEP=1; shift ;;
        -h|--help) sed -n '2,12p' "$0"; exit 0 ;;
        *) echo "e2e: unknown argument $1" >&2; exit 2 ;;
    esac
done

log() { printf 'e2e: %s\n' "$*" >&2; }
die() { log "$*"; exit 1; }

# --- engine detection -------------------------------------------------

if [ -z "$ENGINE" ]; then
    for e in container podman docker; do
        if command -v "$e" >/dev/null 2>&1; then ENGINE="$e"; break; fi
    done
fi
[ -n "$ENGINE" ] || { log "no container/podman/docker found — skipping"; exit 0; }
command -v "$ENGINE" >/dev/null 2>&1 || die "engine $ENGINE not found"

# arch: host arm64 → aarch64 guest for local engines.
if [ -z "$ARCH" ]; then
    case "$(uname -m)" in
        arm64|aarch64) ARCH=aarch64 ;;
        *) ARCH=x86_64 ;;
    esac
fi
TARGET="$ARCH-unknown-linux-musl"

# --- build ------------------------------------------------------------

SERVER="target/$TARGET/release/distrofs"
CLIENT="target/debug/distrofs"
# Always hand the builds to cargo — it no-ops on fresh sources and a
# stale binary here costs a debugging session.
log "building $TARGET server and host client"
cargo build --release --target "$TARGET" >/dev/null
cargo build >/dev/null
[ -x "$SERVER" ] && [ -x "$CLIENT" ] || die "builds missing"

NAME="distrofs-e2e-$$"
TOKEN="$(od -An -N32 -tx1 /dev/urandom | tr -d ' \n')"
export DISTROFS_TOKEN="$TOKEN"

SCRATCH="scratch/e2e-$NAME"
mkdir -p "$SCRATCH"
cleanup() {
    [ "$KEEP" = 1 ] || rm -rf "$SCRATCH"
    case "$ENGINE" in
        container) "$ENGINE" delete "$NAME" >/dev/null 2>&1 || true ;;
        *) "$ENGINE" rm -f "$NAME" >/dev/null 2>&1 || true ;;
    esac
}
trap cleanup EXIT

# Image: alpine rootfs + the static server as /distrofs. Alpine gives
# us /bin/sh in the guest to stage links/files for confinement tests.
cp "$SERVER" "$SCRATCH/distrofs"
printf 'FROM alpine\nCOPY distrofs /distrofs\n' > "$SCRATCH/Dockerfile"

case "$ENGINE" in
    container)
        "$ENGINE" build -t distrofs-e2e -f "$SCRATCH/Dockerfile" "$SCRATCH" >/dev/null
        HOST_SOCK="$("$CLIENT" socket-path "$NAME")"
        "$ENGINE" run -d --name "$NAME" \
            -e DISTROFS_TOKEN \
            --publish-socket "$HOST_SOCK":/var/lib/distrofs/distrofs.sock \
            distrofs-e2e /distrofs serve \
            --socket /var/lib/distrofs/distrofs.sock --idle-secs 300 \
            >/dev/null
        PEER_REQ='identifier "com.apple.container.container-runtime-linux" and anchor apple generic and certificate leaf[subject.OU] = "UPBK2H6LZM"'
        CARGS=(--socket "$HOST_SOCK" --peer-requirement "$PEER_REQ")
        ;;
    podman|docker)
        "$ENGINE" build -t distrofs-e2e -f "$SCRATCH/Dockerfile" "$SCRATCH" >/dev/null
        "$ENGINE" run -d --name "$NAME" \
            -e DISTROFS_TOKEN \
            -p 127.0.0.1::8765 \
            distrofs-e2e /distrofs serve --listen tcp:0.0.0.0:8765 \
            --idle-secs 300 >/dev/null
        PORT="$("$ENGINE" port "$NAME" 8765/tcp | head -1 | sed 's/.*://')"
        [ -n "$PORT" ] || die "couldn't learn the published port"
        CARGS=(--tcp "127.0.0.1:$PORT")
        ;;
esac

cli() { DISTROFS_TOKEN="$TOKEN" "$CLIENT" --timeout 15 "${CARGS[@]}" "$@"; }

log "engine=$ENGINE arch=$ARCH — waiting for the server"
ok=0
for _ in $(seq 1 50); do
    if cli check >/dev/null 2>&1; then ok=1; break; fi
    sleep 0.2
done
[ "$ok" = 1 ] || die "server never answered"

fail=0
t() { if "$@"; then log "PASS $*"; else log "FAIL $*"; fail=1; fi; }

# Stage guest content through the engine.
guest() {
    case "$ENGINE" in
        container) "$ENGINE" exec "$NAME" /bin/sh -c "$1" ;;
        *) "$ENGINE" exec "$NAME" /bin/sh -c "$1" ;;
    esac
}
guest 'echo e2e-payload > /marker.txt'
guest 'mkdir -p /sub && ln -s /etc/passwd /sub/inside-link'
guest 'ln -s ../../../../../../etc/hostname /sub/escape-link'

t cli check
t cli ls /
t cli stat /marker.txt
cli get /marker.txt - | grep -q e2e-payload && log "PASS get" || { log "FAIL get"; fail=1; }

echo "host-payload" > "$SCRATCH/up.txt"
t cli put "$SCRATCH/up.txt" /up.txt
guest 'grep -q host-payload /up.txt' && log "PASS put-visible-in-guest" \
    || { log "FAIL put-visible-in-guest"; fail=1; }

# Guest-side copy lands in the guest without a wire round trip.
t cli cp /marker.txt /marker-copy.txt
guest 'grep -q e2e-payload /marker-copy.txt' \
    && log "PASS cp-visible-in-guest" || { log "FAIL cp-visible-in-guest"; fail=1; }

# Sparse files stay sparse in both directions: an 8 MiB file with a
# few bytes of data should land with ~tens of 512-blocks, not ~16k.
guest 'echo sparse-mark > /sparse.bin && truncate -s 8M /sparse.bin'
t cli get /sparse.bin "$SCRATCH/sparse.bin"
sz=$(stat -f%z "$SCRATCH/sparse.bin"); bl=$(stat -f%b "$SCRATCH/sparse.bin")
if [ "$sz" = 8388608 ] && [ "$bl" -lt 8192 ]; then
    log "PASS sparse get ($bl blocks)"
else
    log "FAIL sparse get (size=$sz blocks=$bl)"; fail=1
fi
t cli put "$SCRATCH/sparse.bin" /sparse2.bin
guest 's=$(stat -c%s /sparse2.bin); b=$(stat -c%b /sparse2.bin);
   [ "$s" = 8388608 ] && [ "$b" -lt 8192 ] && grep -q sparse-mark /sparse2.bin' \
    && log "PASS sparse put" || { log "FAIL sparse put"; fail=1; }

# Symlink confinement: targets must stay relative and inside the root.
out="$(cli stat /sub/inside-link)"
echo "$out" | grep -q 'target: ../etc/passwd' \
    && log "PASS inside-link confined" || { log "FAIL inside-link ($out)"; fail=1; }
out="$(cli stat /sub/escape-link)"
echo "$out" | grep -q 'target: ../etc/hostname' \
    && log "PASS escape-link clamped at root" || { log "FAIL escape-link ($out)"; fail=1; }

# A wrong token must never handshake.
if DISTROFS_TOKEN="$(od -An -N32 -tx1 /dev/urandom | tr -d ' \n')" \
    "$CLIENT" --timeout 3 "${CARGS[@]}" check >/dev/null 2>&1; then
    log "FAIL bad token authenticated"
    fail=1
else
    log "PASS bad token rejected"
fi

if [ "$fail" = 0 ]; then
    log "all checks passed"
else
    die "failures above"
fi
