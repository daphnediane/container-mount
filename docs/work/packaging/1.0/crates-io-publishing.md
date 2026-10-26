# Decide crates.io publishing

- **Issue:** [#1]
- **Milestone:** `1.0`

The name was free as of 2026-10-13. The pitch: a general-purpose
guest file server — anything that runs Linux guests under a VMM with
a socket relay (Lima, Colima, OrbStack-style VMs,
Kata/cloud-hypervisor setups, devcontainer tooling) could reuse the
server plus the host client/`proto`. Publishing implies a stability
commitment: the handshake and wire format documented as a contract
([protocol]), semver discipline on `proto`, and CLI stability. Until
decided, install paths are the `.pkg`, the binstall tarball, `cargo
install --path`, and `container-mount update`/`fetch-servers` ([packaging]).

[#1]: https://github.com/daphnediane/container-mount/issues/1
[packaging]: ../../../design/packaging.md
[protocol]: ../../../design/protocol.md
