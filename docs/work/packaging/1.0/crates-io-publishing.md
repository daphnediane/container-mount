# crates.io: the 1.0 commitment

- **Issue:** [#1]
- **Milestone:** `1.0`

Published since 0.1.0 (2026-10-26): every release goes to crates.io
by hand from a worktree of its tag (docs/design/release.md), and 0.x
promises only semver's "anything may change" — protocol 0 and plugin
interface 0 say so on the wire.

What remains for 1.0 is the commitment a 1.0 crate implies: the
handshake and wire format documented as a contract ([protocol], #6),
semver discipline on the library (`proto`, `client`, `connect`), and a
stable CLI — or deciding which of those the library surface should
not promise, and hiding it. The pitch for that surface: a
general-purpose guest file server that anything running Linux guests
under a VMM with a socket relay (Lima, Colima, OrbStack-style VMs,
Kata/cloud-hypervisor setups, devcontainer tooling) could reuse.
Install paths: the `.pkg`, `cargo install`/`cargo binstall
container-mount`, and `container-mount update`/`fetch-servers`
([packaging]).

[#1]: https://github.com/daphnediane/container-mount/issues/1
[packaging]: ../../../design/packaging.md
[protocol]: ../../../design/protocol.md
