# container-mount

Host access to a Linux guest's filesystem — a `\\wsl$`-style file
bridge, split out of [container-distro] so any container/VM
environment can use it.

The project has two layers:

- **The file bridge** — the wire protocol, the guest file server, and
  the validating host client, all in one `container-mount` binary
  (below). It was called `distrofs` before the first release.
- **`container mount`** (first slice) — a `podman mount`-style
  plugin for Apple's `container`. Today `container mount fs TARGET
  ls|stat|get|put|cp|check` reaches any *running* container or
  machine with no create-time flags (vsock dial through the
  apiserver); stopped disks, images, and real mount points are still
  in design. container-distro will call it through its CLI alone. See
  [docs/design/mount-backends.md](docs/design/mount-backends.md) and,
  for the upstream internals it depends on,
  [docs/design/container-internals.md](docs/design/container-internals.md).

One `container-mount` binary, both ends:

- **Guest server** (`container-mount serve`) — a static `*-linux-musl` build
  that runs inside the container/VM as root, serves the rootfs over a
  Unix socket (or `--listen tcp:`), and exits when idle. Symlink
  targets are confined to the guest root; virtiofs/pseudo filesystems
  present as empty read-only directories.
- **Host client** (`container-mount ls|stat|get|put|check`) — connects over a
  Unix socket (`--socket`, or a managed path from `socket-path`), an
  inherited fd (`--fd`), or TCP (`--tcp`). It owns the whole connect
  path: socket-directory and leaf hygiene, an optional macOS
  code-signing check on the socket peer (`--peer-requirement`), and a
  mutual HMAC-SHA256 handshake over a per-session token
  (`CONTAINER_MOUNT_TOKEN`).

The embedder — container-distro, or your own tooling — supplies the
socket, the token, and the mapped command arguments. It never needs to
know the wire protocol.

## Layout

- `src/proto.rs` — the wire protocol (postcard-framed serde types,
  mutual-handshake constants)
- `src/connect.rs` — transports + socket-path hygiene + peer check
- `src/client.rs`, `src/symlink.rs` — the validating host client
  (the code a future FSKit module will link through a C ABI)
- `src/servers.rs` — locate/report installed and in-tree server
  binaries by arch + version stamp
- `src/release.rs` — release lookup, SHA-256-verified downloads
- `src/dial.rs` — the apiserver vsock dial (raw XPC, signature-pinned)
- `src/bin/container-mount/plugin/` — the `container mount` plugin
- `src/bin/container-mount/` — the binary: `serve`, the fs commands, and
  `servers`/`server-path`/`fetch-servers`/`update`

## Server binaries

Releases ship `container-mount-servers-<ver>.tar.gz` with static musl builds
for `aarch64` and `x86_64`, one `<triple>/container-mount` per platform.
Install the ones matching this `container-mount`'s own release next to it
(or under any prefix) with:

```bash
container-mount fetch-servers            # <prefix>/libexec/container-mount/<triple>/
container-mount fetch-servers --prefix /usr/local
container-mount server-path --triple arm64 --fetch   # fetch if missing, print path
```

`container-mount servers` reports what it can find (platform, version, protocol)
without executing anything. `container-mount update` self-updates an
installed binary — it verifies the SHA-256 the release API reports,
checks the macOS code signature on signed builds, and never
self-elevates.

## The `container mount` plugin

On macOS with Apple's `container`, register the plugin so `container
mount ...` works:

```bash
container-mount install-all      # fetch missing servers + install the plugin
container-mount uninstall-all    # remove the plugin again
```

The `.pkg` installs it already. The plugin is a **copy** of this
binary plus its servers under `<container
prefix>/libexec/container-plugins/mount/` (not a link: that directory
is root-owned, and other users run what's in it); `install-all` uses
`sudo` only for those file operations, and a re-run with nothing new
doesn't prompt. `container-mount update` replaces only the main
binary and says when the plugin copy needs an `install-all`.

## Verifying downloads

Signed builds sign the pkg, the host binary, and the client tarball
-- the tarball gets a detached code signature (`.tar.gz.sig`) and a
`gpg --detach-sign` signature (`.tar.gz.asc`). Drafts and prereleases
may still carry the `-unsigned` CI build.

| Artifact                    | Check                                            |
| --------------------------- | ------------------------------------------------ |
| `.pkg`                      | `pkgutil --check-signature <file>.pkg`           |
| `*.tar.gz` (code signature) | `codesign --verify --detached=<file>.sig <file>` |
| `*.tar.gz` (GPG)            | `gpg --verify <file>.asc <file>`                 |
| extracted `container-mount` | `codesign -dv <path>`                            |

Expected signers:

<!-- cspell:ignore AEDF dvvv -->

- `.pkg`: `Developer ID Installer: Daphne Pfister (YN9EG6U9PY)`;
  detached `.sig` + binary: `Developer ID Application: Daphne
  Pfister (YN9EG6U9PY)`. The certificates chain to the Apple Root CA
  (`codesign -dvvv --detached=` lists them under `Authority=`); the
  signer name + team ID `YN9EG6U9PY` are the stable check.
- `.asc`: GPG `29F8 7CBD 7DF6 A40E 2A44 7111 6F4D 7952 6D9B AEDF`
  (Daphne Pfister <daphnediane@mac.com>). Fetch the public key with
  `curl https://github.com/daphnediane.gpg | gpg --import` or `gpg
  --keyserver keys.openpgp.org --recv-keys
  29F87CBD7DF6A40E2A4471116F4D79526D9BAEDF` -- or import the copy in
  [packaging/release-pubkey.asc](packaging/release-pubkey.asc).

The musl servers are ELF and can't carry a Mac signature: the server
bundle's integrity is the release digest `fetch-servers` checks plus
the servers' embedded version stamp.

## Testing

`tests/e2e.sh` exercises the real path against the first available OCI
engine — Apple's `container` (Unix-socket relay + peer check), Podman,
or Docker (TCP) — building a `FROM scratch` image with the musl server
and running `check`/`ls`/`stat`/`get`/symlink-confinement/bad-token
assertions. It skips cleanly with no engine.

`tests/plugin-e2e.sh` (macOS, Apple `container`) drives the plugin
path instead: no create-time flags, copy delivery, vsock dial —
against alpine, amd64, `FROM scratch`, and (with `--machine`)
`container machine` targets.

## Security model

The token never touches argv (environment only), the client proves it
to the server as well as the reverse, and every host-side defense —
directory/leaf ownership, peer signature, symlink confinement — is in
the client where it can't be skipped. See
[docs/design/security.md](docs/design/security.md) for the threat
model and its deliberate trade-offs, and
[docs/design/protocol.md](docs/design/protocol.md) for the wire
protocol, transports, and client validation.

## License

[BSD-2-Clause](LICENSE).

[container-distro]: https://github.com/daphnediane/container-distro
