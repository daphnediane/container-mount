# `serve` ports: macOS → FreeBSD → Windows

- **Issue:** [#13]
- **Milestone:** none (future)

Per-seam analysis (node-fd model, mount table, errno translation,
identity, per-op availability, transport) in [portability]. macOS
first: same syscall family, buys container-free testing, exercises
the code the FSKit front-end will front.

[#13]: https://github.com/daphnediane/container-mount/issues/13
[portability]: ../../../design/server-portability.md
