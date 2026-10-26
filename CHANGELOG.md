# Changelog

User-facing changes per release, newest first. The top `## X.Y.Z-dev`
section is the running draft for the in-flight release (matching the
dev version in `Cargo.toml`): entries land there as work completes. At
release time the `-dev` heading is renamed to `## X.Y.Z -- YYYY-MM-DD`
and audited against the commit log.

## 0.1.0-dev

### Added

- Initial standalone release of the container-mount project (repo
  formerly `distrofs`), split out of
  [container-distro](https://github.com/daphnediane/container-distro):
  one `distrofs` crate/binary carrying the wire protocol, the
  validating host client (`ls`, `stat`, `get`, `put`, `check`), and
  the static-musl guest server (`serve`).
- Transports: Unix socket (`--socket`), inherited fd (`--fd`), and TCP
  (`--tcp` host / `--listen tcp:` guest) for engines that only publish
  ports. Managed private host socket paths via `socket-path`.
- Mutual HMAC-SHA256 session handshake over `DISTROFS_TOKEN`;
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
- `get`/`put` preserve sparse files end to end: only data extents
  cross the wire and holes stay unallocated on both sides.
- Release packaging: `scripts/build-pkg.sh` produces the macOS `.pkg`,
  the binstall client tarball, and the `distrofs-servers` bundle;
  `build-pkg-signed.sh` (Developer ID via gitignored
  `packaging/signing.local.conf`), `build-servers.sh`,
  `install-local.sh`, `dev-clean.sh`; design docs in
  `docs/design/{protocol,security,filesystems,packaging,release,install-layout,server-portability}.md`.
