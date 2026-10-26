#!/bin/sh
# Sign one file with the Developer ID Application identity
# ($CODESIGN_IDENTITY, required):
#
#   sign.sh [--identifier ID] FILE
#                           in-place codesign for binaries —
#                           hardened runtime, timestamped; ID is the
#                           code-signing identifier (codesign defaults
#                           to the file name, which `update` rejects)
#   sign.sh --detached FILE detached code signature at FILE.sig for
#                           archives, plus an armored GPG
#                           `gpg --detach-sign` FILE.asc when
#                           $GPG_KEY_ID is set (pinentry may prompt on
#                           a cold agent)
set -eu

mode=embedded
identifier=
[ "${1:-}" = "--detached" ] && { mode=detached; shift; }
if [ "${1:-}" = "--identifier" ]; then
    identifier=${2:?"--identifier needs a value"}
    shift 2
fi
file=${1:?"usage: $0 [--detached] FILE"}
: "${CODESIGN_IDENTITY:?signing requires CODESIGN_IDENTITY}"

if [ "$mode" = detached ]; then
    codesign --force --sign "$CODESIGN_IDENTITY" --timestamp \
        --detached "$file.sig" "$file"
    if [ -n "${GPG_KEY_ID:-}" ]; then
        command -v gpg >/dev/null || {
            echo "GPG_KEY_ID is set but gpg isn't installed" >&2
            exit 1
        }
        gpg --yes --armor --local-user "$GPG_KEY_ID" \
            --output "$file.asc" --detach-sign "$file"
    fi
elif [ -n "$identifier" ]; then
    codesign --force --sign "$CODESIGN_IDENTITY" --identifier "$identifier" \
        --options runtime --timestamp "$file"
    # The exact requirement `container-mount update` enforces on a
    # downloaded binary — fail the build, not the user's next update.
    codesign --verify --strict \
        -R="identifier \"$identifier\" and anchor apple generic" "$file"
else
    codesign --force --sign "$CODESIGN_IDENTITY" \
        --options runtime --timestamp "$file"
fi
