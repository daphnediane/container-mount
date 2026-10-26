#!/bin/sh
# Update container-mount to the latest release (or VERSION) — a thin wrapper:
# the installed binary does the release lookup and verification. On this
# pkg install that means the signed .pkg (API-reported SHA-256,
# pkgutil signature + same-team check), handed to Installer.app.
# Shipped in the pkg's bin/ so the package layout carries an updater.
#
#   update-container-mount.sh [VERSION] [--force]
#
# VERSION accepts 0.1.0 or v0.1.0; a downgrade asks first.
set -u

D=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)/container-mount
if [ ! -x "$D" ]; then
    echo "container-mount not found next to $0" >&2
    exit 1
fi
exec "$D" update "$@"
