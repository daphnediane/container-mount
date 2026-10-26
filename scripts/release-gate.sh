#!/bin/sh
# Pre-release gate: everything to check before tagging, in one command.
# Exits nonzero on the first failure.
set -eu
cd "$(dirname "$0")/.."

step() { printf '\n==> %s\n' "$*"; }

step "cargo fmt --check"
cargo fmt --all -- --check

step "cargo clippy (host)"
cargo clippy --workspace --all-targets -- -D warnings

# The Linux-only server code is cfg'd out of the host build.
for TARGET in aarch64-unknown-linux-musl x86_64-unknown-linux-musl; do
    step "cargo clippy ($TARGET)"
    cargo clippy --target "$TARGET" --all-targets -- -D warnings
    step "cargo build --release ($TARGET)"
    cargo build --release --target "$TARGET"
done

step "cargo test --workspace"
cargo test --workspace

step "shell syntax"
sh -n tests/*.sh scripts/*.sh scripts/pkg/*.sh packaging/*.sh

step "cargo audit"
cargo audit

step "cargo deny check"
cargo deny check

step "THIRD-PARTY-NOTICES.yaml freshness"
# The pkg payload and release assets ship this file; it must reflect
# the current Cargo.lock. Regenerate with
# `cargo bundle-licenses -f yaml -o THIRD-PARTY-NOTICES.yaml`.
if command -v cargo-bundle-licenses >/dev/null; then
    cargo bundle-licenses -f yaml -o scratch/notices-check.yaml
    if ! cmp -s scratch/notices-check.yaml THIRD-PARTY-NOTICES.yaml; then
        echo "THIRD-PARTY-NOTICES.yaml is stale — regenerate with" >&2
        echo "  cargo bundle-licenses -f yaml -o THIRD-PARTY-NOTICES.yaml" >&2
        exit 1
    fi
    rm -f scratch/notices-check.yaml
else
    echo "  (no cargo bundle-licenses — skipping freshness check)"
fi

if command -v container >/dev/null 2>&1 || command -v docker >/dev/null 2>&1 \
    || command -v podman >/dev/null 2>&1; then
    step "tests/e2e.sh"
    tests/e2e.sh
else
    echo "==> e2e skipped (no OCI engine)"
fi

printf '\nrelease gate: all checks passed\n'
