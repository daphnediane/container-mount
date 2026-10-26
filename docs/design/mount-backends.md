# Mount back-ends: reaching any container

How container-mount can serve containers other than container-distro's
distros — running containers created by plain `container run`, `FROM
scratch` images, machines, and stopped or never-booted disks — so a
`podman mount`-style `container mount ID [DIR]` plugin can sit on top,
and container-distro can get its distro mounts from that plugin
through nothing more than its CLI. Also records why we keep our own
protocol rather than virtiofs or another standard one.

Written 2026-10-25 against `container` 1.6.1 (source at `804209b1`,
containerization `fcbf6b7`) on macOS 27. Facts marked *verified* were
checked by experiment on that version; everything else is design
intent. Companion to [protocol.md](protocol.md) (transports, client
validation) and container-distro's [host-file-access] (the distro
deployment this generalizes).

## TL;DR

- **`podman mount` is not OCI.** It is a containers/storage operation
  that only works because a Linux host's kernel already holds the
  container's root. Apple `container` roots are ext4 images owned by
  a VM, so a mount has to go through a server running *in* a guest:
  the target's own VM when it is running, or a **helper VM** that
  boots the target's disk when it is not.
- **Three independent choices** make up a back-end: where the server
  runs, how its binary gets there, and how the host reaches it.
  container-distro's first integration (assets volume +
  `--publish-socket`, reverted from its `main` and parked on
  `feature/distrofs`) was one cell of that matrix; every other target
  needs a different cell.
- **The plugin owns the mechanics; container-distro only calls it.**
  `container mount` (shipped from this repo) holds the servers,
  tokens, transports, and helper VMs. container-distro knows no
  container-mount protocol, binary, or socket layout: it runs `container
  mount` commands and, optionally, passes through create-time
  references the plugin hands it as JSON.
- **vsock dial reaches any running container with no create-time
  flag** (*verified*). `container-apiserver`'s `containerDial` XPC
  route hands back a connected fd to a guest vsock port; `container
  build` uses it to reach buildkit. A process inside an ordinary
  container can listen on vsock. This is the key alternative to
  `--publish-socket`.
- **Delivery besides `container copy`:** a create-time read-only share
  of the servers directory, or no delivery at all (the helper VM
  carries the server). `container copy` of a *directory* keeps the
  exec bit (*verified*); a single file lands `0644` and can't run in
  an image with no `chmod`.
- **The helper VM is the true `podman mount` analogue** — it mounts
  storage without running the workload, covers stopped and
  never-booted containers, machines, and images, and offers a free
  read-only snapshot via `clonefile`.
- **virtiofs can't do this.** Virtualization.framework's virtiofs only
  shares host directories *into* a guest; inverting storage (distro
  root as a host folder) loses ownership, setuid, and device nodes
  (*verified*) and lands guest symlinks verbatim on the host. The
  custom protocol stays.

## Prior art: `podman mount`

`podman mount` (alias `podman container mount`) and `buildah mount`
ask **containers/storage** — Podman's storage library, not any OCI
spec — to overlay-mount the container's layers on the host and print
the path. The OCI runtime spec defines only `state`, `create`,
`start`, `kill`, and `delete`; the image spec defines layers but no
mount operation. Engine equivalents:

| Engine     | Equivalent                                                 |
| ---------- | ---------------------------------------------------------- |
| Podman     | `podman mount` / `podman image mount` (containers/storage) |
| Buildah    | `buildah mount` (same library)                             |
| containerd | snapshotter `Mounts()` (`ctr snapshots mounts`)            |
| Docker     | none (`docker cp` / `docker export` only)                  |

Podman restricts it: rootless mode mounts only the VFS driver unless
run inside `podman unshare` ([podman-mount(1)]), and the command is
registered for the local engine only (`registry.EngineMode:
registry.ABIMode` in `cmd/podman/containers/mount.go`) — so the
remote client used with `podman machine` on a Mac doesn't have it. There
is no prior art on macOS to copy; the closest model is WSL's
`\\wsl$`, which is the guest-served shape container-mount already has.

## The three choices

A back-end picks one answer from each:

1. **Where the server runs** — in the target's own VM (live view,
   target must be running) or in a helper VM that boots the target's
   disk (target stopped, or a snapshot of a running one).
2. **Delivery** — how a `container-mount` server binary gets into that VM.
3. **Transport** — how the host gets a connected stream to it.

The handshake, protocol, and client-side validation are identical in
every cell; the client only ever sees a socket or an fd.

## Delivery

| Option                          | When it applies            | Status   |
| ------------------------------- | -------------------------- | -------- |
| assets volume (`/sbin.distro`)  | distros, at create         | reverted |
| read-only servers share         | any container, at create   | proposed |
| `container copy` of a directory | running containers         | verified |
| `container copy` of one file    | running, image has `chmod` | weak     |
| exec stdin into a shell         | running, image has `sh`    | weak     |
| interpreter `memfd` exec        | running, has python/perl   | rejected |
| custom `--init-image`           | any container, at create   | rejected |
| none -- helper VM carries it    | stopped disks, images      | proposed |

**Assets volume (reverted).** container-distro copied the server
into each distro's read-only `/sbin.distro` assets. That ties
container-distro to container-mount's binaries and layout, which the plugin
design below removes; it stays parked on container-distro's
`feature/distrofs` branch.

**Read-only servers share.** `-v <prefix>/libexec/container-mount:/.container-mount:ro`
at create time puts the server binaries in the guest with no copy,
works for `FROM scratch` images, and picks up new builds live. A
plugin can't inject flags into `container run`, so this is opt-in:
`container mount prepare` prints the references (see [the
plugin](#prepare-create-time-references)) for container-distro, or a
user, to add. The server already presents virtiofs mountpoints as
empty, so the share doesn't appear in its own view.

**`container copy` of a directory.** *Verified* on alpine: copying a
directory keeps file modes (`0755` stays executable) but the files
are owned by the *host* uid inside the guest (1027 in the test). A
single file instead lands `0644` owned by root, which an image
without `chmod` can never execute. So delivery copies a directory
holding the server. Caveat: a guest process running as that uid could
replace the binary before root execs it. Copying under a root-only
parent (`/root`, normally `0700`) closes that where the parent exists;
where it doesn't, the residual risk is guest-internal (guest uid to
guest root), not host-facing — the client enforces everything that
protects the host. Not yet verified: copy into a `FROM scratch`
container (no `/tmp`, possibly read-only root).

**Rejected.** Streaming over `exec -i` into `sh -c 'cat > f; chmod
+x f'` depends on the image's shell and on a writable, exec-able
directory. `memfd_create` + `execve` needs an interpreter in the
image. A custom `--init-image` replaces Apple's `vminitd` guest agent
for that container — far too much surface to own for file access.

## Transports

| Transport                   | Reaches                    | Create-time flag | Status   |
| --------------------------- | -------------------------- | ---------------- | -------- |
| `--publish-socket` relay    | containers created with it | yes              | verified |
| vsock via `containerDial`   | any running container      | no               | verified |
| `exec -i` stdio (`--stdio`) | any running container      | no               | planned  |
| TCP on the guest IP         | containers with a network  | no               | rejected |

### vsock via `containerDial`

*Verified* (2026-10-25): an alpine container started by plain
`container run -d` ran `socat VSOCK-LISTEN:5555`; a 30-line Swift
program sent `container-apiserver` the XPC message below and got back
a connected fd that read the guest's reply.

```text
service: com.apple.container.apiserver   (user-domain Mach service)
route:   "com.apple.container.xpc.route" = "containerDial"
keys:    "id" = <container id>, "port" = <u64 vsock port>
reply:   "fd" = connected stream; "com.apple.container.xpc.error" on failure
```

The runtime helper answers only while the container is `running` or
`booted`. Upstream's own client is `ContainerClient.dial(id:port:)`,
which `container build` uses to reach buildkit on port 8088 — so the
route has an in-tree consumer, but no CLI verb and no documented
contract. Using it is an L1 dependency (container-distro's
[integration levels]) on one narrow route; raw XPC is a C API, so
the embedder can call it from Rust, or ship a tiny helper.

Properties that make it attractive:

- **No create-time flag** — works on containers we didn't create, and
  could replace `--publish-socket` for distros too (no host socket
  path, so no directory/leaf hygiene).
- **Peer identity moves to XPC.** Instead of an audit-token check on a
  socket peer, the embedder pins the apiserver with
  `xpc_connection_set_peer_code_signing_requirement` before sending.
- **Port squatting is cheap to prevent.** Linux reserves vsock ports
  below 1024 for `CAP_NET_BIND_SERVICE`. *Verified*: a uid-1000 guest
  process could bind port 5600 but got `EPERM` on port 800. `vminitd`
  owns port 1024, so the server should listen on a fixed port below
  1024; a guest *root* squatter still fails the handshake.
- **No network needed** in principle — vsock is not tied to the
  container's network namespace. Not yet verified under `--network
  none`.

### `exec -i` stdio

`container exec -i -u 0 ID <server> serve --stdio` ([#11]) reaches any
running container that has the binary and needs no XPC. Costs: all
traffic rides exec's stdio relay (slower), and it is exposed to the
upstream first-write drop on exec stdio noted in container-distro's
upstream-limitations. Keep it as the fallback when the dial route is
unavailable.

### TCP

`serve --listen tcp:` on the guest's vmnet IP is reachable from every
other guest on the subnet and absent under `--network none`. It stays
for Docker/Podman, which can only publish ports; not used for Apple
`container`.

## Stopped disks: the helper VM

When the target isn't running — or must not be started — the server
runs in a scratch container that boots the target's *disk* instead of
its workload. container-distro's `distro export` proves the mechanism:
it creates a scratch container and points its root disk at another
ext4 image (`rootFsOverride` / the conventional
`containers/<id>/rootfs.ext4`; see its upstream-limitations). The
plugin reimplements that trick itself rather than sharing code — a
small duplication, kept on purpose so the two stay loosely coupled.

Shape:

- **Root disk:** the target's ext4 (or a clone of it).
- **Entrypoint:** the server, from a read-only servers share —
  `--entrypoint /.container-mount/container-mount-server-<arch> serve …`, the same
  "entrypoint inside a volume" trick distros use for `/sbin.distro/
  init`. The target's own init never runs.
- **Transport:** vsock dial or `--publish-socket` — the helper is ours,
  so either works.
- **Ownership is real** — it's the target's own ext4 under a root
  server, unlike virtiofs (below).

Two modes:

| Mode      | Disk                         | Access     | Target may run? |
| --------- | ---------------------------- | ---------- | --------------- |
| snapshot  | `clonefile` copy of the disk | read-only  | yes (stale)     |
| exclusive | the real disk, under a lock  | read-write | no              |

- **Snapshot** costs nothing on APFS and never touches the source.
  Taken from a running container it is only crash-consistent and goes
  stale immediately; the clone is private, so ext4 journal replay on
  mount is harmless. The server refuses writes in this mode.
- **Exclusive** is only for a disk whose caller owns its lifecycle —
  `container mount --disk PATH --exclusive`, where the caller promises
  nothing else attaches the disk. container-distro does this for
  stopped distros under its distro lock and refuses `start` while the
  disk is mounted. Writing a regular container's `rootfs.ext4` would
  mutate upstream-owned state with nothing stopping a concurrent
  `container start`, so container targets stay read-only.

What it reaches, by disk location. The plugin reads these Layout-tier
internals directly, except the distro journal, whose path
container-distro passes in:

| Target                       | Disk                                         |
| ---------------------------- | -------------------------------------------- |
| stopped distro               | journal `rootfs.ext4`, via `--disk`          |
| stopped machine              | plugin-state `rootfs.ext4`                   |
| stopped container            | `containers/<id>/rootfs.ext4`                |
| never-booted container       | image snapshot `snapshots/<digest>/snapshot` |
| image (`podman image mount`) | image snapshot, as above                     |

## Choosing per target

| Target                 | State   | Primary                     | Fallback            |
| ---------------------- | ------- | --------------------------- | ------------------- |
| distro                 | running | vsock dial + servers share  | `--publish-socket`  |
| distro                 | stopped | helper VM, exclusive        | boot the distro     |
| machine                | running | vsock dial + directory copy | exec stdio          |
| machine                | stopped | helper VM, snapshot         | --                  |
| container              | running | vsock dial + directory copy | helper VM, snapshot |
| container from scratch | running | vsock dial + directory copy | helper VM, snapshot |
| container              | stopped | helper VM, snapshot         | --                  |
| image                  | --      | helper VM, snapshot         | --                  |

Containers created with the read-only servers share skip the copy.
Distros get it through `prepare` when container-distro uses it.

## The `container mount` plugin

### Ownership and coupling

- **The plugin ships from this repo** — proposed as a second binary,
  installed as `<prefix>/libexec/container-plugins/mount/bin/mount`
  (upstream takes a plugin's name from its binary's filename, as
  with `distro/bin/distro`). It links the client library directly (no C ABI needed),
  releases lock-step with the servers it launches, and is the one
  place that knows the `container` internals mounts need: the dial
  route, disk locations, the helper VM. It knows nothing of distros.
- **container-distro knows nothing of container-mount** — no wire protocol,
  no server binaries, no tokens, no socket layout. Its whole
  integration is invoking `container mount` and, optionally, passing
  through the create-time references `prepare` returns. Without the
  plugin installed, its file features say so and do nothing else.
- **Contract:** the plugin's CLI and its `--format json` output,
  versioned by an *interface* number separate from the wire protocol
  version. That is the only thing the two projects share.

### Surface (sketch)

Upstream plugins dispatch on the top-level verb (`container distro`,
`container machine`), so a plugin can't add `container image mount`;
images take `--image`. `container umount` would be a second plugin
name for the same binary.

```text
container mount [--snapshot | --exclusive] TARGET [DIR]   # container id
container mount --image REF [DIR]
container mount --disk PATH [--exclusive] [DIR]           # ext4 the caller owns
container mount fs TARGET ls|stat|get|put|check ...       # file ops, no FSKit
container mount list --format json                        # mounts, disks held
container mount prepare --id ID --format json             # create-time references
container mount version --format json                     # interface number
container umount TARGET|DIR
```

### What container-distro does with it

| container-distro operation     | Plugin call                                                |
| ------------------------------ | ---------------------------------------------------------- |
| mount a running distro         | `container mount <runtime-id> [DIR]`                       |
| mount a stopped distro         | under the distro lock: `--disk <journal disk> --exclusive` |
| `distro fs ...` file ops       | `container mount fs <runtime-id> ...`                      |
| `start` a distro               | `list --format json`; refuse if its disk is held           |
| `stop` / `rm` a mounted distro | `container umount <runtime-id>` first                      |
| materialize with files on      | optionally add `prepare` output to `create` args           |
| `--restricted` distro          | never calls the plugin (policy only -- see below)          |

container-distro's `files` setting keeps its role as policy — whether
container-distro offers mounts for a distro and passes `prepare`
references — but no longer gates any mechanism.

### `prepare`: create-time references

The one place container-distro passes plugin-supplied values into a
container. Everything in it is optional: vsock dial plus a directory
copy works with no create-time help, so `prepare` only saves the copy
and adds a socket relay as a second transport. Sketch:

```json
{
  "interface": 1,
  "volumes": [
    { "source": "/usr/local/libexec/container-mount", "destination": "/.container-mount", "readonly": true }
  ],
  "publishSockets": [
    { "host": "<darwin user temp>/container-mount/s/<hash>.sock", "guest": "/var/lib/container-mount/container-mount.sock" }
  ]
}
```

- **Called at every materialize, never journaled.** Distros are
  re-created from the journal at each start, so a plugin upgrade (new
  server path, new socket layout) takes effect on the next start, and
  no stale plugin path ever lands in a journal.
- **Validated before use.** container-distro accepts only these keys;
  volumes must be absolute, read-only, and outside `$HOME`; socket
  host paths must sit under the per-user Darwin temp dir. A bad value
  drops the references with a warning — the plugin still works over
  vsock dial.
- **Keyed by runtime id** (`--id`), which container-distro picks
  before `create`, so the plugin can derive the socket path and later
  find it again from the id alone.

### Compatibility

`container mount version --format json` reports `{ "interface": N,
"version": "<container-mount version>" }`. container-distro declares the
interface range it understands and treats anything else as "plugin
absent", with a message naming both versions. A missing plugin shows
up as `container`'s unknown-command error, which container-distro maps
to the same message.

### Presenting the mount

The FSKit module. Until that lands there is no first-party way to
present a mount point, so `container mount fs` (the old `container
distro fs` verbs, generalized) comes first and `container mount
TARGET DIR` arrives with FSKit. macFUSE or FUSE-T could be an
optional earlier back-end, at the cost of a third-party install.

## Why not virtiofs (or another standard protocol)

Virtualization.framework's virtiofs is one-way: the host serves a
`VZSharedDirectory` and the guest mounts it. There is no device for a
guest to export files to the host and no vhost-user support to build
one, and macOS has no virtiofs client. The only way to "use virtiofs"
is to invert storage — keep the distro root as a host folder shared
in as `/` (upstream has an unused `VirtiofsType.rootfs` enum, but no
CLI path; an init could `switch_root` into a `-v` share).

*Verified* on `container` 1.6.1, alpine guest, share on a
case-sensitive APFS volume:

| Operation             | Result                                          |
| --------------------- | ----------------------------------------------- |
| ownership             | every file shows as owned by the viewing uid    |
| `chown 123:456`       | reports success, changes nothing                |
| `chmod 4755`          | setuid bit silently dropped                     |
| `mknod`               | `EPERM`                                         |
| fifo, hard link       | work                                            |
| symlink               | works; target stored verbatim on the host       |
| `user.*` xattr        | works; lands on the host file                   |
| `security.capability` | accepted; lands on the host file as a raw xattr |
| case sensitivity      | inherited from the host volume                  |

So a Linux root can't live there: `sudo`/`su` lose setuid, permissions
mean nothing (`/etc/shadow` reads as the viewer's own file), package
managers' `chown` silently no-ops, and device nodes can't exist. A
default case-insensitive APFS volume breaks it further. It is also a
host-security regression: a guest link such as `x -> /etc/passwd`
sits on the host unchanged, so every host tool that follows it lands
on a real host file — exactly the escape container-mount's client-side
symlink rewriting exists to stop. And metadata-heavy work is why
upstream backs roots with ext4 block images in the first place.

The other standard options stay rejected for the reasons in
host-file-access's alternatives table: the guest kernel has no 9P
(`CONFIG_NET_9P` off) and macOS has no 9P client; NFS, SMB, and SFTP
bring loopback-auth or network-exposure problems. Adopting the **FUSE
wire format** itself was also considered: for distro-to-distro mounts
a guest kernel could consume a peer's stream nearly directly, but
symlink and name validation would still need a middle layer, and the
FUSE protocol assumes a trusted daemon. Our ops are already
FUSE-shaped; the custom protocol costs little and we own both ends.

## Security notes

To land in [security.md](security.md) (and container-distro's) with
the code, not before:

- **vsock dial** adds a host-trusted fd source: the apiserver over
  XPC. Pin its code signature on the connection. Any same-user process
  can already dial any running container — the same reach as
  `container exec` — and still has to pass the handshake.
- **Fixed port below 1024** so unprivileged guest processes can't
  squat it; guest root can, and fails the handshake.
- **Copy-delivered binaries** are owned by the host uid inside the
  guest; copy under a root-only parent where one exists. Residual risk
  is guest-internal.
- **Helper VMs** run no guest workload, so nothing in the target can
  race or replace the server; the disk's *contents* are as untrusted
  as ever and the client validates them the same way. Snapshot mode
  never writes the source; exclusive mode is distros-only, under the
  distro lock.
- **Restricted distros become policy-only.** With vsock dial, any
  same-user process can run `container mount <runtime-id>` on a
  running restricted distro; `files: off` only means container-distro
  won't. That was always true in reach (`container exec` works
  anyway), but container-distro's docs must say it next to
  `--restricted`'s claims.
- **`prepare` output is plugin-supplied input to `create`.** The
  plugin runs from `container`'s own plugin root, so it is as trusted
  as `container` itself; container-distro still validates the shape
  (read-only volumes outside `$HOME`, sockets under the per-user temp
  dir) so a buggy plugin can't widen a distro's host reach.
- **Browsing arbitrary containers and images** puts untrusted content
  in front of Finder, Quick Look, and Spotlight — mount `nobrowse`, as
  for distros (container-distro's T13).

## What this implies for this repo

- `serve --listen vsock:PORT` — new server transport (`AF_VSOCK`;
  `libc::sockaddr_vm` is bound for Linux). Host side needs nothing new:
  the embedder dials and passes the fd with `--fd`.
- `serve --stdio` ([#11]) — the exec fallback.
- A read-only serve mode for snapshots — the server refuses every
  mutating op itself, so the guarantee doesn't rest on how the
  embedder invokes the client.
- Release layout: the servers directory must be shareable read-only
  as a unit (it already is — `libexec/container-mount/`).
- The `mount` plugin binary: the dial client (raw XPC), the
  helper VM, `prepare`/`list`/`version`, and plugin install into
  `container`'s plugin root.

No GitHub issues are filed for these yet.

## Open questions

- vsock dial under `--network none`, and on a machine's backing
  container.
- `container copy` of a directory into a `FROM scratch` container.
- Helper VM: does `vminitd` mount a target disk whose own image config
  (user, workdir) is absent or odd without complaint, given an
  explicit entrypoint?
- Whether upstream would take a `container dial`-style CLI verb, which
  would turn the L1 dependency back into an L0 one.
- Plugin verbs vs container ids: `fs`, `list`, `prepare`, and
  `version` are also valid container names. Reserve them, require
  `--` before such a TARGET, or move them under flags?
- Plugin packaging: a second `[[bin]]` in the `container-mount`
  crate, or a cargo feature.
- Exclusive-mode lifetime: an FSKit mount outlives any CLI call, so
  something long-lived (the FSKit agent, or a supervisor tied to the
  helper container) must hold the disk. Is `list` enough for
  container-distro's `start` check, or should both sides also take an
  advisory `flock` on the disk file?

[host-file-access]: https://github.com/daphnediane/container-distro/blob/main/docs/design/host-file-access.md
[integration levels]: https://github.com/daphnediane/container-distro/blob/main/docs/design/lower-level-integration.md
[podman-mount(1)]: https://docs.podman.io/en/latest/markdown/podman-mount.1.html
[#11]: https://github.com/daphnediane/container-mount/issues/11
