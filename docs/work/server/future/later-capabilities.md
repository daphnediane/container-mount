# Later capabilities

- **Issue:** [#9]
- **Milestone:** none (future)

`btime` (`statx` via `libc::syscall`; the musl `libc` bindings don't
expose the struct), BSD file flags (`FS_IOC_*FLAGS` ↔ chflags;
`HIDDEN` has no Linux analogue), ACL mapping (POSIX ↔ NFSv4),
advisory locks if FSKit surfaces them, batch `Lookup` for deep paths.
`Mknod` stays deliberately absent.

[#9]: https://github.com/daphnediane/container-mount/issues/9
