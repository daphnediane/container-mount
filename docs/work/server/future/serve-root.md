# `serve --root DIR`

- **Issue:** [#12]
- **Milestone:** none (future)

Serve a subtree instead of `/`; a one-fd change on Linux (the root
node's `openat` target). [portability]

[#12]: https://github.com/daphnediane/container-mount/issues/12
[portability]: ../../../design/server-portability.md
