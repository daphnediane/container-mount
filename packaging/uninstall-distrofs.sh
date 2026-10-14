#!/bin/sh
# Remove the distrofs .pkg — modeled on Apple's uninstall-container.sh:
# the file/dir list comes from the pkg receipt (never a hardcoded list,
# so an upgraded package's layout can't strand files), and commands get
# sudo only when the path's parent isn't writable by this user.
#
#   uninstall-distrofs.sh         remove the package
#   uninstall-distrofs.sh -n      dry run — show, don't do
#
# distrofs keeps no per-user state (fetched servers live under the
# install prefix the receipt owns), so unlike container-distro's
# uninstaller there is no keep/delete-data choice to make.
set -u

PKG=io.github.daphnediane.distrofs
DRY=

for arg in "$@"; do
    case "$arg" in
        -n) DRY=1 ;;
        *) echo "Usage: $0 [-n]" >&2; exit 2 ;;
    esac
done

if ! pkgutil --pkg-info "$PKG" >/dev/null 2>&1; then
    echo "$PKG is not installed" >&2
    exit 1
fi
if [ "$(id -u)" -ne 0 ] && [ -z "$DRY" ]; then
    echo "Removing root-owned files may require an administrator password."
fi

# x executes or, in a dry run, just shows the command — including the
# sudo prefix sudo_if would have chosen.
x() {
    if [ -n "$DRY" ]; then
        printf '  %s\n' "$(printf '%q ' "$@")"
    elif [ -n "${QUIET:-}" ]; then
        "$@" >/dev/null
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

# The receipt records where the payload root landed ("usr/local").
# Normalize to a canonical absolute path — trailing slash stripped
# first so a bare "/" survives — and receipt paths join under it.
LOC=$(pkgutil --pkg-info "$PKG" | sed -n 's/^location: *//p' | head -n1)
LOC=${LOC%/}
LOC=/${LOC#/}

# Deletion must never escape the install root. TOP is LOC
# canonicalized, with /usr/local as the fallback when the receipt
# claims /. Every removal checks its parent dir resolves inside TOP —
# that blocks `..` traversal and a symlinked parent pointing
# elsewhere. Removing a symlink itself is safe: unlink doesn't
# follow it.
TOP=$LOC
[ "$TOP" = / ] && TOP=/usr/local
[ -d "$TOP" ] && TOP=$(cd -P "$TOP" && pwd -P)

in_top() {
    # in_top <path> — its parent dir must resolve inside TOP
    parent=${1%/*}
    [ -d "$parent" ] || return 1
    real=$(cd -P "$parent" 2>/dev/null && pwd -P) || return 1
    case "$real" in
        "$TOP" | "$TOP"/*) return 0 ;;
    esac
    return 1
}

# Files first (receipt paths are relative to LOC; the ._ AppleDouble
# entries never exist on disk — the -f/-L test skips them).
pkgutil --only-files --files "$PKG" | while IFS= read -r f; do
    p="${LOC%/}/${f#./}"
    if [ -f "$p" ] || [ -L "$p" ]; then
        if in_top "$p"; then
            sudo_if "$p" rm -f "$p"
        else
            echo "skipping $p — parent resolves outside $TOP" >&2
        fi
    fi
done
# Then dirs, deepest first, only when empty — nothing foreign is ever
# deleted. TOP itself never matches in_top (its parent is outside), so
# the install root is never rmdir'd.
pkgutil --only-dirs --files "$PKG" | sort -r | while IFS= read -r d; do
    p="${LOC%/}/${d#./}"
    if in_top "$p"; then
        sudo_if "$p" rmdir "$p" 2>/dev/null
    elif [ -d "$p" ] || [ -L "$p" ]; then
        echo "skipping $p — parent resolves outside $TOP" >&2
    fi
done
QUIET=1 sudo_if /var/db/receipts pkgutil --forget "$PKG"

if [ -n "$DRY" ]; then
    echo "Dry run — nothing was removed. Rerun without -n to remove."
else
    echo "Removed distrofs."
fi
