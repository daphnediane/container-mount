#!/bin/sh
# Build the static musl guest servers for both architectures — rust-lld
# links them against Rust's self-contained musl (.cargo/config.toml),
# so no cross C toolchain or container is needed.
#
#   scripts/build-servers.sh                  # build only
#   scripts/build-servers.sh --install PREFIX # also copy to
#                                             # PREFIX/libexec/container-mount/
#
# `--install ~/.cargo` pairs with `cargo install --path .`: a container-mount
# binary under <prefix>/bin discovers servers in
# <prefix>/libexec/container-mount/<triple>/container-mount (src/servers.rs).
set -eu
cd "$(dirname "$0")/.."

for TARGET in aarch64-unknown-linux-musl x86_64-unknown-linux-musl; do
    rustup target list --installed | grep -qx "$TARGET" \
        || rustup target add "$TARGET"
    cargo build --release --target "$TARGET"
    echo "built target/$TARGET/release/container-mount"
done

if [ "${1:-}" = "--install" ]; then
    PREFIX="${2:?usage: $0 --install PREFIX}"
    for TARGET in aarch64-unknown-linux-musl x86_64-unknown-linux-musl; do
        DEST="$PREFIX/libexec/container-mount/$TARGET"
        mkdir -p "$DEST"
        install -m 0755 "target/$TARGET/release/container-mount" "$DEST/container-mount"
        echo "installed $DEST/container-mount"
    done
fi
