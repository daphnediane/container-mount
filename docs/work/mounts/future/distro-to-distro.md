# Distro-to-distro mounts

- **Issue:** [#10]
- **Milestone:** none (future)

The `/mnt/wsl` analogue: a guest-side `distrofs mount` FUSE client
spliced to a peer's server through a host broker. Design in
container-distro's [host-file-access]; the guest FUSE client and the
post-handshake stream handoff are distrofs's half.

[#10]: https://github.com/daphnediane/container-mount/issues/10
[host-file-access]: https://github.com/daphnediane/container-distro/blob/main/docs/design/host-file-access.md
