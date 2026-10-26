# Changelog

User-facing changes per release, newest first. The top `## X.Y.Z-dev`
section is the running draft for the in-flight release (matching the
dev version in `Cargo.toml`): entries land there as work completes. At
release time the `-dev` heading is renamed to `## X.Y.Z -- YYYY-MM-DD`
and audited against the commit log.

## 0.1.0-dev

### Added

- Initial standalone release of container-mount (developed as
  `distrofs` and renamed before release), split out of
  [container-distro](https://github.com/daphnediane/container-distro):
  one `container-mount` crate/binary carrying the wire protocol, the
  validating host client (`ls`, `stat`, `get`, `put`, `check`), and
  the static-musl guest server (`serve`).
- Transports: Unix socket (`--socket`), inherited fd (`--fd`), and TCP
  (`--tcp` host / `--listen tcp:` guest) for engines that only publish
  ports. Managed private host socket paths via `socket-path`.
- Mutual HMAC-SHA256 session handshake over `CONTAINER_MOUNT_TOKEN`;
  optional macOS `--peer-requirement` check on the unix peer's code
  signature; socket directory/leaf hygiene; guest symlink confinement.
- Server discovery (`servers`, `server-path`) by architecture and
  embedded version/protocol stamp; verified downloads via
  `fetch-servers`; self-update via `update` (`--dry-run`, `--force`,
  signed/unsigned asset selection).
- `tests/e2e.sh`: OCI-engine end-to-end test (Apple `container`,
  Podman, or Docker) exercising the full connect path without
  container-distro.
- Optional protocol ops, advertised via `ServerAuth` capability bits:
  `Copy`/`CopyRange` (server-side copies — reflinked where the
  filesystem allows, hole-preserving extent copies elsewhere),
  `Seek` (`SEEK_DATA`/`SEEK_HOLE` scans), `Fallocate` (preallocate,
  punch hole, zero/collapse/insert ranges), `FsyncDir`/`SyncFs`, and
  xattr get/set/list/remove.
- `cp` command — guest-side file copies without a wire round trip.
- `container mount` plugin, first slice: `fs TARGET ls|stat|get|put|cp|check`
  on any running container or `container machine` (by name) with no
  create-time flags — the server is copied in with `container copy`,
  started as root on a random vsock port below 1024 with a fresh
  per-command token, and reached through the apiserver's dial route
  over a code-signature-pinned XPC connection. `version --format json`
  reports the plugin interface number. Runs as `container-mount plugin
  ...` until installed as `container-plugins/mount/bin/mount`; warns on
  `container` releases outside the verified range
  (`docs/design/container-internals.md`). `tests/plugin-e2e.sh` covers
  alpine, amd64, `FROM scratch`, and machine targets.
- `install-all` / `uninstall-all` (and `install-plugin` /
  `uninstall-plugin`): fetch this release's servers when missing and
  register the `container mount` plugin as a self-contained copy
  (`container-plugins/mount/` with the binary and its servers), using
  `sudo` only for the file operations and skipping no-op reinstalls.
  The `.pkg` ships the plugin; `update` notes when the plugin copy
  needs refreshing.
- `serve --listen vsock:PORT`, plus `--once`, `--remove-binary`, and
  `--token-env NAME` for per-command servers.
- Per-triple server layout ([#3]): servers install as
  `libexec/container-mount/<triple>/container-mount`, and the pkg, the
  servers bundle, and the client tarball all carry that layout.
  `server-path --triple T` takes a triple, a label (`linux-arm64`), or
  `arm64`/`amd64`, and `--fetch` downloads a missing server first;
  `servers` lists platforms by label. `fetch-servers` always fetches
  its own release (no `--version`): client and servers are lock-step.
- `get`/`put` preserve sparse files end to end: only data extents
  cross the wire and holes stay unallocated on both sides.
- Release packaging: `scripts/build-pkg.sh` produces the macOS `.pkg`,
  the binstall client tarball, and the `container-mount-servers` bundle;
  `build-pkg-signed.sh` (Developer ID via gitignored
  `packaging/signing.local.conf`), `build-servers.sh`,
  `install-local.sh`, `dev-clean.sh`. Signed releases also sign the
  binstall tarball (detached code signature `.sig` plus a GPG `.asc`;
  the README's *Verifying downloads* lists the expected signers) and
  build from a `git worktree` of the release tag, so a canonical
  artifact name always means tag content; `upload-release-artifacts.sh`
  verifies and swaps them in over the CI draft. Design docs in
  `docs/design/{protocol,security,filesystems,packaging,release,install-layout,server-portability,container-internals}.md`.

[#3]: https://github.com/daphnediane/container-mount/issues/3
