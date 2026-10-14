#!/bin/sh
# Build the static musl guest servers for both architectures — rust-lld
# links them against Rust's self-contained musl (.cargo/config.toml),
# so no cross C toolchain or container is needed.
#
#   scripts/build-servers.sh                  # build only
#   scripts/build-servers.sh --install PREFIX # also copy to
#                                             # PREFIX/libexec/distrofs/
#
# `--install ~/.cargo` pairs with `cargo install --path .`: a distrofs
# binary under <prefix>/bin discovers servers in
# <prefix>/libexec/distrofs/distrofs-server-<arch> (src/servers.rs).
set -eu
cd "$(dirname "$0")/.."

for TARGET in aarch64-unknown-linux-musl x86_64-unknown-linux-musl; do
    rustup target list --installed | grep -qx "$TARGET" \
        || rustup target add "$TARGET"
    cargo build --release --target "$TARGET"
    echo "built target/$TARGET/release/distrofs"
done

if [ "${1:-}" = "--install" ]; then
    PREFIX="${2:?usage: $0 --install PREFIX}"
    DEST="$PREFIX/libexec/distrofs"
    mkdir -p "$DEST"
    install -m 0755 target/aarch64-unknown-linux-musl/release/distrofs \
        "$DEST/distrofs-server-aarch64"
    install -m 0755 target/x86_64-unknown-linux-musl/release/distrofs \
        "$DEST/distrofs-server-x86_64"
    echo "installed $DEST/distrofs-server-{aarch64,x86_64}"
fi
