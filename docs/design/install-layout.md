# Install layout and update policy

Where container-mount's files live, how the guest servers are discovered and
fetched, what "packaged" means, and the contract for embedders that
ship their own pinned copy. Companion to [packaging.md](packaging.md)
(release assets) and [protocol.md](protocol.md) (wire contract). The
per-OS server work this layout makes room for is analyzed in
[server-portability.md](server-portability.md).

Nothing here needs a compatibility path: neither container-mount nor a
container-distro release that consumes it has shipped, so the old flat
`container-mount-server-<arch>` names simply never exist.

## Canonical layout

Every install root — `/usr/local` for the pkg, `~/.cargo` for cargo
installs, an embedder's own root — looks the same:

```text
bin/container-mount
libexec/container-mount/<triple>/container-mount[.exe]      one dir per guest platform
share/doc/container-mount/{LICENSE,THIRD-PARTY-NOTICES.yaml}
share/man/man1/container-mount*.1                     clap-feature builds
bin/uninstall-container-mount.sh, bin/update-container-mount.sh   pkg only
```

`<triple>` is the Rust target triple the server was built for.
Subdirectories (not flat `container-mount-server-<triple>` files) because a
platform can need side-files later — a Windows server ships `.exe`
plus eventual helpers/manifests — and because it mirrors the in-tree
`target/<triple>/<profile>/container-mount` dev-build location, so
`--dev` discovery is the same lookup rooted elsewhere.

## Guest platforms

`servers::Arch` becomes a `Platform` table — rust triple ↔ display
label ↔ aliases for `--triple`:

| triple                       | label         | notes                      |
| ---------------------------- | ------------- | -------------------------- |
| `aarch64-unknown-linux-musl` | linux-arm64   | static musl, today's build |
| `x86_64-unknown-linux-musl`  | linux-amd64   | static musl, today's build |
| `aarch64-apple-darwin`       | macos-arm64   | with the macOS serve port  |
| `x86_64-apple-darwin`        | macos-amd64   | nested macOS VMs           |
| `aarch64-pc-windows-msvc`    | windows-arm64 | with the Windows port      |
| `x86_64-pc-windows-msvc`     | windows-amd64 |                            |
| `x86_64-unknown-freebsd`     | freebsd-amd64 | if wanted                  |

`servers` lists every table row (found or not); `server-path --triple
T` accepts the triple, the label, or a registered alias (`arm64`,
`amd64` resolve to the linux-musl rows). `fetch-servers` installs one
`<triple>/container-mount` per row the bundle carries.

## Discovery order

`server_path(triple)` tries, in order:

1. under `--dev`: `<repo>/target/<triple>/{release,debug}/container-mount`
   (in-tree builds first, as today);
2. the **per-user servers dir** — `~/Library/Application
   Support/container-mount/servers/<triple>/container-mount` on macOS,
   `${XDG_DATA_HOME:-~/.local/share}/container-mount/servers/<triple>/container-mount`
   elsewhere;
3. `<prefix>/libexec/container-mount/<triple>/container-mount` — bundled or
   fetch-installed;
4. when the running exe's own directory is named `container-mount`
   (a server binary inside `libexec/container-mount/`): sibling
   `<triple>/container-mount`.

The per-user dir precedes libexec so an explicit user fetch — a
hotfix server, a triple the package doesn't ship — shadows the
bundle without touching package-managed files. It is container-mount's
first per-user state: `uninstall-container-mount.sh` gains a keep/delete
prompt for it (the packaging doc's "no per-user state" claim ends
here).

## Lock-step versions

Client and servers ship from the same release and the handshake
wants them protocol-equal (below), so `fetch-servers` always fetches
the bundle for **its own** version — `v$CARGO_PKG_VERSION` — and
loses the `--version` flag. A flag that can only assemble mismatched
pairs buys nothing. Semantics: install every bundled triple not
already present; report the rest as already installed. If the own
tag has no release (never-published dev build) the error points at
`update`.

`server-path --fetch` is ensure-then-print: an installed server
prints its path; a missing one is fetched first (to wherever this
install may write), then printed. That makes server acquisition
scriptable — `container exec "$(container-mount server-path --triple
aarch64-unknown-linux-musl --fetch)" …` — and it's where "don't fetch
what the package already has" lands as behavior.

## The `packaged` feature

A compile-time cargo feature, set by `build-pkg.sh` on the release
builds — the pkg payload and the binstall/tarball binaries — and off
for `cargo install`/plain `cargo build`. Compile-time, not runtime
detection: the release build *knows* it ships a bundle; sniffing a
root-owned libexec can't tell "admin-installed cargo prefix" from
"pkg-managed receipt" reliably.

| operation                           | unpackaged          | packaged                                                                                                                                   |
| ----------------------------------- | ------------------- | ------------------------------------------------------------------------------------------------------------------------------------------ |
| `fetch-servers`, bundled triple     | install to libexec  | report already installed — the bundled copy **is** the fetch result (same release, same bytes)                                             |
| `fetch-servers`, non-bundled triple | install to libexec  | install to the per-user servers dir                                                                                                        |
| `server-path --fetch`               | fetch → libexec     | bundled → print bundled path, no download; missing → fetch → per-user dir                                                                  |
| `update`                            | client-tarball swap | prefer the signed pkg when a pkg receipt exists (the existing [work item](../work/packaging/1.0/update-prefers-pkg.md)), tarball elsewhere |

The fallback when libexec isn't writable applies everywhere, not
just under `packaged`: fetch → per-user dir, warn that the prefix
copy was skipped. An unpackaged binary in a root-owned prefix (a
distro-installed cargo prefix, say) behaves like a packaged one for
fetch purposes — the feature only changes what "already fetched"
means, not writability.

A `CONTAINER_MOUNT_SERVER=PATH` explicit-server override, if added, is a
`--dev` seam — same rule as `CONTAINER_MOUNT_RELEASES_URL` and the
`CONTAINER_CLI`/`DISTROFS` overrides in container-distro.

## The `clap` feature

The CLI is hand-rolled today to keep the musl server's dep tree and
binary size down (see the Cargo.toml header). Decided: a `clap`
cargo feature, **default on**, with the musl server builds compiled
`--no-default-features`:

- feature on: clap-derive frontend, `clap_mangen` man pages, and
  `install-man`/`uninstall-man` subcommands modeled on
  container-distro's (`<prefix>/share/man/man1/container-mount*.1`,
  stale generated-page pruning);
- feature off: the existing parsers, unchanged. The server build
  carries no clap, keeping the guest artifact small and its
  supply chain minimal.

Two parsers must agree on the same argv. The hand-rolled parser
stays compiled in test builds; a conformance test runs a shared
argv corpus through both and compares the resulting structs.
`packaged` and `clap` are orthogonal — release builds set both,
musl builds neither.

## Embedders: pinned pairs

An embedder that wants guaranteed client==server (container-distro's
plan: per-version copies under its own appdata root, e.g.
`…/container-distro/container-mount-4.3/`) **fetches and verifies release
assets itself**. container-mount's side is a documented contract, not a
fetch API:

```text
<embedder-root>/container-mount-<ver>/
    bin/container-mount
    libexec/container-mount/<triple>/container-mount
```

What container-mount guarantees the embedder can rely on:

- the servers tarball always mirrors the libexec layout —
  `<triple>/container-mount` entries — so unpacking it into a prefix
  reproduces this layout verbatim;
- the version stamp (below) distinguishes container-mount builds and
  reports version + protocol without executing the binary;
- `servers --dir ROOT` / `server-path --dir ROOT` (planned) audit
  and locate inside a foreign root, so an embedder can query its
  store through the same discovery logic instead of reimplementing it;
- signed builds always carry identifier
  `io.github.daphnediane.container-mount` — the requirement container-distro
  already pins before handing the client a session token.

container-distro-side follow-ups (tracked there, not here): its
`server_candidates` gains the pinned appdata roots ahead of the
prefix paths, and drops the pre-release `container-mount-server-aarch64` /
plain-`container-mount` names this layout retires.

## Stamp and build identity

The stamp grows optional trailing `key:value` pairs:

```text
container-mount-version:<ver>:protocol:<n>:commit:<sha>[:dirty]
```

`scan_stamp` iterates pairs after the version and ignores unknown
keys — old scanners still read version+protocol, and further fields
(`tag`, a build-content hash) can join later. `build-pkg.sh` injects
the commit via a `CONTAINER_MOUNT_COMMIT` env → `option_env!` (no vergen
dependency — musl must stay toolchain-free); `dirty` is set when the
tree isn't clean. This is the cheap half of version equality:
an embedder can prove client and server are the *same build*, not
merely protocol-compatible, before injecting the server.

## Protocol version policy

Pre-1.0 the handshake stays strict — `ClientAuth.version ==
proto::VERSION` or the server drops the connection. Nothing
negotiates because nothing needs to: the lock-step fetch policy and
pinned-pair embedder layout keep pairs matched by construction.

Post-1.0 the design target is **server-advertised ranges**: a 4.z
server whose wire semantics are a superset of 3.y's claims to serve
`3.y…=4.z` clients; a server whose messages changed meaning can't
claim the old range. That needs a handshake evolution — postcard
structs can't grow fields, so it lands as a new hello variant keyed
off the magic/version fields, plus a normative compat matrix in the
protocol-contract doc (already a [1.0 work
item](../work/protocol/1.0/wire-contract.md)). Ranges, the stamp's
build identity, and strict-equality-today together cover the three
questions an embedder asks of a server: *can it speak to me, which
release is it, is it the exact build I pinned.*
