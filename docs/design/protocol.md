# Protocol and transport

The wire protocol, transports, and client-side validation distrofs
implements. Companion to [security.md](security.md) (threat model) and
[filesystems.md](filesystems.md) (observed host/guest fs behavior).
The deploying side of the integration — how container-distro starts
the server, picks socket paths, and supplies tokens — is in
container-distro's
[host-file-access](https://github.com/daphnediane/container-distro/blob/main/docs/design/host-file-access.md)
doc; nothing here depends on it.

## Transports

A session is one framed stream. Three ways to get one:

- **Unix socket** (`--socket PATH`) — the production shape: a relay
  (Apple `container`'s `--publish-socket`, an SSH forward, a socat
  pair) exposes the guest listener as a host-side path.
- **TCP** (`--tcp HOST:PORT` client, `serve --listen tcp:ADDR`) — for
  engines that can only publish TCP (docker/podman `-p`), and for
  development. Provides no peer identity; rely on the handshake.
- **Inherited fd** (`--fd N` / `serve --stdio`) — the caller passes an
  already-connected socket, e.g. an agent handing the module an
  `SCM_RIGHTS` fd, or `container exec -i` pipes.

## Connection hygiene (Unix paths)

macOS has no public `connectat`/`bindat`, no public per-thread
`fchdir`, and `/dev/fd/N/name` doesn't traverse directories, so
`connect(2)` necessarily resolves a path and follows symlinks. The
client defends in three layers (`src/connect.rs`):

1. **Directory:** open each component under the managed socket dir
   with `O_DIRECTORY | O_NOFOLLOW`; `fstat` must show our uid and mode
   `0700`, or we refuse. This also protects the relay's
   `unlink` + `bind` of the path, which follows directory components.
2. **Leaf:** `fstatat(dirfd, name, AT_SYMLINK_NOFOLLOW)` must show
   `S_ISSOCK` and our uid. Defence in depth only — it narrows the race
   window, it can't close it.
3. **Peer, after connect** — what makes the race irrelevant:
   `--peer-requirement <string>` is enforced against the connected
   peer's audit token via `SecCodeCheckValidity` (macOS only).
   container-distro passes the requirement pinning Apple's
   `container-runtime-linux` helper; a swapped path pointing at some
   other listener fails here. The shared runtime helper can't tell
   *which* guest answered — the handshake does, since only the server
   launched with this session's token can prove it.

Trust failures (directory, leaf, peer signature, handshake) are never
retried; "no server yet" failures (EOF, ECONNREFUSED) retry until
`--timeout` because relays accept the host connection before the guest
listener exists.

## Handshake (mutual)

```text
server → ServerHello { magic = "distrofs", version, server_nonce[32] }
client → ClientAuth  { version, client_nonce[32], mac_c = HMAC-SHA256(token, "distrofs/c" ‖ server_nonce) }
server → ServerAuth  { mac_s = HMAC-SHA256(token, "distrofs/s" ‖ client_nonce), caps, root attributes }
```

The client proves the token too: a guest-local process that reaches
the socket must not get a root file server. `mac_c` is checked in
constant time; a failure closes the connection without detail. The
token is 32 random bytes per server instance, supplied via
`DISTROFS_TOKEN`. Nonces stop replay across instances.

## Framing and messages

`u32` little-endian length + payload, max 16 MiB per frame (enforced
on both sides before allocation). Payloads are [postcard]-encoded
serde types from `src/proto.rs`; postcard's wire format is stable at
1.x and the crate is pure Rust. Enum variants encode by position —
append, never reorder.

`Request { id: u64, op: Op }` → `Response { id, result:
Result<Reply, Errno> }`. Requests are pipelined: the server runs them
on a small worker pool and replies out of order, since filesystem
front-ends (FSKit) issue concurrent operations. `id == 0` is reserved
for server-pushed `Event`s.

## Nodes and handles

Like FUSE's low-level API:

- `NodeId` (`u64`) is server-assigned on `Lookup`/`Create`/`Mkdir`/…
  and backed by an `O_PATH | O_NOFOLLOW` fd plus `(dev, ino)`. It
  carries a lookup count released by `Forget { node, n }`. Node 1 is
  `/`.
- `Lookup { parent, name }` takes a **single component**; names
  containing `/` or NUL, or equal to `.`/`..`, are rejected on both
  sides. Every resolution is `openat(parent_fd, name, O_NOFOLLOW…)`,
  so there is no path-traversal surface at all.
- `FileHandle` (`u64`) comes from `Open`/`Create` and is released by
  `Release`.

## Operations (v1)

`Lookup`, `Forget`, `GetAttr`, `SetAttr` (mode, uid/gid, size, times),
`ReadDir { node, cookie, max_entries }` returning entries *with*
attributes (stateless — the server reopens the directory and
`seekdir`s to the cookie), `Open`, `Read`, `Write`, `Fsync`,
`Release`, `Create`, `Mkdir`, `Unlink`, `Rmdir`, `Rename { flags }`
(`RENAME_NOREPLACE`/`EXCHANGE`), `Symlink`, `ReadLink`, `Link`,
`StatFs`. `Mknod` is deliberately absent.

**Optional ops** are gated by `ServerAuth.caps`; a v1 peer drops the
connection on an op it can't decode, so the client checks the bit
(`Client::need` → `EOPNOTSUPP`) before sending. Enum variants encode
by position — append, never reorder — and postcard can't extend a
struct once shipped, so a new field on `Attr`/`SetAttr`/`StatFs`/
`OpenFlags` means a *new* op or struct, not an edited one.

| cap             | ops                                                |
| --------------- | -------------------------------------------------- |
| `CAP_COPY`      | `Copy`, `CopyRange`                                |
| `CAP_SEEK`      | `Seek`                                             |
| `CAP_FALLOCATE` | `Fallocate`                                        |
| `CAP_DIR_SYNC`  | `FsyncDir`, `SyncFs`                               |
| `CAP_XATTR`     | `GetXattr`, `ListXattr`, `SetXattr`, `RemoveXattr` |
| `CAP_EVENTS`    | `Event` pushes (planned)                           |

**Copying.** `Copy` is whole-file (`clonefileat` semantics): an
exclusive create under `dst_parent` (`EEXIST` if taken), mode/times
from the source, owner is the provisioned user like every host-created
entry. `CopyRange` is `copy_file_range` between open handles. Ops are
named for the contract, not the mechanism: data is reflinked
(`FICLONE`/`FICLONERANGE`) where the filesystem supports it and copied
extent-by-extent otherwise — skipping holes, so a sparse source never
materializes as allocated zeros. `COPY_REQUIRE_SHARED` fails
`EOPNOTSUPP` instead of falling back, for callers that need sharing
itself. Replies report `shared` so a caller can see which happened.

**Sparse files.** `Seek { fh, offset, whence: Data|Hole }` is
`SEEK_DATA`/`SEEK_HOLE`: `ENXIO` means the rest of the file is a hole,
`EOPNOTSUPP` means the fs can't report extents (treat it as dense).
`Fallocate` takes the wire `FALLOC_*` mode bits (the server maps them
to `FALLOC_FL_*`): preallocate, `PUNCH_HOLE` — with `KEEP_SIZE` —
deallocates a range back to zeros, plus `ZERO_RANGE`,
`COLLAPSE`/`INSERT_RANGE`, `UNSHARE`. Detection needs no op:
`attr.blocks * 512 < attr.size`. `SetAttr.size` + positional `Write`
always covered hole *creation*; what's new is hole *discovery* and
*deallocation*. `get`/`put` already ride on this — a sparse file moves
only its extents and stays sparse on both ends. One host-side wrinkle:
APFS allocates the flushed gaps of an extended file rather than
leaving them sparse, so `get` additionally `F_PUNCHHOLE`s the hole
ranges before close ([filesystems.md](filesystems.md) has the full
measurements).

**Xattrs** (`CAP_XATTR`) map to `f*xattr` on a real fd reopened
through `/proc/self/fd` (`O_PATH` fds can't carry them; `O_NONBLOCK`
keeps a FIFO node from hanging). Names ≤ 255 bytes, values ≤ 64 KiB;
all namespaces pass through. Symlinks refuse `EOPNOTSUPP` — a
proc-path reopen would follow the link. `SetXattr` flags: 0 set,
1 create-only, 2 replace-only. Hidden mountpoints present as having no
xattrs. These exist chiefly for the FSKit front-end, where macOS
writes `com.apple.*` attrs on every Finder copy.

**Durability.** `FsyncDir` fsyncs a directory's own metadata so
create/rename dentries are durable; `SyncFs` is `syncfs` on the
filesystem hosting the node.

**Attributes.** `ino, kind, perm, nlink, uid, gid, rdev, size, blocks,
blksize, atime, mtime, ctime` from `fstat` on the node fd. `btime`
needs `statx` — the musl `libc` bindings don't expose the struct, so
it goes through `libc::syscall` when it lands; a later capability.

**Errors.** The wire carries Linux errno *values* as a documented
enum. The host maps them to Darwin numbers, which differ (`ENOTEMPTY`
39 vs 66, `ELOOP` 40 vs 62, …); unknown values become `EIO`.

**Events (planned).** The server places inotify watches on
directories the client has read and files it has open, and pushes
`Event::Invalidate { node, what: Attr | Data | Entries | Gone }`. On
inotify exhaustion (`max_user_watches`) it says so once and the host
falls back to short TTL caching.

**For the FSKit front-end.** The optional ops exist to serve macOS:
`clonefile`/Finder copy → `Copy`, `SEEK_DATA`/`SEEK_HOLE` and
hole-aware reads → `Seek`, `F_ALLOCATE*`/`fpunchhole` fcntls →
`Fallocate`, `com.apple.*` xattrs (quarantine, FinderInfo,
ResourceFork) → the xattr ops, directory fsync → `FsyncDir`. Still
deferred, as further capabilities: `btime` (`statx`), BSD file flags
(`FS_IOC_*FLAGS` ↔ chflags — `UF_IMMUTABLE`/`APPEND` map naturally,
`HIDDEN` doesn't), POSIX-ACL ↔ NFSv4-ACL mapping, and advisory locks
if FSKit surfaces them.

## Guest server behavior

**Identity and permissions.** The server runs as guest root so it can
present the whole rootfs. It reports real ownership and modes; the
*host kernel* enforces them against the calling user. Files and
directories created from the host are `fchown`ed to the provisioned
user when the caller supplies one.

**Mount crossing.** The server runs in the container's mount
namespace, so it also sees virtiofs shares and pseudo filesystems. It
reads `/proc/self/mountinfo` and serves only local filesystem types
(`ext4`, `xfs`, `btrfs`, `tmpfs`, `overlay`). Mountpoints of anything
else — `virtiofs`, `proc`, `sysfs`, `devtmpfs`, `devpts`, `cgroup2`,
`fuse` and friends — appear as empty, read-only directories. That
avoids serving host directories back to the host (a loop indexers
would happily walk) and keeps `/proc` out of view.

**Special files.** Devices, FIFOs and sockets are listed with their
real type but `open` fails with `EOPNOTSUPP`; mounts should also be
`nodev,nosuid`.

**Lifetime.** The server creates its socket directory `0700` root
(guest users must not reach a root server's socket), unlinks any stale
socket, binds, and listens. **Last starter wins:** a new instance
unlinks and re-binds the path — new connections reach the new instance
while an older one keeps serving the clients it already has and exits
when they're gone. A client that loses the race fails the handshake
(wrong token) and retries. Idle exit: no client within 30 s of start,
or 30 s after the last client disconnects.

## What the host client enforces (the trusted side)

The server is guest code: after guest root it is whatever the guest
wants it to be. `src/client.rs` + `src/symlink.rs` are the single
place that treats its replies as hostile input:

- frame size cap; per-request timeouts; bounded outstanding requests;
- names in directory entries: no `/`, no NUL, not `.`/`..`, bounded
  length (255 bytes); otherwise the entry is dropped;
- attribute sanity: kind matches mode, sizes non-negative,
  setuid/setgid/sticky *reported* but neutralized by the `nosuid`
  mount;
- **symlink targets** — the classic escape. macOS VFS resolves
  symlinks client-side, so a guest link `/etc/x -> /Users/me/.ssh`
  would point into the *host*, and `../../..` beyond the mount root
  would climb out of it. The client knows each node's depth from the
  lookup chain and rewrites every target with chroot semantics:
  resolve it inside the guest root (absolute ⇒ from the root, `..` at
  the root stays at the root), then emit a relative path from the
  link's directory that never leaves the volume. Targets that can't
  be resolved lexically are returned as-is only if they are relative
  and can't climb above the root; otherwise they are replaced with a
  dangling in-volume target. Client-created symlinks are stored
  verbatim.

A future FSKit module links this code through a C ABI, so CLI tools
and Finder get identical enforcement.

## Version stamp

Every `distrofs` build embeds a scannable stamp —
`distrofs-version:<version>:protocol:<n>` — so server discovery can
reject a stale or misplaced binary before launching it rather than
surfacing a failed exec or a hung handshake.

[postcard]: https://docs.rs/postcard
