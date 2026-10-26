# History — the distrofs split

`distrofs` was split out of `container-distro` into this standalone
repo so the guest file server + wire protocol + client library are
usable by other container environments, not just Apple `container` via
container-distro.

## Repo layout (bare + worktrees)

Everything lives under `/Volumes/CaseSens/Repositories/mine/container-distro/`:

- `bare/` — bare git repos (`container-distro.git`,
  `container-mount.git`)
- `main/` — container-distro worktree (`main-distrofs/` holds its
  `feature/distrofs` branch)
- `mount/` — **this repo's** worktree (`bare/container-mount.git`,
  remote: `git@github.com:daphnediane/container-mount.git`)
- `workspace/` — scratch dir

## What moved here

From `main/` (container-distro):

- `crates/distrofs/` → `src/bin/distrofs/{serve,server,mounts}.rs` —
  the guest server, now built for both `aarch64` and `x86_64` musl
- `src/distrofs/proto.rs` → `src/proto.rs` — wire protocol
- `src/distrofs/client.rs` → `src/client.rs`, `symlink.rs` →
  `src/symlink.rs` — the validating host client library (later the C
  ABI the FSKit module links)
- container-distro's `fs` command implementations →
  `src/bin/distrofs/fscli.rs` (+ `table.rs`) — the host-side file
  commands (`ls`, `stat`, `get`, `put`, `check`); container-distro's
  `src/cli/commands/fs.rs` is only the arg mapper
- `src/distrofs/connect.rs` → `src/connect.rs` — generalized to
  unix/tcp/fd transports; the `container-distro`-specific socket-dir
  naming moved out (`socket-path` manages `<temp>/distrofs/s/` for
  embedders that want it; `--peer-requirement` takes the check as a
  parameter instead of baking in Apple's helper)

New here: `src/servers.rs` (arch/stamp discovery), `src/release.rs`
(verified downloads), `fetch-servers`, `update`, `socket-path`, TCP
listen/connect, `tests/e2e.sh` (container/podman/docker), and the full
repo scaffolding (rules, workflows, docs, deny.toml).

## What stayed in container-distro

Apple-`container`-specific deployment only, in `src/distro/distrofs.rs`:

- its own `<temp>/container-distro/s/` socket layout (stable across
  the split — already-published sockets keep resolving)
- `--publish-socket` arg building, `container exec -d` launch,
  `--files` resolution
- guest-binary location + a `distrofs-version:` stamp check
- client-binary location + verification (Developer ID requirement on
  signed builds, ownership/writability on unsigned)
- the `container-runtime-linux` peer requirement string

container-distro does **not** link the distrofs crate and knows no
wire-protocol types — it hands the `distrofs` binary a socket path, a
token, and mapped arguments, and propagates its exit code.

## Decisions taken

- **One crate**, not a workspace — lib + one `[[bin]]` serving both
  ends.
- **Client + symlink moved in the same pass** — container-distro's
  `fs.rs` is now a thin arg mapper over the distrofs binary.
- **Protocol version is the compatibility contract** between
  independently-released distrofs and container-distro
  (`proto::VERSION`, embedded stamps).

## Rename to container-mount

In October 2026, before any release, the project was renamed from
`distrofs` to `container-mount`, to match the new direction in
[mount-backends.md](mount-backends.md): a `container mount` plugin
that owns host file access for any container, with container-distro
loosely coupled to it through the plugin's CLI instead of driving the
binary itself. Everything above this section uses the old names as
they were at the time.

- **Repo:** the repo, worktree (`mount/`), bare repo, and GitHub
  URLs. Issue numbers carried over with the repo.
- **Crate and binary:** `container-mount` (library
  `container_mount`), sources under `src/bin/container-mount/`.
- **Wire protocol:** handshake magic `b"container-mount"` (was the
  8-byte `b"distrofs"`), HMAC labels `container-mount/c` and
  `container-mount/s`, and `proto::VERSION` 1 → 2, so a server built
  before the rename is rejected by its stamp instead of failing the
  handshake.
- **Runtime names:** `CONTAINER_MOUNT_TOKEN`/`_SOCKET`/
  `_RELEASES_URL`, the `container-mount-version:` stamp,
  `container-mount-server-<arch>`, `libexec/container-mount/`,
  `<temp>/container-mount/s/`, and the guest default
  `/var/lib/container-mount/container-mount.sock`.
- **Distribution:** pkg and signing identifier
  `io.github.daphnediane.container-mount`, artifact names
  (`container-mount-<ver>.pkg`, `container-mount-servers-<ver>.tar.gz`,
  …), and `uninstall-container-mount.sh`/`update-container-mount.sh`.

Nothing depended on the old names — no release had shipped, the
protocol is lock-step before 1.0, and container-distro's `main` had
no distrofs integration. container-distro's `feature/distrofs`
branch still uses them; it is reference material, not a consumer —
container-distro will integrate afresh against the plugin.
