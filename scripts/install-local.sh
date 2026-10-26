#!/bin/sh
# Maintainer reinstall from this checkout: the host binary plus both
# musl guest servers. Equivalent to:
#
#   cargo install --path . && scripts/build-servers.sh --install ~/.cargo
#
# The servers land in <cargo-root>/libexec/container-mount/, which a binary
# under <root>/bin discovers automatically (src/servers.rs).
set -eu
cd "$(dirname "$0")/.."

step() { printf '\n==> %s\n' "$*"; }

step "cargo install --path ."
cargo install --path .

step "scripts/build-servers.sh --install"
scripts/build-servers.sh --install \
    "${CARGO_INSTALL_ROOT:-${CARGO_HOME:-$HOME/.cargo}}"
