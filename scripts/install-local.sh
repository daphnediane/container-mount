#!/bin/sh
# Maintainer reinstall from this checkout: the host binary plus both
# musl guest servers, then the `container mount` plugin. Equivalent to:
#
#   cargo install --path . && scripts/build-servers.sh --install ~/.cargo \
#       && ~/.cargo/bin/container-mount install-all --no-fetch
#
# The servers land in <cargo-root>/libexec/container-mount/, which a binary
# under <root>/bin discovers automatically (src/servers.rs).
set -eu
cd "$(dirname "$0")/.."

step() { printf '\n==> %s\n' "$*"; }

step "cargo install --path ."
cargo install --path .

ROOT="${CARGO_INSTALL_ROOT:-${CARGO_HOME:-$HOME/.cargo}}"

step "scripts/build-servers.sh --install"
scripts/build-servers.sh --install "$ROOT"

# Copies the binary and the servers just installed into container's
# plugin dir — sudo only when that dir isn't user-writable.
step "container-mount install-all"
"$ROOT/bin/container-mount" install-all --no-fetch
