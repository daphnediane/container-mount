# Serving beyond Linux, and below the root

`distrofs serve` today exists only for Linux and always serves `/`.
Two relaxations, analyzed here rather than scheduled: a `--root DIR`
flag (serve a subtree — trivial) and ports to macOS/BSD/Windows
(substantial; the nested-VM use cases that motivate the
per-triple layout in [install-layout.md](install-layout.md)).

## `serve --root DIR`

The served root is a single line: `Conn::new` opens
`openat(AT_FDCWD, "/", O_PATH|O_DIRECTORY)`. `--root` points that fd
at `DIR` instead. Everything downstream is already `*at()`-relative
to node fds, so confinement is structural rather than checked — a
client cannot name a path at all, let alone one outside the root fd:

- mount-hiding still works: it keys on `st_dev` transitions, not
  paths, so a bind-mount pulled in from outside the root shows
  exactly as hidden/visible as its filesystem warrants;
- `--uid`/`--gid` chown and `StatFs` apply to the root's filesystem
  as-is;
- the client's symlink confinement already treats the served root as
  the boundary — a subtree root is indistinguishable from `/` to it.

Uses: embedders that want to expose only a volume (`--root
/mnt/shared` instead of the whole rootfs), a second server instance
with a different provisioned uid, and — once the macOS port exists —
serving a host directory for testing with no container or VM in the
loop at all.

## Porting the server

The Linux server is ~1,300 lines of `*at()` syscalls; the wire
protocol is platform-neutral in shape. The work splits into seams,
in roughly descending order of porting pain:

### Node-fd model — the hard one

Confinement depends on pinning inodes with fds and never resolving
paths.

| platform | node-fd strategy                                                                                                                                                                                                                                                                                                                                          |
| -------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Linux    | `O_PATH\|O_NOFOLLOW` — today's code                                                                                                                                                                                                                                                                                                                       |
| macOS    | No `O_PATH`. `O_EVTONLY` fds can't carry `fstat`/`fchdir`-family ops, so nodes become real `O_RDONLY\|O_NOFOLLOW\|O_NONBLOCK` fds with fstat verification — `O_NONBLOCK` keeps FIFO nodes from hanging on open, and directory nodes need `O_DIRECTORY`. Workable, but every op site must be audited for fd-type assumptions the Linux code gets for free. |
| FreeBSD  | `O_PATH` exists (13+) — nearest to Linux.                                                                                                                                                                                                                                                                                                                 |
| Windows  | No `openat`. The honest equivalent is `NtCreateFile` with a `RootDirectory` handle plus `FILE_OPEN_REPARSE_POINT`; Win32 path+verify is TOCTOU-prone and should not be the confinement primitive. Largest single piece of the port — plausibly a sibling implementation sharing `proto` rather than `cfg`s inside `server.rs`.                            |

### Mount table and hidden mountpoints

- macOS: `getfsstat(2)` + `statfs.f_fstypename` replaces
  `/proc/self/mountinfo`; the served-fstype table keeps its meaning
  (apfs/hfs local; osxfuse/smbfs/nfs hidden).
- FreeBSD: `getvfsstat(2)`, same table shape.
- Windows: no unified mount tree — junctions are reparse points and
  volumes are separate roots. Likely serve-one-volume semantics with
  a reparse-point policy decided alongside the client's symlink
  handling.

### Errno translation — a protocol consequence

The wire `Errno` enum is **Linux-numbered**, and the client maps
Linux → host. Non-Linux servers must therefore translate native
errnos *to Linux wire values* so the client keeps a single table.
That is cheaper than negotiating an errno space in the handshake —
one translation table per server platform versus a second error
vocabulary on every op — but it must be written as a documented
requirement, since e.g. macOS `ENOTEMPTY`-for-nonempty-dirs and
`ELOOP` thresholds have Linux equivalents with different numbers,
and Windows needs a full NTSTATUS→Linux mapping.

### Identity and ownership

`uid`/`gid` are POSIX: macOS and FreeBSD map directly, including
`--uid`/`--gid` chown of host-created entries. Windows has SIDs —
options are a synthetic RID→uid mapping or `0:0` reporting with
chown as a no-op. A first Windows server likely serves read-mostly
and defers ownership semantics.

### Per-op availability — caps already gate this

| op               | macOS                                                                                                                              | FreeBSD                 | Windows                                                  |
| ---------------- | ---------------------------------------------------------------------------------------------------------------------------------- | ----------------------- | -------------------------------------------------------- |
| `Copy`           | `clonefile(2)`                                                                                                                     | `copy_file_range` (13+) | `CopyFileExW` / `FSCTL_DUPLICATE_EXTENTS_TO_FILE` (ReFS) |
| `CopyRange`      | `fcopyfile(3)`/copyfile                                                                                                            | same                    | plain read/write fallback                                |
| `Seek`           | **absent** — no `SEEK_DATA`/`HOLE`; `F_LOG2PHYS_EXT` is per-extent and awkward → cap off, client treats files dense                | present                 | `FSCTL_QUERY_ALLOCATED_RANGES` on sparse files           |
| `Fallocate`      | `fcntl(F_ALLOCATE*)`                                                                                                               | `posix_fallocate`       | `SetEndOfFile`/`SetFileValidData`                        |
| xattrs           | `f*xattr` — flat names, no `user.`/`trusted.` namespaces; wire-name mapping needs a decision (namespace-collision risk documented) | `extattr_*`             | ADS or `FILE_EA_*` — mapping TBD                         |
| `Rename` flags   | plain `renameat`; `RENAME_*` caps off                                                                                              | same                    | `MoveFileEx` flags, partial                              |
| `btime` (future) | **easier than Linux** — `getattrlist(ATTR_CMN_CRTIME)`, no statx dance                                                             | `st_birthtim`           | creation time, native                                    |

### Transport

- Unix sockets: fine on macOS/BSD — we bind paths, no abstract
  namespace needed. Windows: `AF_UNIX` exists since Win10 RS4; TCP
  and named pipes are the fallbacks.
- The `--stdio` transport already on the work list
  ([item](../work/server/future/stdio-transport.md)) matters *more*
  here: exec-pipe relays are the common way to reach a Windows guest.

### Symlinks

Client-side confinement needs no port — it's lexical policy on the
trusted side, identical regardless of server OS. Server-side
`Symlink`/`ReadLink` do differ: Windows requires
`SeCreateSymbolicLinkPrivilege` and distinguishes file-vs-dir links;
macOS/BSD match Linux semantics.

## Ordering

1. **`serve --root DIR`** — trivial on Linux; do any time.
2. **macOS** — moderate: same syscall family, an errno table, the
   `O_PATH` workaround. Buys the true container-free test loop and
   exercises the same enforcement code the FSKit front-end will
   eventually front.
3. **FreeBSD** — similar shape after macOS; low demand.
4. **Windows** — largest; new handle model, identity mapping, mount
   model. Decide sibling-crate-vs-cfg when it gets scheduled.
