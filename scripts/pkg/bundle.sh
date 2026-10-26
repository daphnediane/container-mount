#!/bin/sh
# Assemble the release artifacts entirely from --out DIR: signs the
# compiled host binary in bin/, stages the payload from collect/,
# builds the installer .pkg (productbuild --sign when
# $PKG_SIGN_IDENTITY), notarizes + staples when $NOTARY_PROFILE, and
# produces the guest-server bundle plus the cargo-binstall tarball
# with its detached signatures. No source checkout needed — a build
# worktree can already be gone.
#
#   bundle.sh --out DIR
#
# Requires $PKG_VERSION (and reads $PROV_SUFFIX) for artifact naming —
# export the pkg/name.sh values. Signing is env-driven like the rest
# of the pipeline; unsigned outputs are name-mangled `-unsigned`.
set -eu

step() { printf '\n==> %s\n' "$*"; }
HERE=$(cd "$(dirname "$0")" && pwd)

out=
while [ $# -gt 0 ]; do
    case "$1" in
        --out) out=$2; shift 2 ;;
        *) echo "usage: $0 --out DIR" >&2; exit 2 ;;
    esac
done
[ -n "$out" ] || { echo "usage: $0 --out DIR" >&2; exit 2; }
OUT=$(cd "$out" && pwd)

: "${PKG_VERSION:?artifact naming needs PKG_VERSION — eval scripts/pkg/name.sh}"
version=$PKG_VERSION
prov_suffix=${PROV_SUFFIX:-}

COMPONENT=container-mount-core.pkg
IDENTIFIER=io.github.daphnediane.container-mount
TARGET=aarch64-apple-darwin
CATALOG="$OUT/collect"
# collect.sh stages the servers as <triple>/container-mount here.
SERVERS="$OUT/servers"
SBOM="container-mount-$version$prov_suffix-sbom.cdx"

# Only the host binary is signed — the musl servers are ELF.
if [ -n "${CODESIGN_IDENTITY:-}" ]; then
    step "codesign container-mount ($CODESIGN_IDENTITY)"
    "$HERE/sign.sh" "$OUT/bin/container-mount"
fi

step "stage payload"
# Payload is rooted at the install-location — /usr/local — so the
# receipt's file/dir paths and its `location:` field both describe
# what we actually own.
PAYLOAD="$OUT/payload"
rm -rf "$PAYLOAD"
mkdir -p "$PAYLOAD/bin" "$PAYLOAD/libexec/container-mount" \
    "$PAYLOAD/share/doc/container-mount"
cp "$OUT/bin/container-mount" "$CATALOG/uninstall-container-mount.sh" \
    "$CATALOG/update-container-mount.sh" "$PAYLOAD/bin/"
cp -R "$SERVERS/." "$PAYLOAD/libexec/container-mount/"
cp "$CATALOG/LICENSE" "$CATALOG/THIRD-PARTY-NOTICES.yaml" \
    "$PAYLOAD/share/doc/container-mount/"
# The `container mount` plugin copy (binary + its bundled servers) is
# produced by `install-all`, the same output a manual install writes;
# Apple's `container` lives under the same /usr/local root, so the
# payload's container-plugins dir is where it looks. --no-fetch: the
# servers are already staged above.
"$PAYLOAD/bin/container-mount" install-all --no-fetch \
    --plugin-dir "$PAYLOAD/libexec/container-plugins"

step "pkgbuild"
# The component pkg is an intermediate, not a release asset — keep it
# in a subdir so a scratch/pkg/container-mount-*.pkg glob can't pick it up.
COMPONENT_DIR=$OUT/component
mkdir -p "$COMPONENT_DIR"
pkgbuild --root "$PAYLOAD" \
    --identifier "$IDENTIFIER" \
    --version "$version" \
    --install-location /usr/local \
    --ownership recommended \
    "$COMPONENT_DIR/$COMPONENT"

step "productbuild"
sed -e "s/@VERSION@/$version/g" -e "s/@COMPONENT@/$COMPONENT/" \
    "$CATALOG/Distribution.xml" > "$OUT/Distribution.xml"
# productbuild --resources takes a dir — assemble it fresh so the
# generated license screen can live next to the committed pages.
RESOURCES=$OUT/resources
rm -rf "$RESOURCES"
mkdir -p "$RESOURCES"
cp "$CATALOG/welcome.html" "$CATALOG/conclusion.html" "$RESOURCES/"
# The installer's license step names every vendored crate's license
# expression; the full copyright + license texts stay in
# THIRD-PARTY-NOTICES.yaml (payload + release asset).
{
    cat "$CATALOG/LICENSE"
    echo
    echo "======================================================================"
    echo "Third-party components"
    echo
    echo "container-mount is built with these Rust crates. The complete"
    echo "copyright and license texts are reproduced in"
    echo "THIRD-PARTY-NOTICES.yaml, installed alongside the binary and"
    echo "attached to each GitHub Release."
    echo
    awk '
        /^- package_name:/    { name=$NF }
        /^  package_version:/ { ver=$NF }
        /^  license: /        { print "  " name " " ver " — " substr($0, index($0, ":") + 2) }
    ' "$CATALOG/THIRD-PARTY-NOTICES.yaml"
} > "$RESOURCES/licenses.txt"
# A signed pkg keeps the clean name; unsigned builds (CI without a
# keychain) get the -unsigned convention — the maintainer adds the
# signed build at draft-review time (docs/design/packaging.md).
PKG_NAME="container-mount-$version$prov_suffix"
[ -n "${PKG_SIGN_IDENTITY:-}" ] || PKG_NAME="$PKG_NAME-unsigned"
if [ -n "${PKG_SIGN_IDENTITY:-}" ]; then
    productbuild --distribution "$OUT/Distribution.xml" \
        --resources "$RESOURCES" --package-path "$COMPONENT_DIR" \
        --sign "$PKG_SIGN_IDENTITY" \
        "$OUT/$PKG_NAME.pkg"
else
    productbuild --distribution "$OUT/Distribution.xml" \
        --resources "$RESOURCES" --package-path "$COMPONENT_DIR" \
        "$OUT/$PKG_NAME.pkg"
    echo "  (unsigned — see docs/design/packaging.md for the signing setup)"
fi

if [ -n "${NOTARY_PROFILE:-}" ] && [ -n "${PKG_SIGN_IDENTITY:-}" ]; then
    step "notarize + staple ($NOTARY_PROFILE)"
    xcrun notarytool submit "$OUT/$PKG_NAME.pkg" \
        --keychain-profile "$NOTARY_PROFILE" --wait
    xcrun stapler staple "$OUT/$PKG_NAME.pkg"
fi

step "server bundle tarball"
# container-mount-servers-<ver>.tar.gz — what `container-mount
# fetch-servers` downloads. No signed variant: the ELF servers can't
# carry a Mac signature, so only provenance mangles the name.
SERVERS_DIR="container-mount-servers-$version"
SERVERS_NAME="$SERVERS_DIR$prov_suffix.tar.gz"
rm -rf "$OUT/${SERVERS_DIR:?}" "$OUT/$SERVERS_DIR"*.tar.gz
mkdir -p "$OUT/$SERVERS_DIR"
cp -R "$SERVERS/." "$OUT/$SERVERS_DIR/"
tar -czf "$OUT/$SERVERS_NAME" -C "$OUT" "$SERVERS_DIR"
rm -rf "$OUT/${SERVERS_DIR:?}"

step "cargo-binstall tarball"
# { name }-{ target }-v{ version }.tar.gz with a matching inner dir —
# the layout [package.metadata.binstall] in Cargo.toml advertises —
# plus the server binaries, which `container-mount update` installs
# beside them. The inner dir keeps the canonical name; only the
# archive name mangles — unsigned builds get `-unsigned` like the pkg
# (what `update` picks only with consent), so the canonical name only
# ever appears in its final signed form and a signed upload adds
# assets instead of mutating already-attached ones.
TARBALL_DIR="container-mount-$TARGET-v$version"
TAR_NAME="$TARBALL_DIR$prov_suffix.tar.gz"
[ -n "${CODESIGN_IDENTITY:-}" ] || TAR_NAME="$TARBALL_DIR$prov_suffix-unsigned.tar.gz"
rm -rf "$OUT/${TARBALL_DIR:?}" "$OUT/$TARBALL_DIR"*.tar.gz*
mkdir -p "$OUT/$TARBALL_DIR"
cp "$OUT/bin/container-mount" \
    "$CATALOG/LICENSE" "$CATALOG/THIRD-PARTY-NOTICES.yaml" "$OUT/$TARBALL_DIR/"
cp -R "$SERVERS/." "$OUT/$TARBALL_DIR/"
tar -czf "$OUT/$TAR_NAME" -C "$OUT" "$TARBALL_DIR"
rm -rf "$OUT/${TARBALL_DIR:?}"
# Signed builds give the tarball a detached code signature too — same
# Developer ID Application identity as the binary, timestamped like
# it is. GPG_KEY_ID adds an armored `gpg --detach-sign` .asc on top.
if [ -n "${CODESIGN_IDENTITY:-}" ]; then
    "$HERE/sign.sh" --detached "$OUT/$TAR_NAME"
fi

step "summary"
pkgutil --payload-files "$COMPONENT_DIR/$COMPONENT" | sed 's/^/  /'
SUMMARY_FILES="$OUT/$PKG_NAME.pkg $OUT/$TAR_NAME $OUT/$SERVERS_NAME"
[ -f "$OUT/$TAR_NAME.sig" ] && SUMMARY_FILES="$SUMMARY_FILES $OUT/$TAR_NAME.sig"
[ -f "$OUT/$TAR_NAME.asc" ] && SUMMARY_FILES="$SUMMARY_FILES $OUT/$TAR_NAME.asc"
[ -f "$OUT/$SBOM.json" ] && SUMMARY_FILES="$SUMMARY_FILES $OUT/$SBOM.json"
shasum -a 256 $SUMMARY_FILES | sed 's/^/  /'
