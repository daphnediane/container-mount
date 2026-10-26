#!/bin/sh
# Update container-mount to the latest release (or VERSION) — a thin wrapper:
# the installed binary does the release lookup, verification
# (API-reported SHA-256, codesign + same-team check on the new binary),
# and the swap itself. Shipped in the pkg's bin/ so the package layout
# carries an updater.
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
