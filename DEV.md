# Developing container-mount

Building, testing, and releasing the `container-mount` crate and
binary. Contributing conventions live in
[CONTRIBUTING.md](CONTRIBUTING.md).

## Prerequisites

- a current stable Rust toolchain (edition 2024; `rustup` recommended)
- `rustup target add aarch64-unknown-linux-musl x86_64-unknown-linux-musl`
  for the guest-server builds — `rust-lld` links them against Rust's
  self-contained musl (`.cargo/config.toml`), no cross C toolchain
- `cargo deny` and `cargo audit` for the release gate
- an OCI engine (`container`, `podman`, or `docker`) to run
  `tests/e2e.sh` — skipped without one

## Build and test

```bash
cargo build                                              # host binary
cargo build --release --target aarch64-unknown-linux-musl  # guest server (arm64)
cargo build --release --target x86_64-unknown-linux-musl   # guest server (amd64)
```

CI-equivalent before committing:

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo clippy --target aarch64-unknown-linux-musl --all-targets -- -D warnings
cargo clippy --target x86_64-unknown-linux-musl --all-targets -- -D warnings
cargo test --workspace
tests/e2e.sh     # if an OCI engine is available
scripts/release-gate.sh   # all of the above + audit + deny
```

Keep the dependency tree pure Rust — the musl build must link without
a C toolchain (no `ring`, `openssl`, `cc`).

## The e2e test

`tests/e2e.sh` picks the first engine among `container`, `podman`,
`docker` (override: `E2E_ENGINE=…`), builds a `FROM scratch` image
containing the musl server as PID 1, and exercises it through the
host binary:

- Apple `container`: guest `--socket` relayed to a managed host socket
  (`container-mount socket-path`), client connects with `--peer-requirement`
  pinned to the runtime helper's signature — the production-shaped
  path.
- docker/podman: `serve --listen tcp:` published to a loopback port,
  client connects with `--tcp` — no peer check applies (TCP has no
  audit token).

Asserts: `check`, `ls`, `stat`, `get`, `put`, symlink confinement,
and bad-token rejection.

## Dev-mode overrides

`--dev` is the per-invocation opt-in for seams that would be backdoors
if the environment alone could enable them:

- `CONTAINER_MOUNT_RELEASES_URL` — redirect release lookups
  (`fetch-servers`, `update`) at a local fixture tree; honored only
  behind `--dev`.
- in-tree server discovery — `servers`/`server-path` search the musl
  target dirs first.

## Release assets

`fetch-servers` expects `container-mount-servers-<ver>.tar.gz` containing
`container-mount-server-aarch64` + `container-mount-server-x86_64`; `update` expects
`container-mount-<host-triple>-v<ver>.tar.gz` containing the `container-mount`
binary plus any `container-mount-server-*` it should install. Asset
integrity is the GitHub API-reported SHA-256 digest, checked before
install — the digest is the trust root, not the transport.

`scripts/build-pkg.sh` produces all release artifacts (also the
macOS `.pkg` and the SBOM) through the `scripts/pkg/` helpers. A
clean checkout builds inside a `git worktree` of the release tag (or
of HEAD for a dev build) under `scratch/`; `--no-worktree` builds in
place with tag + clean still enforced, `--allow-dirty` drops that
requirement. Unanchored builds get `-<hash>`/`-dirty` name mangling,
unsigned ones `-unsigned`. `scripts/build-pkg-signed.sh` wraps it
with the maintainer's Developer ID identities (and optional
`GPG_KEY_ID`) from the gitignored `packaging/signing.local.conf`;
signed builds also emit a detached code signature for the tarball
(`*.tar.gz.sig`) plus a GPG `.asc`. `scripts/upload-release-artifacts.sh
vX.Y.Z` verifies them and swaps the CI-built unsigned assets on the
draft release — see [docs/design/packaging.md] and
[docs/design/release.md].

## Maintainer scripts

- `scripts/install-local.sh` — `cargo install --path .` plus
  `scripts/build-servers.sh --install ~/.cargo` (both musl servers
  into `libexec/container-mount/`).
- `scripts/dev-clean.sh` — remove every install: pkg payload, cargo
  binary, `libexec/container-mount/` server dirs (`-n` previews).
- `scripts/build-servers.sh` — just the musl builds; `--install
  PREFIX` copies them into `PREFIX/libexec/container-mount/`.
- `scripts/release-gate.sh` — the pre-tag gate (above + musl
  clippy/build, notices freshness, e2e when an engine is up).

[docs/design/packaging.md]: docs/design/packaging.md
[docs/design/release.md]: docs/design/release.md
