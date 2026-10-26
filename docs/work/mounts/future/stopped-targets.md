# Stopped targets: helper VM and lifecycle

- **Issue:** [#21]
- **Milestone:** none (future)

Mounting stopped containers, machines, distros, and images through a
helper VM — and the undecided part: how mounts behave when targets start
and stop, the container-distro hand-off, detection for targets the
plugin doesn't control, writes to upstream-owned disks, staleness, and
helper VM cleanup. Concerns and open decisions are listed in
[mount-backends] "Lifecycle".

[#21]: https://github.com/daphnediane/container-mount/issues/21
[mount-backends]: ../../../design/mount-backends.md
