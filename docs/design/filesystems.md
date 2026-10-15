# Filesystem behavior notes

Observed host/guest filesystem behavior the implementation leans on —
the "why" behind the non-obvious parts of `server.rs` and `fscli.rs`.
Companion to [protocol.md](protocol.md), which defines the ops.
Measured on Linux/ext4 guests (Apple `container`) and macOS APFS
hosts; noted where something else could differ.

## Guest side — Linux

### Extent scanning (`Seek`)

`SEEK_DATA`/`SEEK_HOLE` on ext4 return the real extent map, aligned to
the fs block size; `ENXIO` means nothing but hole beyond the offset.
Filesystems that can't report extents fail the first `SEEK_DATA` and
callers fall back to treating the file as dense — content is still
correct, just denser and chattier.

### Server-side copy (`Copy`, `CopyRange`)

- `FICLONE`/`FICLONERANGE` ioctls share extents where the fs supports
  it (btrfs/xfs/bcachefs/overlayfs); ext4 answers `ENOTTY` and the
  server falls back. `COPY_REQUIRE_SHARED` never falls back — a caller
  asking for sharing gets `EOPNOTSUPP` instead of a silent copy.
- `copy_file_range` alone is *not* hole-aware: on a filesystem without
  reflinks it copies holes as real zeros, de-sparsifying the
  destination. So the fallback iterates `SEEK_DATA`/`SEEK_HOLE` itself
  and `copy_file_range`s only the data extents (`copy_extents`);
  filesystems that can't even report extents get the dense loop.
- `copy_file_range` goes through a raw `libc::syscall` — musl's
  wrapper and the kernel's emulation have disagreed historically, and
  the syscall answers don't depend on either.

### `O_PATH` fds are second-class

The server's node handles are all `O_PATH | O_NOFOLLOW`, which several
syscalls refuse:

- `f*xattr` need a real fd — the server reopens the node through
  `/proc/self/fd/N` with `O_RDONLY | O_NONBLOCK | O_NOCTTY` (`O_NONBLOCK`
  so a FIFO node can't hang the open). Symlink nodes refuse `EOPNOTSUPP`:
  reopening a proc path follows the link, which would escape the
  symlink confinement.
- `syncfs` likewise reopens through `/proc/self/fd`.
- `FsyncDir` instead opens `"."` under the node's dirfd — a real
  directory fd with no path lookup at all.

## Host side — APFS

### What works as expected

- `st_blocks` (512-byte units) reports real allocation, so
  `blocks * 512 < size` is a reliable sparse check — that's the gate
  `get` uses before probing extents.
- `SEEK_DATA`/`SEEK_HOLE` work on sparse files, so `put` scans local
  extents with the same extent-walk shape as the server.
- `ftruncate` extension creates real holes; a file truncated to 8 MiB
  reports 0 data blocks.

### The delayed-allocation trap

APFS densifies sparse files at flush time. When a *full, aligned*
filesystem block (4 KiB `f_bsize`) of dirty data is flushed on a file
whose size extends beyond the written region, the whole `[0, size)`
range ends up physically allocated — holes become real zeros on disk.

Bisected observations (APFS volume, `File::create` + `pwrite` +
`set_len` + close):

| Sequence                                        | Result               |
| ----------------------------------------------- | -------------------- |
| write 4 KiB → `set_len(8MiB)` → close           | dense (16384 blocks) |
| `set_len(8MiB)` → write 4 KiB → close           | dense                |
| write 4 KiB → `set_len` → `fsync` → close       | dense                |
| write 4 KiB → `fsync` → `set_len(8MiB)` → close | sparse (8 blocks)    |
| write 4095 bytes → `set_len(8MiB)` → close      | sparse               |
| `F_NOCACHE` on the fd                           | no change            |

So it's the flush of dirty full-block data against a large file size
that triggers it — flushing while the size is still small, or writing
only partial blocks, dodges it; `fsync` after extending doesn't help
(the fsync itself densifies), and `F_NOCACHE` doesn't either.

Two traps this sets for anyone debugging it: an `fstat` on the *open*
fd still reports the pre-flush block count, so the file looks sparse
until the moment you `stat` the path; and a `pwrite` at a large offset
densifies everything *below* the write too — the flush-time file size
is the allocation ceiling.

### The workaround: `F_PUNCHHOLE`

`fcntl(fd, F_PUNCHHOLE, &fpunchhole_t)` frees whole-block ranges and —
the part that matters — it wins over the flush-time allocation even
with dirty data still in flight. `get_sparse` therefore copies only
the guest's data extents, `set_len`s, then punches every hole range
before close. Verified both ways: an 8 MiB file with one 4 KiB extent
lands at 8 blocks, mid-file holes included.

Details that bite:

- `fpunchhole_t`/`F_PUNCHHOLE` aren't bound in the Rust `libc` crate —
  `punch_hole` declares the struct and command (99) itself.
- `fcntl` is variadic, and on arm64 Darwin variadic args always go on
  the stack — declaring it as a fixed-argument extern passes the
  pointer in a register the callee never reads (`EFAULT`). Use
  `libc::fcntl`.
- Ranges round inward to `f_bsize`; a sub-block scrap at an edge stays
  allocated zeros — still correct, just less sparse at the boundary.
- Best-effort: where punching isn't supported the file keeps correct
  content, just dense. Failure doesn't error the transfer.

## Mechanism map

| Need          | Linux guest                              | macOS host                                          |
| ------------- | ---------------------------------------- | --------------------------------------------------- |
| scan extents  | `Seek` op → `SEEK_DATA`/`SEEK_HOLE`      | `libc::lseek` same constants                        |
| create holes  | don't write; `SetAttr.size` for the tail | `set_len` **plus** `F_PUNCHHOLE`                    |
| punch a hole  | `Fallocate` `PUNCH_HOLE\|KEEP_SIZE`      | `fcntl(F_PUNCHHOLE)`                                |
| share extents | `FICLONE`/`FICLONERANGE` ioctl           | `clonefile` (unused — data crosses the wire anyway) |
| fsync a dir   | reopen `"."` under dirfd, `fsync`        | (FSKit front-end's job)                             |
| sync a fs     | `syncfs` on a `/proc/self/fd` reopen     | —                                                   |
| xattrs        | `f*xattr` on a `/proc/self/fd` reopen    | `getxattr`/`setxattr`/…                             |

## Verified by e2e

`tests/e2e.sh` stages an 8 MiB guest file with ~4 KiB of data and
asserts both directions stay under 8K 512-blocks (`sparse get`,
`sparse put`), plus a guest-visible `cp` round trip — on ext4 today,
which exercises the no-reflink fallback path.
