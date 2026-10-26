# container-mount

Host access to a Linux guest's filesystem — a `\\wsl$`-style file
bridge, split out of [container-distro] so any container/VM
environment can use it.

The project has two layers:

- **distrofs** — the wire protocol, the guest file server, and the
  validating host client, all in one `distrofs` binary (below). The
  crate, binary, protocol, and release artifacts still carry that
  name; whether they follow the rename is open until the first
  release.
- **`container mount`** (in design) — a `podman mount`-style plugin
  for Apple's `container` that reaches *any* container, stopped disk,
  or image, and that container-distro calls for its distro mounts
  through the plugin's CLI alone. See
  [docs/design/mount-backends.md](docs/design/mount-backends.md).

One `distrofs` binary, both ends:

- **Guest server** (`distrofs serve`) — a static `*-linux-musl` build
  that runs inside the container/VM as root, serves the rootfs over a
  Unix socket (or `--listen tcp:`), and exits when idle. Symlink
  targets are confined to the guest root; virtiofs/pseudo filesystems
  present as empty read-only directories.
- **Host client** (`distrofs ls|stat|get|put|check`) — connects over a
  Unix socket (`--socket`, or a managed path from `socket-path`), an
  inherited fd (`--fd`), or TCP (`--tcp`). It owns the whole connect
  path: socket-directory and leaf hygiene, an optional macOS
  code-signing check on the socket peer (`--peer-requirement`), and a
  mutual HMAC-SHA256 handshake over a per-session token
  (`DISTROFS_TOKEN`).

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
- `src/bin/distrofs/` — the binary: `serve`, the fs commands, and
  `servers`/`server-path`/`fetch-servers`/`update`

## Server binaries

Releases ship `distrofs-servers-<ver>.tar.gz` with static musl builds
for `aarch64` and `x86_64`. Install them next to an installed
`distrofs` (or any prefix) with:

```bash
distrofs fetch-servers            # <prefix>/libexec/distrofs/
distrofs fetch-servers --prefix /usr/local
```

`distrofs servers` reports what it can find (arch, version, protocol)
without executing anything. `distrofs update` self-updates an
installed binary — it verifies the SHA-256 the release API reports,
checks the macOS code signature on signed builds, and never
self-elevates.

## Testing

`tests/e2e.sh` exercises the real path against the first available OCI
engine — Apple's `container` (Unix-socket relay + peer check), Podman,
or Docker (TCP) — building a `FROM scratch` image with the musl server
and running `check`/`ls`/`stat`/`get`/symlink-confinement/bad-token
assertions. It skips cleanly with no engine.

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
