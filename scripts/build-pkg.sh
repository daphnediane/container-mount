#!/bin/sh
# Build the macOS installer .pkg, the cargo-binstall client tarball,
# the guest-server bundle, and — when cargo-cyclonedx is installed —
# the CycloneDX SBOM that the release workflow attaches to GitHub
# Releases. When cargo-auditable is installed the host binary also
# embeds its dep tree for `cargo audit bin`. Both plugins are
# recommended but optional here — build-pkg-signed.sh (the official
# release path) requires them.
# macOS only — needs cargo, jq, pkgbuild, and productbuild. Output
# lands in scratch/pkg/ (gitignored). `--skip-build` reuses the
# existing target/release builds.
#
# Assets produced (the names fetch-servers/update look for):
#   container-mount-<ver>.pkg / container-mount-<ver>-unsigned.pkg
#   container-mount-servers-<ver>.tar.gz          — both musl servers
#   container-mount-<triple>-v<ver>.tar.gz        — signed client + servers
#   container-mount-<triple>-v<ver>-unsigned.tar.gz  — unsigned variant
#
# Signing is auto-detected (docs/design/packaging.md):
#   --sign           require signing — fail when the identities are absent
#   --no-sign        force the -unsigned artifacts even with certs present
#   CODESIGN_IDENTITY  signs the container-mount binary (default: a "Developer ID
#                      Application" keychain identity, if present)
#   PKG_SIGN_IDENTITY  signs the .pkg (default: "Developer ID Installer")
#   NOTARY_PROFILE     notarytool keychain profile — when set, the signed
#                      pkg is notarized and stapled
# The musl servers are ELF — they can't carry a Mac signature; their
# integrity is the version stamp plus the release digest check.
set -eu
cd "$(dirname "$0")/.."

step() { printf '\n==> %s\n' "$*"; }

SKIP_BUILD=
SIGN=auto
for arg in "$@"; do
    case "$arg" in
        --skip-build) SKIP_BUILD=1 ;;
        --sign) SIGN=1 ;;
        --no-sign) SIGN=0 ;;
        *) echo "usage: $0 [--skip-build] [--sign|--no-sign]" >&2; exit 2 ;;
    esac
done

OUT=scratch/pkg
# Stale artifacts from a prior run must not leak into a release glob.
rm -rf "$OUT"
COMPONENT=container-mount-core.pkg
IDENTIFIER=io.github.daphnediane.container-mount
TARGET=aarch64-apple-darwin
MUSL_A=aarch64-unknown-linux-musl
MUSL_X=x86_64-unknown-linux-musl

META=$(cargo metadata --no-deps --format-version 1)
version=$(echo "$META" | jq -r '.packages[0].version')
R="$(echo "$META" | jq -r '.target_directory')/release"
TA="$(echo "$META" | jq -r '.target_directory')/$MUSL_A/release"
TX="$(echo "$META" | jq -r '.target_directory')/$MUSL_X/release"

if [ -z "$SKIP_BUILD" ]; then
    for TARGET_TRIPLE in "$MUSL_A" "$MUSL_X"; do
        step "cargo build --release --target $TARGET_TRIPLE"
        cargo build --release --target "$TARGET_TRIPLE"
    done
    # `cargo auditable` is a drop-in cargo wrapper that embeds the
    # resolved dep tree (zlib JSON, `.dep-v0` section) into the
    # binary — `cargo audit bin` can then scan an installed copy with
    # no bookkeeping. Interim until native cargo SBOM embedding lands.
    if command -v cargo-auditable >/dev/null; then
        step "cargo auditable build --release"
        cargo auditable build --release
    else
        step "cargo build --release"
        cargo build --release
        echo "  (no cargo auditable — the binary won't embed its dep tree; recommended for releases)"
    fi
fi

[ -x "$R/container-mount" ] && [ -x "$TA/container-mount" ] && [ -x "$TX/container-mount" ] || {
    echo "release binaries missing — run without --skip-build" >&2
    exit 1
}
otool -l "$R/container-mount" | grep -q dep-v0 || \
    echo "  note: container-mount has no .dep-v0 dep tree (not built via cargo auditable)"

step "CycloneDX SBOM"
SBOM="container-mount-$version-sbom.cdx"
mkdir -p "$OUT"
rm -f "$OUT/$SBOM.json"
if command -v cargo-cyclonedx >/dev/null; then
    cargo cyclonedx --format json --spec-version 1.5 \
        --target "$TARGET" --override-filename "$SBOM"
    mv "$SBOM.json" "$OUT/$SBOM.json"
else
    echo "  (no cargo cyclonedx — skipping the SBOM; recommended for releases)"
fi

# Find the first keychain identity with the given cert name under the
# given search policy. Empty when absent (the unsigned path).
find_identity() {
    security find-identity -v -p "$1" 2>/dev/null |
        sed -n "s/.*\"\($2[^\"]*\)\".*/\1/p" | head -n1
}
if [ "$SIGN" = 0 ]; then
    APP_IDENTITY=
    PKG_IDENTITY=
else
    APP_IDENTITY=${CODESIGN_IDENTITY:-$(find_identity codesigning "Developer ID Application")}
    PKG_IDENTITY=${PKG_SIGN_IDENTITY:-$(find_identity basic "Developer ID Installer")}
fi
if [ "$SIGN" = 1 ] && { [ -z "$APP_IDENTITY" ] || [ -z "$PKG_IDENTITY" ]; }; then
    echo "--sign was given but Developer ID identities are missing" >&2
    echo "  binary: ${APP_IDENTITY:-not found}" >&2
    echo "  pkg:    ${PKG_IDENTITY:-not found}" >&2
    exit 1
fi

if [ -n "$APP_IDENTITY" ]; then
    step "codesign container-mount ($APP_IDENTITY)"
    codesign --force --sign "$APP_IDENTITY" \
        --options runtime --timestamp "$R/container-mount"
fi

step "stage payload"
# Payload is rooted at the install-location — /usr/local — so the
# receipt's file/dir paths and its `location:` field both describe
# what we actually own.
PAYLOAD="$OUT/payload"
rm -rf "$PAYLOAD"
mkdir -p "$PAYLOAD/bin" "$PAYLOAD/libexec/container-mount" \
    "$PAYLOAD/share/doc/container-mount"
cp "$R/container-mount" packaging/uninstall-container-mount.sh \
    packaging/update-container-mount.sh "$PAYLOAD/bin/"
cp "$TA/container-mount" "$PAYLOAD/libexec/container-mount/container-mount-server-aarch64"
cp "$TX/container-mount" "$PAYLOAD/libexec/container-mount/container-mount-server-x86_64"
cp LICENSE THIRD-PARTY-NOTICES.yaml "$PAYLOAD/share/doc/container-mount/"

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
    packaging/Distribution.xml > "$OUT/Distribution.xml"
RESOURCES=$OUT/resources
rm -rf "$RESOURCES"
mkdir -p "$RESOURCES"
cp packaging/welcome.html packaging/conclusion.html "$RESOURCES/"
# The installer's license step names every vendored crate's license
# expression; the full copyright + license texts stay in
# THIRD-PARTY-NOTICES.yaml (payload + release asset).
{
    cat LICENSE
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
    ' THIRD-PARTY-NOTICES.yaml
} > "$RESOURCES/licenses.txt"
# A signed pkg keeps the clean name; unsigned builds (CI without a
# keychain) get the -unsigned convention — the maintainer adds the
# signed build at draft-review time (docs/design/packaging.md).
PKG_NAME="container-mount-$version"
[ -n "$PKG_IDENTITY" ] || PKG_NAME="$PKG_NAME-unsigned"
if [ -n "$PKG_IDENTITY" ]; then
    productbuild --distribution "$OUT/Distribution.xml" \
        --resources "$RESOURCES" --package-path "$COMPONENT_DIR" \
        --sign "$PKG_IDENTITY" \
        "$OUT/$PKG_NAME.pkg"
else
    productbuild --distribution "$OUT/Distribution.xml" \
        --resources "$RESOURCES" --package-path "$COMPONENT_DIR" \
        "$OUT/$PKG_NAME.pkg"
    echo "  (unsigned — see docs/design/packaging.md for the signing setup)"
fi

if [ -n "${NOTARY_PROFILE:-}" ] && [ -n "$PKG_IDENTITY" ]; then
    step "notarize + staple ($NOTARY_PROFILE)"
    xcrun notarytool submit "$OUT/$PKG_NAME.pkg" \
        --keychain-profile "$NOTARY_PROFILE" --wait
    xcrun stapler staple "$OUT/$PKG_NAME.pkg"
fi

step "server bundle tarball"
# container-mount-servers-<ver>.tar.gz — what `container-mount fetch-servers`
# downloads.
SERVERS_DIR="container-mount-servers-$version"
rm -rf "$OUT/$SERVERS_DIR" "$OUT/$SERVERS_DIR.tar.gz"
mkdir -p "$OUT/$SERVERS_DIR"
cp "$PAYLOAD/libexec/container-mount"/container-mount-server-* "$OUT/$SERVERS_DIR/"
tar -czf "$OUT/$SERVERS_DIR.tar.gz" -C "$OUT" "$SERVERS_DIR"
rm -rf "$OUT/$SERVERS_DIR"

step "cargo-binstall tarball"
# { name }-{ target }-v{ version }.tar.gz with a matching inner dir —
# the layout [package.metadata.binstall] in Cargo.toml advertises —
# plus the server binaries, which `container-mount update` installs beside
# them. Signed binary ⇒ clean name; unsigned ⇒ -unsigned (what
# `update` picks only with consent).
TARBALL_DIR="container-mount-$TARGET-v$version"
rm -rf "$OUT/$TARBALL_DIR" "$OUT/$TARBALL_DIR.tar.gz" "$OUT/$TARBALL_DIR-unsigned.tar.gz"
mkdir -p "$OUT/$TARBALL_DIR"
cp "$R/container-mount" "$PAYLOAD/libexec/container-mount"/container-mount-server-* \
    LICENSE THIRD-PARTY-NOTICES.yaml "$OUT/$TARBALL_DIR/"
TAR_NAME="$TARBALL_DIR"
[ -n "$APP_IDENTITY" ] || TAR_NAME="$TARBALL_DIR-unsigned"
tar -czf "$OUT/$TAR_NAME.tar.gz" -C "$OUT" "$TARBALL_DIR"
rm -rf "$OUT/$TARBALL_DIR"

step "summary"
pkgutil --payload-files "$COMPONENT_DIR/$COMPONENT" | sed 's/^/  /'
SUMMARY_FILES="$OUT/$PKG_NAME.pkg $OUT/$TAR_NAME.tar.gz $OUT/$SERVERS_DIR.tar.gz"
[ -f "$OUT/$SBOM.json" ] && SUMMARY_FILES="$SUMMARY_FILES $OUT/$SBOM.json"
shasum -a 256 $SUMMARY_FILES | sed 's/^/  /'
