# TODO

The working roadmap — open work only. Each item links to the document
with the analysis; the linked docs are the source of truth for
details. When an item is completed it leaves this file: the
user-facing change is recorded in the `X.Y.Z-dev` section of
[CHANGELOG.md](../CHANGELOG.md), which is the permanent record of what
shipped.

## For 1.0

- **Decide crates.io publishing** — the name was free as of
  2026-10-13. The pitch: a general-purpose guest file server —
  anything that runs Linux guests under a VMM with a socket relay
  (Lima, Colima, OrbStack-style VMs, Kata/cloud-hypervisor setups,
  devcontainer tooling) could reuse the server plus the host
  client/`proto`. Publishing implies a stability commitment: the
  handshake and wire format documented as a contract ([protocol]),
  semver discipline on `proto`, and CLI stability. Until decided,
  install paths are the `.pkg`, the binstall tarball, `cargo install
  --path`, and `distrofs update`/`fetch-servers` ([packaging]).
- **`update` prefers the signed pkg on pkg installs** — today it swaps
  the binary in place from the tarball, which fails (correctly) on a
  root-owned `/usr/local` install. Mirror container-distro's update:
  detect a pkg receipt, download the signed pkg, hand off to
  Installer.app.
- **Protocol contract doc** — [protocol] is the design; a published
  wire contract needs the message/enum tables, exact HMAC inputs, and
  the version/capability negotiation spelled out as normative text so
  a second implementation could interop.

## Feature roadmap

- **FSKit filesystem front-end** — the original motivation
  (`\\wsl$`-style mounts in Finder). The client library gets a C ABI
  so a sandboxed `FSUnaryFileSystem` module gets the same validation
  as the CLI; an agent brokers the connect/launch and passes the
  authenticated fd over XPC. Phased plan and the host-side app live
  in container-distro's [host-file-access]; the C ABI + inotify event
  ops land here first.
- **inotify events → cache invalidation** — server places watches on
  read directories/open files and pushes `Event::Invalidate`
  ([protocol]); designed for macOS 27's `FSVolume.DataCacheHandler`.
- **Later capabilities** — `btime` (`statx` via `libc::syscall`; the
  musl `libc` bindings don't expose the struct), BSD file flags
  (`FS_IOC_*FLAGS` ↔ chflags; `HIDDEN` has no Linux analogue), ACL
  mapping (POSIX ↔ NFSv4), advisory locks if FSKit surfaces them,
  batch `Lookup` for deep paths. `Mknod` stays deliberately absent.
- **Distro-to-distro mounts** — the `/mnt/wsl` analogue: a guest-side
  `distrofs mount` FUSE client spliced to a peer's server through a
  host broker. Design in container-distro's [host-file-access];
  the guest FUSE client and the post-handshake stream handoff are
  distrofs's half.
- **`--stdio`/exec transport** — `serve` over stdin/stdout for guests
  reached only by exec pipes (machines, not-yet-restarted distros).
- **x86_64 / universal host builds** — today only
  `aarch64-apple-darwin`; add `x86_64-apple-darwin` assets (and pick
  per-triple in `update`/`fetch`) if Intel support matters.

## Infra

- **CI coverage for the musl targets** — the release gate covers them
  locally; add a clippy/build job if pre-merge musl regressions start
  slipping through.
- **`make-fake-releases` equivalent** — the e2e fixture for
  `fetch-servers`/`update` is hand-rolled today
  (`DISTROFS_RELEASES_URL` behind `--dev`); a script like
  container-distro's would make the verified-download path part of the
  regular test loop.

[host-file-access]: https://github.com/daphnediane/container-distro/blob/main/docs/design/host-file-access.md
[packaging]: design/packaging.md
[protocol]: design/protocol.md
