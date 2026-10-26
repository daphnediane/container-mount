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

In October 2026 the repo (and its GitHub remote) was renamed from
`distrofs` to `container-mount`, to match the new direction in
[mount-backends.md](mount-backends.md): a `container mount` plugin
that owns host file access for any container, with container-distro
loosely coupled to it through the plugin's CLI instead of driving the
`distrofs` binary itself.

- **Renamed:** the repo, worktree (`mount/`), bare repo, and GitHub
  URLs. Issue numbers carried over with the repo.
- **Not renamed (yet):** the crate, binary, wire protocol (handshake
  magic, `DISTROFS_TOKEN`, version stamps), server binaries
  (`distrofs-server-*`), release artifacts, and signing identifier
  still say `distrofs`. Nothing depends on those names — no release
  has shipped, the protocol is lock-step before 1.0, and
  container-distro's `main` carries no distrofs integration — so
  renaming them is a free choice to make before the first release.
- container-distro's `feature/distrofs` integration predates the new
  direction; it is reference material, not a consumer. container-distro
  will integrate afresh against the plugin.
