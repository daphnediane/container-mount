#!/bin/sh
# Compile the release binaries in DIR — pure build step, nothing
# copied out (pkg/collect.sh gathers outputs afterward): the two static
# musl guest servers, then the host client. Prefers `cargo auditable`
# for the host binary so it embeds its dep tree (`cargo audit bin` can
# scan an installed copy — interim until native cargo SBOM embedding
# lands). To reuse an existing build, don't invoke this at all —
# that's the orchestrator's --skip-build.
set -eu

dir=${1:?"usage: $0 DIR"}
cd "$dir"

step() { printf '\n==> %s\n' "$*"; }

for TARGET_TRIPLE in aarch64-unknown-linux-musl x86_64-unknown-linux-musl; do
    step "cargo build --release --target $TARGET_TRIPLE"
    cargo build --release --target "$TARGET_TRIPLE"
done

if command -v cargo-auditable >/dev/null; then
    step "cargo auditable build --release"
    cargo auditable build --release
else
    step "cargo build --release"
    cargo build --release
    echo "  (no cargo auditable — the binary won't embed its dep tree; recommended for releases)"
fi
