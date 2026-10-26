#!/bin/sh
# Remove every container-mount install from this machine — for dev machines
# cycling between pkg / cargo / manual installs, when a new build must
# be the only copy. Removes:
#   - the .pkg payload (delegated to packaging/uninstall-container-mount.sh)
#   - $CARGO_HOME/bin/container-mount via `cargo uninstall`, and
#     */libexec/container-mount/ server dirs under the cargo root and
#     /usr/local (fetch-servers / build-servers.sh --install)
#
# Never touches guests or checkouts. `-n` previews instead of doing.
set -u
cd "$(dirname "$0")/.."

step() { printf '\n==> %s\n' "$*"; }

DRY=
for arg in "$@"; do
    case "$arg" in
        -n) DRY=1 ;;
        *) echo "usage: $0 [-n]" >&2; exit 2 ;;
    esac
done

x() {
    if [ -n "$DRY" ]; then
        printf '  %s\n' "$(printf '%q ' "$@")"
    else
        "$@"
    fi
}

# sudo only when needed: already root, or the path's parent dir is
# user-writable, means the command runs directly.
sudo_if() {
    # sudo_if <path affected> <command...>
    t=$1; shift
    if [ "$(id -u)" -eq 0 ] || [ -w "${t%/*}" ]; then
        x "$@"
    else
        x sudo "$@"
    fi
}

PKG=io.github.daphnediane.container-mount
CH=${CARGO_HOME:-$HOME/.cargo}

step "pkg payload"
if pkgutil --pkg-info "$PKG" >/dev/null 2>&1; then
    if [ -n "$DRY" ]; then
        sh packaging/uninstall-container-mount.sh -n
    else
        sh packaging/uninstall-container-mount.sh
    fi
else
    echo "  (no receipt for $PKG)"
fi

step "cargo uninstall"
if command -v cargo >/dev/null 2>&1 && [ -e "$CH/bin/container-mount" ]; then
    x cargo uninstall container-mount ||
        echo "  (cargo uninstall failed — remove $CH/bin/container-mount by hand)"
else
    echo "  (no container-mount under $CH/bin)"
fi

step "fetched/built servers"
# <prefix>/libexec/container-mount — install-managed, so not covered by the
# pkg receipt when fetch-servers added files to it, and not by cargo.
for prefix in "$CH" /usr/local "$HOME/.local"; do
    d="$prefix/libexec/container-mount"
    [ -d "$d" ] || continue
    for f in "$d"/container-mount-server-*; do
        [ -e "$f" ] && sudo_if "$d" rm -f "$f"
    done
    sudo_if "$prefix/libexec" rmdir "$d" 2>/dev/null
    sudo_if "$prefix" rmdir "$prefix/libexec" 2>/dev/null
done

if [ -z "$DRY" ]; then
    p=$(command -v container-mount 2>/dev/null || true)
    [ -n "$p" ] && echo "  still on PATH: $p"
fi

if [ -n "$DRY" ]; then
    echo "Dry run — nothing was removed."
else
    echo "Done."
fi
