# Changelog

User-facing changes per release, newest first. The top `## X.Y.Z-dev`
section is the running draft for the in-flight release (matching the
dev version in `Cargo.toml`): entries land there as work completes. At
release time the `-dev` heading is renamed to `## X.Y.Z -- YYYY-MM-DD`
and audited against the commit log.

## 0.2.0-dev

## 0.1.0 -- 2026-10-26

First release of container-mount, the `\\wsl$`-style file bridge split
out of [container-distro](https://github.com/daphnediane/container-distro)
(developed as `distrofs`): one `container-mount` binary that is the
static-musl guest server, the validating host client, and the
`container mount` plugin for Apple's `container`. Protocol 0 and
plugin interface 0 (1 is kept for 1.0.0); until then a fingerprint of
the protocol source in the handshake and the version stamp keeps builds
that differ on the wire apart, with a clear error.

### Added

- **`container mount` — files in any running container or machine,
  no create-time flags.** `ls`, `stat`, `cat`, and `copy` (`cp`),
  addressed scp-style as `[USER@]TARGET:PATH`: TARGET is a container id
  or machine name, relative paths and `~`/`~USER` resolve against the
  guest user's home, and new files belong to that user and their
  primary group (`root@web:x` → `/root/x` as `0:0`). The user is a home
  and an owner, not a permission check — the server is guest root, the
  reach `container exec` already gives. `copy` handles files and
  directories host to guest, guest to host, within a guest
  (server-side, reflinked where possible), and guest to guest; copies
  out keep only symlinks that stay inside the copied tree, never write
  through a host symlink, and drop setuid/setgid. `--user`, `--` to end
  verb recognition, "did you mean" for near-miss verbs, `list` and
  `--version [--format json]` for embedders. `attach`/`detach` (real
  mount points, with FSKit), stopped targets, and further verbs are
  reserved for later releases.
- **How the plugin reaches a guest:** it copies its own server in with
  `container copy`, starts it as root on a random vsock port below 1024
  with a fresh per-command token, and dials it through
  `container-apiserver` over an XPC connection pinned to Apple's
  signature. It warns on `container` releases outside the verified
  range; the upstream internals it relies on are listed in
  `docs/design/container-internals.md`.
- **Host client:** `ls`, `stat`, `get`, `put`, `cp`, `check`, and `user`
  over a Unix socket (`--socket`, managed paths via `socket-path`), an
  inherited fd (`--fd`), or TCP (`--tcp`) for engines that only publish
  ports. A mutual HMAC-SHA256 handshake over `CONTAINER_MOUNT_TOKEN`,
  an optional macOS `--peer-requirement` code-signature check on the
  socket peer, socket directory/leaf hygiene, and guest symlink
  confinement all live in the client. `get`/`put` keep sparse files
  sparse in both directions.
- **Guest server (`serve`):** serves the guest root with every lookup
  `*at()`-relative and no-follow, hides virtiofs and pseudo
  filesystems, and listens on a Unix socket, TCP, or vsock
  (`--listen vsock:PORT`). `--user NAME|UID[:GID]` resolves a user by
  the guest's own rules (libc, then its `/etc/passwd`/`/etc/group`) and
  owns host-created files as them; `--once`, `--remove-binary`, and
  `--token-env` support per-command servers. Optional ops behind
  capability bits: server-side `Copy`/`CopyRange`, `Seek`
  (`SEEK_DATA`/`SEEK_HOLE`), `Fallocate`, `FsyncDir`/`SyncFs`, xattrs,
  and `LookupUser`.
- **Installing:** `install-all` / `uninstall-all` (and
  `install-plugin` / `uninstall-plugin`) fetch this release's servers
  when missing and register the plugin as a self-contained, root-owned
  copy under `<container prefix>/libexec/container-plugins/mount/`,
  using `sudo` only for those file operations. Servers live at
  `libexec/container-mount/<triple>/container-mount` ([#3]);
  `servers`, `server-path --triple T [--fetch]`, and `fetch-servers`
  find and download them, always for the binary's own release.
- **Updating:** `update` self-updates from the release tarball with a
  digest check and, on macOS, a Developer ID and same-team check; on a
  pkg install it downloads the signed `.pkg` and hands it to
  Installer.app instead ([#2]). It notes when the plugin copy needs an
  `install-all`.
- **Releases:** a signed, notarized macOS `.pkg` (binary, servers,
  plugin, uninstall/update helpers), a binstall client tarball (with a
  detached code signature and a GPG `.asc`), and a servers bundle,
  built by `scripts/build-pkg.sh` from a worktree of the release tag;
  the README's *Verifying downloads* lists the signers.
- **Tests:** `tests/e2e.sh` drives the client against the server in
  Apple `container`, Podman, or Docker; `tests/plugin-e2e.sh` drives the
  plugin against alpine, amd64, `FROM scratch`, and machine targets.
  Both run in `scripts/release-gate.sh`.

[#2]: https://github.com/daphnediane/container-mount/issues/2
[#3]: https://github.com/daphnediane/container-mount/issues/3
