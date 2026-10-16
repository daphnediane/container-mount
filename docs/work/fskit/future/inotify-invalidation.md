# inotify events → cache invalidation

- **Issue:** [#8]
- **Milestone:** none (future)

Server places watches on read directories/open files and pushes
`Event::Invalidate` ([protocol]); designed for macOS 27's
`FSVolume.DataCacheHandler`.

[#8]: https://github.com/daphnediane/distrofs/issues/8
[protocol]: ../../../design/protocol.md
