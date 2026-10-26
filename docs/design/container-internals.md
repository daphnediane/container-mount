# Apple `container` internals we depend on

The `container mount` plugin ([mount-backends.md](mount-backends.md))
uses parts of Apple's `container` that are not a public, documented
interface. This doc lists each one, the version it was verified on,
and how to re-verify it when upstream ships a new minor. It is the
plugin's counterpart to container-distro's [container-internals]; the
two keep separate lists and gates on purpose, so neither project's
release waits on the other.

The guest side is not on this list: `AF_VSOCK` sockets, `/proc`, and
ext4 are Linux kernel interfaces, not upstream's.

Verified on `container` 1.6.1 (source `804209b1`), macOS 27,
2026-10-26.

## Integration levels

Using container-distro's [integration levels]:

| Level  | Meaning                                       | We use                          |
| ------ | --------------------------------------------- | ------------------------------- |
| L0     | public CLI, documented flags and output       | `inspect`, `copy`, `exec`       |
| L1     | undocumented API or behavior, stable in tree  | `containerDial`, exec env merge |
| Layout | files under `appRoot`                         | none yet (helper VM will)       |

L0 behavior we rely on beyond the documented surface (JSON field
names, copy semantics) is listed below too: it is what breaks first.

## The dial route (L1)

The core dependency. `container-apiserver` answers an XPC request
that returns a connected fd to a vsock port inside a running
container. Upstream's own client is `ContainerClient.dial(id:port:)`
(`Sources/Services/ContainerAPIService/Client/ContainerClient.swift`),
used by `container build` to reach buildkit; there is no CLI verb.
`src/dial.rs` speaks it over raw libxpc:

| Item                 | Value                                             |
| -------------------- | ------------------------------------------------- |
| Mach service         | `com.apple.container.apiserver` (user domain)     |
| route key            | `com.apple.container.xpc.route` = `containerDial` |
| request keys         | `id` (string), `port` (uint64)                    |
| reply fd key         | `fd` (`xpc_dictionary_dup_fd`)                    |
| reply error key      | `com.apple.container.xpc.error` (data)            |
| error payload        | JSON `{"code": ..., "message": ...}`              |
| answers while        | container is `running` (or `booted`)              |
| apiserver identifier | `com.apple.container.apiserver`                   |
| apiserver team       | `UPBK2H6LZM`                                      |

<!-- cspell:ignore UPBK2H6LZM -->

The connection is pinned with
`xpc_connection_set_peer_code_signing_requirement` to `identifier
"com.apple.container.apiserver" and anchor apple generic and
certificate leaf[subject.OU] = "UPBK2H6LZM"` before the request is
sent (`APISERVER_REQUIREMENT`). A locally built `container` fails
this check; there is deliberately no switch to relax it yet.

## CLI behavior (L0, undocumented details)

- **`container inspect ID`** prints a JSON array; we read
  `[0].configuration.id`, `[0].status.state` (`"running"`), and the
  default user from `[0].configuration.initProcess.user` — either
  `{"raw": {"userString": "1000:1000"}}` (empty means root) or
  `{"id": {"uid": …, "gid": …}}`.
- **`container copy DIR ID:DEST`** keeps file modes (`0755` stays
  executable), lands files owned by the host uid, creates missing
  parents root-owned `0755`, and nests `DIR` inside `DEST` when
  `DEST` already exists (like `cp -r`). So each command copies into
  a fresh directory. Works on `FROM scratch` images.
- **`container exec -d -u 0 -e NAME ID ARGV`** starts a detached root
  process; a bare `-e NAME` takes the value from the `container`
  process's environment, so a secret never reaches an argv.
- **exec environment merge (L1).** The exec process's environment is
  the container's environment *followed by* the `-e` entries, with no
  de-duplication; `getenv` returns the first match, so a variable the
  container already defines wins over `-e` (shells, which keep the
  last, hide this). The plugin therefore passes the token under a
  fresh `CONTAINER_MOUNT_TOKEN_<nonce>` name (`serve --token-env`).
  The same symptom on `run` was [apple/container#1212], fixed by #1218
  inside `Parser.allEnv`; `ContainerExec` appends its entries to the
  init process's environment outside that de-duplication.
  `tests/upstream-exec-env.sh` reproduces it.
- **`container machine inspect NAME`** prints a JSON array whose
  `[0].containerId` names the machine's backing container — an
  ordinary container the dial route reaches (plain `container ls`
  hides it) — and whose `[0].userSetup` (`uid`, `gid`, `username`,
  `home`) is the provisioned account, our default user. The home
  itself comes from the guest's passwd, which machine setup writes
  from `--home`.
- **`container system status --format json`** exposes
  `server.version`, which the version gate reads.
- **Guest kernel arch is the host's.** On Apple silicon, amd64 images
  run under Rosetta on an arm64 kernel, so the aarch64 server runs in
  every container.
- **Plugin discovery.** `container` runs
  `<prefix>/libexec/container-plugins/<name>/bin/<name>` for
  `container <name> ...`, and may pass `<name>` as `argv[1]` (we drop
  it). Shared with container-distro's notes.

## Version gating

`VERIFIED_CONTAINER_MINOR = ["1.6"]`
(`src/bin/container-mount/plugin/upstream.rs`) lists the `major.minor`
versions these checks have passed on. Every plugin path that dials or
copies calls `warn_unverified_version()` first: it warns once per
process on stderr when `server.version` isn't listed. It warns rather
than refuses, like container-distro: the client validates everything
the server sends, so a changed internal fails safe (a dial or
handshake error), not open. `version` and `help` never check.

**On a `container` upgrade, re-verify** (part of the
[release process](release.md)):

1. `cargo test --lib dial -- --ignored` passes: a wrong requirement is
   refused, and the real one reaches the apiserver (whose error JSON
   still parses).
2. The apiserver's designated requirement (`codesign -d -r-
   /usr/local/bin/container-apiserver`) still matches the identifier
   and team above.
3. `ContainerClient.dial` and the `XPCKeys`/`XPCMessage` names in the
   upstream source still match the table above.
4. `tests/plugin-e2e.sh --machine` passes: `ls`/`cat`/`copy` against
   an alpine container, an amd64 one, a `FROM scratch` one whose
   environment defines `CONTAINER_MOUNT_TOKEN`, and a machine — with
   home-relative paths, `USER@` ownership, and guest-to-guest copies.
5. `container copy` semantics above still hold (modes kept, parents
   created, nesting into an existing directory).
6. The exec environment merge: `tests/upstream-exec-env.sh` exits 1
   while it's broken. If upstream fixes it, the `--token-env`
   workaround stays harmless; note the fixed version here.
7. Then add the minor to `VERIFIED_CONTAINER_MINOR`.

[container-internals]: https://github.com/daphnediane/container-distro/blob/main/docs/design/container-internals.md
[apple/container#1212]: https://github.com/apple/container/issues/1212
[integration levels]: https://github.com/daphnediane/container-distro/blob/main/docs/design/lower-level-integration.md
