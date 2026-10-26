# FSKit filesystem front-end

- **Issue:** [#7]
- **Milestone:** none (future)

The original motivation (`\\wsl$`-style mounts in Finder). The client
library gets a C ABI so a sandboxed `FSUnaryFileSystem` module gets
the same validation as the CLI; an agent brokers the connect/launch
and passes the authenticated fd over XPC. Phased plan and the
host-side app live in container-distro's [host-file-access]; the C
ABI + inotify event ops land here first.

[#7]: https://github.com/daphnediane/container-mount/issues/7
[host-file-access]: https://github.com/daphnediane/container-distro/blob/main/docs/design/host-file-access.md
