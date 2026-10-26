/*
 * Copyright (c) 2026 Daphne Pfister
 * SPDX-License-Identifier: BSD-2-Clause
 * See LICENSE file for full license text
 */

//! The host-side protocol client — the trusted side.
//!
//! The server is guest code, so everything it sends is hostile input:
//! frames are size-capped before allocation (in [`proto`]), calls time
//! out, in-flight requests are bounded, directory entry names are
//! validated, symlink targets are bounded here and confined by the
//! path layer ([`Client::resolve`], [`crate::symlink`]), and unexpected
//! replies are protocol errors rather than panics.

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::Duration;

use crate::connect::Stream;
use crate::proto::*;
use crate::symlink;

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const CALL_TIMEOUT: Duration = Duration::from_secs(60);
/// Bound on requests awaiting a reply.
const MAX_INFLIGHT: usize = 256;
/// Bound on one directory listing, against a server that never says EOF.
const MAX_LISTING: usize = 1 << 20;
/// Symlink hops while resolving one path (Linux `MAXSYMLINKS`).
const MAX_HOPS: usize = 40;

#[derive(Debug)]
pub enum Error {
    /// The guest operation failed with this (Linux) errno.
    Guest(Errno),
    Io(io::Error),
    /// The peer broke the protocol or failed authentication.
    Protocol(String),
    Timeout,
    Closed,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Guest(e) => write!(f, "{}", io::Error::from_raw_os_error(e.to_darwin())),
            Error::Io(e) => write!(f, "container-mount connection: {e}"),
            Error::Protocol(m) => write!(f, "container-mount protocol error: {m}"),
            Error::Timeout => write!(f, "container-mount request timed out"),
            Error::Closed => write!(f, "container-mount connection closed"),
        }
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error::Io(e)
    }
}

impl Error {
    /// The Darwin errno this error should surface as.
    #[must_use]
    pub fn darwin_errno(&self) -> i32 {
        match self {
            Error::Guest(e) => e.to_darwin(),
            Error::Io(e) => e.raw_os_error().unwrap_or(libc::EIO),
            Error::Timeout => libc::ETIMEDOUT,
            Error::Protocol(_) | Error::Closed => libc::EIO,
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;

fn protocol(msg: impl Into<String>) -> Error {
    Error::Protocol(msg.into())
}

fn unexpected(reply: &Reply) -> Error {
    protocol(format!("unexpected reply {}", reply_name(reply)))
}

fn reply_name(r: &Reply) -> &'static str {
    match r {
        Reply::Ok => "Ok",
        Reply::Entry { .. } => "Entry",
        Reply::Entries { .. } => "Entries",
        Reply::Attr(_) => "Attr",
        Reply::Opened { .. } => "Opened",
        Reply::Data(_) => "Data",
        Reply::Written(_) => "Written",
        Reply::Created { .. } => "Created",
        Reply::Target(_) => "Target",
        Reply::StatFs(_) => "StatFs",
        Reply::Offset(_) => "Offset",
        Reply::Copied { .. } => "Copied",
        Reply::CopiedRange { .. } => "CopiedRange",
        Reply::Value(_) => "Value",
        Reply::Names(_) => "Names",
    }
}

fn check_name(name: &[u8]) -> Result<()> {
    if valid_name(name) {
        Ok(())
    } else {
        Err(Error::Guest(if name.len() > NAME_MAX {
            Errno::ENAMETOOLONG
        } else {
            Errno::EINVAL
        }))
    }
}

type Waiter = mpsc::Sender<std::result::Result<Reply, Errno>>;

#[derive(Default)]
struct Pending {
    waiters: HashMap<u64, Waiter>,
    closed: bool,
}

/// A path resolved from the guest root.
#[derive(Debug, Clone, PartialEq)]
pub struct Resolved {
    pub node: NodeId,
    pub attr: Attr,
    /// The node's components from the root (empty for the root).
    pub path: Vec<Vec<u8>>,
}

/// An authenticated container-mount session. Thread-safe: requests from any
/// thread are pipelined over the one connection.
pub struct Client {
    stream: Stream,
    writer: Mutex<Stream>,
    pending: Arc<Mutex<Pending>>,
    next_id: AtomicU64,
    root: Attr,
    caps: u64,
}

impl Client {
    /// Authenticate over a connected `stream` with the session `token`
    /// (mutual: each side proves it knows the token over the other's
    /// nonce), then start the reply reader.
    pub fn handshake(mut stream: Stream, token: &[u8]) -> Result<Client> {
        stream.set_read_timeout(Some(HANDSHAKE_TIMEOUT))?;
        let hello: ServerHello = read_frame(&mut stream).map_err(|e| match e.kind() {
            io::ErrorKind::InvalidData => protocol("peer is not a container-mount server"),
            _ => Error::Io(e),
        })?;
        if hello.magic != MAGIC {
            return Err(protocol("peer is not a container-mount server"));
        }
        if hello.version != VERSION {
            return Err(protocol(format!(
                "server speaks protocol {}, this build speaks {VERSION}",
                hello.version
            )));
        }
        let nonce = random32()?;
        write_frame(
            &mut stream,
            &ClientAuth {
                version: VERSION,
                nonce,
                mac: client_mac(token, &hello.nonce),
            },
        )?;
        let auth: ServerAuth = read_frame(&mut stream).map_err(|e| match e.kind() {
            io::ErrorKind::UnexpectedEof => protocol("server rejected the session token"),
            _ => Error::Io(e),
        })?;
        if !ct_eq(&auth.mac, &server_mac(token, &nonce)) {
            return Err(protocol("server failed to prove the session token"));
        }
        stream.set_read_timeout(None)?;

        let pending = Arc::new(Mutex::new(Pending::default()));
        let mut reader = stream.try_clone()?;
        let p = Arc::clone(&pending);
        thread::spawn(move || {
            while let Ok(msg) = read_frame::<_, ServerMsg>(&mut reader) {
                match msg {
                    ServerMsg::Response(r) => {
                        let waiter = p.lock().ok().and_then(|mut p| p.waiters.remove(&r.id));
                        // Unknown ids (late replies to timed-out calls,
                        // or a confused server) are dropped.
                        if let Some(w) = waiter {
                            let _ = w.send(r.result);
                        }
                    }
                    ServerMsg::Event(_) => {}
                }
            }
            if let Ok(mut p) = p.lock() {
                p.closed = true;
                p.waiters.clear();
            }
        });
        Ok(Client {
            writer: Mutex::new(stream.try_clone()?),
            stream,
            pending,
            next_id: AtomicU64::new(1),
            root: auth.root,
            caps: auth.caps,
        })
    }

    #[must_use]
    pub fn root_attr(&self) -> &Attr {
        &self.root
    }

    /// The optional-op bits the server advertised ([`CAP_*`] constants).
    #[must_use]
    pub fn caps(&self) -> u64 {
        self.caps
    }

    /// `EOPNOTSUPP` without a round trip when the server lacks `bit` —
    /// a v1 peer would drop the connection on an op it can't decode.
    fn need(&self, bit: u64) -> Result<()> {
        if self.caps & bit != 0 {
            Ok(())
        } else {
            Err(Error::Guest(Errno::EOPNOTSUPP))
        }
    }

    /// Send one operation and wait for its reply.
    pub fn call(&self, op: Op) -> Result<Reply> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::channel();
        {
            let mut p = self.pending.lock().map_err(|_| Error::Closed)?;
            if p.closed {
                return Err(Error::Closed);
            }
            if p.waiters.len() >= MAX_INFLIGHT {
                return Err(protocol("too many requests in flight"));
            }
            p.waiters.insert(id, tx);
        }
        let sent = self
            .writer
            .lock()
            .map_err(|_| Error::Closed)
            .and_then(|mut w| write_frame(&mut *w, &Request { id, op }).map_err(Error::Io));
        if let Err(e) = sent {
            self.forget_waiter(id);
            return Err(e);
        }
        match rx.recv_timeout(CALL_TIMEOUT) {
            Ok(Ok(r)) => Ok(r),
            Ok(Err(e)) => Err(Error::Guest(e)),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                self.forget_waiter(id);
                Err(Error::Timeout)
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(Error::Closed),
        }
    }

    fn forget_waiter(&self, id: u64) {
        if let Ok(mut p) = self.pending.lock() {
            p.waiters.remove(&id);
        }
    }

    pub fn lookup(&self, parent: NodeId, name: &[u8]) -> Result<(NodeId, Attr)> {
        check_name(name)?;
        match self.call(Op::Lookup {
            parent,
            name: name.to_vec(),
        })? {
            Reply::Entry { node, attr } => Ok((node, attr)),
            r => Err(unexpected(&r)),
        }
    }

    pub fn forget(&self, node: NodeId, n: u64) -> Result<()> {
        self.call(Op::Forget { node, n }).map(drop)
    }

    pub fn getattr(&self, node: NodeId) -> Result<Attr> {
        match self.call(Op::GetAttr { node })? {
            Reply::Attr(a) => Ok(a),
            r => Err(unexpected(&r)),
        }
    }

    pub fn setattr(&self, node: NodeId, set: SetAttr) -> Result<Attr> {
        match self.call(Op::SetAttr { node, set })? {
            Reply::Attr(a) => Ok(a),
            r => Err(unexpected(&r)),
        }
    }

    /// One page of entries after `cookie`, with invalid names dropped.
    pub fn readdir(
        &self,
        node: NodeId,
        cookie: u64,
        max_entries: u32,
    ) -> Result<(Vec<DirEntry>, bool)> {
        match self.call(Op::ReadDir {
            node,
            cookie,
            max_entries,
        })? {
            Reply::Entries { mut entries, eof } => {
                entries.retain(|e| valid_name(&e.name));
                Ok((entries, eof))
            }
            r => Err(unexpected(&r)),
        }
    }

    /// Every entry of a directory, paging until EOF.
    pub fn readdir_all(&self, node: NodeId) -> Result<Vec<DirEntry>> {
        let mut all = Vec::new();
        let mut cookie = 0;
        loop {
            let (page, eof) = self.readdir(node, cookie, 512)?;
            match page.last() {
                Some(last) if last.cookie != cookie => cookie = last.cookie,
                Some(_) => return Err(protocol("directory cookie did not advance")),
                None if !eof => return Err(protocol("empty directory page before EOF")),
                None => {}
            }
            all.extend(page);
            if all.len() > MAX_LISTING {
                return Err(protocol("directory listing too large"));
            }
            if eof {
                return Ok(all);
            }
        }
    }

    pub fn open(&self, node: NodeId, flags: OpenFlags) -> Result<FileHandle> {
        match self.call(Op::Open { node, flags })? {
            Reply::Opened { fh } => Ok(fh),
            r => Err(unexpected(&r)),
        }
    }

    pub fn read(&self, fh: FileHandle, offset: u64, len: u32) -> Result<Vec<u8>> {
        match self.call(Op::Read {
            fh,
            offset,
            len: len.min(MAX_IO),
        })? {
            Reply::Data(d) if d.len() <= len.min(MAX_IO) as usize => Ok(d),
            Reply::Data(_) => Err(protocol("read returned more than requested")),
            r => Err(unexpected(&r)),
        }
    }

    pub fn write(&self, fh: FileHandle, offset: u64, data: &[u8]) -> Result<u32> {
        if data.len() > MAX_IO as usize {
            return Err(Error::Guest(Errno::EINVAL));
        }
        match self.call(Op::Write {
            fh,
            offset,
            data: data.to_vec(),
        })? {
            Reply::Written(n) if n as usize <= data.len() => Ok(n),
            Reply::Written(_) => Err(protocol("write reported more than sent")),
            r => Err(unexpected(&r)),
        }
    }

    pub fn fsync(&self, fh: FileHandle) -> Result<()> {
        self.call(Op::Fsync { fh }).map(drop)
    }

    pub fn release(&self, fh: FileHandle) -> Result<()> {
        self.call(Op::Release { fh }).map(drop)
    }

    pub fn create(
        &self,
        parent: NodeId,
        name: &[u8],
        perm: u32,
        exclusive: bool,
        flags: OpenFlags,
    ) -> Result<(NodeId, Attr, FileHandle)> {
        check_name(name)?;
        match self.call(Op::Create {
            parent,
            name: name.to_vec(),
            perm,
            exclusive,
            flags,
        })? {
            Reply::Created { node, attr, fh } => Ok((node, attr, fh)),
            r => Err(unexpected(&r)),
        }
    }

    pub fn mkdir(&self, parent: NodeId, name: &[u8], perm: u32) -> Result<(NodeId, Attr)> {
        check_name(name)?;
        match self.call(Op::Mkdir {
            parent,
            name: name.to_vec(),
            perm,
        })? {
            Reply::Entry { node, attr } => Ok((node, attr)),
            r => Err(unexpected(&r)),
        }
    }

    pub fn unlink(&self, parent: NodeId, name: &[u8]) -> Result<()> {
        check_name(name)?;
        self.call(Op::Unlink {
            parent,
            name: name.to_vec(),
        })
        .map(drop)
    }

    pub fn rmdir(&self, parent: NodeId, name: &[u8]) -> Result<()> {
        check_name(name)?;
        self.call(Op::Rmdir {
            parent,
            name: name.to_vec(),
        })
        .map(drop)
    }

    pub fn rename(
        &self,
        parent: NodeId,
        name: &[u8],
        new_parent: NodeId,
        new_name: &[u8],
        flags: u32,
    ) -> Result<()> {
        check_name(name)?;
        check_name(new_name)?;
        self.call(Op::Rename {
            parent,
            name: name.to_vec(),
            new_parent,
            new_name: new_name.to_vec(),
            flags,
        })
        .map(drop)
    }

    /// Create a symlink; the target is stored verbatim (it's the host
    /// user's own choice).
    pub fn symlink(&self, parent: NodeId, name: &[u8], target: &[u8]) -> Result<(NodeId, Attr)> {
        check_name(name)?;
        match self.call(Op::Symlink {
            parent,
            name: name.to_vec(),
            target: target.to_vec(),
        })? {
            Reply::Entry { node, attr } => Ok((node, attr)),
            r => Err(unexpected(&r)),
        }
    }

    /// The *raw* guest target, bounded and NUL-free. Present it to the
    /// host only through [`symlink::confine`] (as [`Client::readlink_confined`] does).
    pub fn readlink_raw(&self, node: NodeId) -> Result<Vec<u8>> {
        match self.call(Op::ReadLink { node })? {
            Reply::Target(t) if !t.is_empty() && t.len() <= TARGET_MAX && !t.contains(&0) => Ok(t),
            Reply::Target(_) => Err(protocol("malformed symlink target")),
            r => Err(unexpected(&r)),
        }
    }

    /// The target of a link at `link`, rewritten to stay in the volume.
    pub fn readlink_confined(&self, link: &Resolved) -> Result<Vec<u8>> {
        let raw = self.readlink_raw(link.node)?;
        let dir: Vec<&[u8]> = link
            .path
            .iter()
            .take(link.path.len().saturating_sub(1))
            .map(Vec::as_slice)
            .collect();
        Ok(symlink::confine(&dir, &raw))
    }

    pub fn link(
        &self,
        node: NodeId,
        new_parent: NodeId,
        new_name: &[u8],
    ) -> Result<(NodeId, Attr)> {
        check_name(new_name)?;
        match self.call(Op::Link {
            node,
            new_parent,
            new_name: new_name.to_vec(),
        })? {
            Reply::Entry { node, attr } => Ok((node, attr)),
            r => Err(unexpected(&r)),
        }
    }

    pub fn statfs(&self, node: NodeId) -> Result<StatFs> {
        match self.call(Op::StatFs { node })? {
            Reply::StatFs(s) => Ok(s),
            r => Err(unexpected(&r)),
        }
    }

    /// Server-side copy of `src` to a new `dst_name` under `dst_parent`
    /// (exclusive). Returns the new node, its attributes, and whether
    /// the extents are shared. See [`Op::Copy`].
    pub fn copy(
        &self,
        src: NodeId,
        dst_parent: NodeId,
        dst_name: &[u8],
        flags: u32,
    ) -> Result<(NodeId, Attr, bool)> {
        check_name(dst_name)?;
        self.need(CAP_COPY)?;
        match self.call(Op::Copy {
            src,
            dst_parent,
            dst_name: dst_name.to_vec(),
            flags,
        })? {
            Reply::Copied { node, attr, shared } => Ok((node, attr, shared)),
            r => Err(unexpected(&r)),
        }
    }

    /// `copy_file_range` between open handles; `(bytes, shared)`. See
    /// [`Op::CopyRange`].
    pub fn copy_range(
        &self,
        src_fh: FileHandle,
        src_off: u64,
        dst_fh: FileHandle,
        dst_off: u64,
        len: u64,
        flags: u32,
    ) -> Result<(u64, bool)> {
        self.need(CAP_COPY)?;
        match self.call(Op::CopyRange {
            src_fh,
            src_off,
            dst_fh,
            dst_off,
            len,
            flags,
        })? {
            Reply::CopiedRange { n, shared } => Ok((n, shared)),
            r => Err(unexpected(&r)),
        }
    }

    /// The next data/hole boundary on an open file — a guest
    /// `ENXIO`/`EOPNOTSUPP` surfaces as [`Error::Guest`]. See [`Op::Seek`].
    pub fn seek(&self, fh: FileHandle, offset: u64, whence: SeekWhence) -> Result<u64> {
        self.need(CAP_SEEK)?;
        match self.call(Op::Seek { fh, offset, whence })? {
            Reply::Offset(o) => Ok(o),
            r => Err(unexpected(&r)),
        }
    }

    /// `fallocate(2)` on an open file — `FALLOC_*` mode bits.
    pub fn fallocate(&self, fh: FileHandle, mode: u32, offset: u64, len: u64) -> Result<()> {
        self.need(CAP_FALLOCATE)?;
        self.call(Op::Fallocate {
            fh,
            mode,
            offset,
            len,
        })
        .map(drop)
    }

    /// fsync a directory's own metadata.
    pub fn fsync_dir(&self, node: NodeId) -> Result<()> {
        self.need(CAP_DIR_SYNC)?;
        self.call(Op::FsyncDir { node }).map(drop)
    }

    /// Flush the filesystem hosting `node`.
    pub fn syncfs(&self, node: NodeId) -> Result<()> {
        self.need(CAP_DIR_SYNC)?;
        self.call(Op::SyncFs { node }).map(drop)
    }

    fn xattr_name(name: &[u8]) -> Result<()> {
        if name.is_empty() || name.len() > XATTR_NAME_MAX || name.contains(&0) {
            return Err(Error::Guest(if name.len() > XATTR_NAME_MAX {
                Errno::ENAMETOOLONG
            } else {
                Errno::EINVAL
            }));
        }
        Ok(())
    }

    /// One xattr's value (`Errno::ENODATA` = 61 surfaces via
    /// [`Error::Guest`] when unset).
    pub fn getxattr(&self, node: NodeId, name: &[u8]) -> Result<Vec<u8>> {
        Self::xattr_name(name)?;
        self.need(CAP_XATTR)?;
        match self.call(Op::GetXattr {
            node,
            name: name.to_vec(),
        })? {
            Reply::Value(v) if v.len() <= XATTR_VALUE_MAX => Ok(v),
            Reply::Value(_) => Err(protocol("xattr value too large")),
            r => Err(unexpected(&r)),
        }
    }

    /// Every xattr name on the node.
    pub fn listxattr(&self, node: NodeId) -> Result<Vec<Vec<u8>>> {
        self.need(CAP_XATTR)?;
        match self.call(Op::ListXattr { node })? {
            Reply::Names(ns) => {
                for n in &ns {
                    Self::xattr_name(n)?;
                }
                Ok(ns)
            }
            r => Err(unexpected(&r)),
        }
    }

    /// Set an xattr — `flags`: 0 set, 1 create-only, 2 replace-only.
    pub fn setxattr(&self, node: NodeId, name: &[u8], value: &[u8], flags: u32) -> Result<()> {
        Self::xattr_name(name)?;
        if value.len() > XATTR_VALUE_MAX || flags > 2 {
            return Err(Error::Guest(Errno::EINVAL));
        }
        self.need(CAP_XATTR)?;
        self.call(Op::SetXattr {
            node,
            name: name.to_vec(),
            value: value.to_vec(),
            flags,
        })
        .map(drop)
    }

    pub fn removexattr(&self, node: NodeId, name: &[u8]) -> Result<()> {
        Self::xattr_name(name)?;
        self.need(CAP_XATTR)?;
        self.call(Op::RemoveXattr {
            node,
            name: name.to_vec(),
        })
        .map(drop)
    }

    /// Resolve a guest path from the root, component by component.
    /// Intermediate symlinks are followed through [`symlink::confine`]
    /// (so resolution can never leave the guest root); the last one only
    /// when `follow_last`. `..` is lexical.
    pub fn resolve(&self, path: &[u8], follow_last: bool) -> Result<Resolved> {
        let mut stack: Vec<(NodeId, Attr, Vec<u8>)> = Vec::new();
        let mut queue: VecDeque<Vec<u8>> = split(path);
        let mut hops = 0;
        while let Some(comp) = queue.pop_front() {
            if comp == b".." {
                stack.pop();
                continue;
            }
            let parent = stack.last().map_or(ROOT, |(n, _, _)| *n);
            let (node, attr) = self.lookup(parent, &comp)?;
            if attr.kind == FileKind::Symlink && (!queue.is_empty() || follow_last) {
                hops += 1;
                if hops > MAX_HOPS {
                    return Err(Error::Guest(Errno::ELOOP));
                }
                let raw = self.readlink_raw(node)?;
                let dir: Vec<&[u8]> = stack.iter().map(|(_, _, n)| n.as_slice()).collect();
                let rel = symlink::confine(&dir, &raw);
                for c in split(&rel).into_iter().rev() {
                    queue.push_front(c);
                }
                continue;
            }
            if attr.kind != FileKind::Dir && !queue.is_empty() {
                return Err(Error::Guest(Errno::ENOTDIR));
            }
            stack.push((node, attr, comp));
        }
        Ok(match stack.pop() {
            Some((node, attr, name)) => {
                let mut path: Vec<Vec<u8>> = stack.into_iter().map(|(_, _, n)| n).collect();
                path.push(name);
                Resolved { node, attr, path }
            }
            None => Resolved {
                node: ROOT,
                attr: self.root.clone(),
                path: Vec::new(),
            },
        })
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        // Ends the reader thread; the server releases this session's nodes.
        let _ = self.stream.shutdown();
    }
}

/// Path components, skipping empty and `.` ones.
fn split(path: &[u8]) -> VecDeque<Vec<u8>> {
    path.split(|&b| b == b'/')
        .filter(|c| !c.is_empty() && *c != b".")
        .map(<[u8]>::to_vec)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::os::unix::net::UnixStream;

    const TOKEN: [u8; 32] = [9; 32];

    fn attr(ino: u64, kind: FileKind) -> Attr {
        Attr {
            ino,
            kind,
            perm: 0o755,
            nlink: 1,
            uid: 0,
            gid: 0,
            rdev: 0,
            size: 0,
            blocks: 0,
            blksize: 4096,
            atime: Time::default(),
            mtime: Time::default(),
            ctime: Time::default(),
        }
    }

    /// A tiny in-memory guest: `/etc/passwd`, `/bin -> usr/bin`,
    /// `/usr/bin/sh`, `/evil -> ../../../../Users/me`, `/loop -> loop`.
    type Tree = BTreeMap<(NodeId, &'static [u8]), (NodeId, FileKind, &'static [u8])>;

    fn tree() -> Tree {
        let mut t = BTreeMap::new();
        t.insert((ROOT, &b"etc"[..]), (2, FileKind::Dir, &b""[..]));
        t.insert((2, &b"passwd"[..]), (3, FileKind::File, &b""[..]));
        t.insert((ROOT, &b"bin"[..]), (4, FileKind::Symlink, &b"usr/bin"[..]));
        t.insert((ROOT, &b"usr"[..]), (5, FileKind::Dir, &b""[..]));
        t.insert((5, &b"bin"[..]), (6, FileKind::Dir, &b""[..]));
        t.insert((6, &b"sh"[..]), (7, FileKind::File, &b""[..]));
        t.insert(
            (ROOT, &b"evil"[..]),
            (8, FileKind::Symlink, &b"../../../../Users/me"[..]),
        );
        t.insert((ROOT, &b"loop"[..]), (9, FileKind::Symlink, &b"loop"[..]));
        t
    }

    /// Run a fake server on one end of a socketpair. `mutate` lets a
    /// test misbehave in the handshake.
    fn fake_server(token: [u8; 32], magic: [u8; 15], prove: bool) -> Stream {
        let (client, mut server) = UnixStream::pair().unwrap();
        thread::spawn(move || {
            let nonce = random32().unwrap();
            write_frame(
                &mut server,
                &ServerHello {
                    magic,
                    version: VERSION,
                    nonce,
                },
            )
            .unwrap();
            let Ok(auth) = read_frame::<_, ClientAuth>(&mut server) else {
                return;
            };
            if !ct_eq(&auth.mac, &client_mac(&token, &nonce)) {
                return; // close, as the real server does
            }
            let mac = if prove {
                server_mac(&token, &auth.nonce)
            } else {
                [0; 32]
            };
            write_frame(
                &mut server,
                &ServerAuth {
                    mac,
                    caps: 0,
                    root: attr(1, FileKind::Dir),
                },
            )
            .unwrap();
            let t = tree();
            while let Ok(req) = read_frame::<_, Request>(&mut server) {
                let result = match req.op {
                    Op::Lookup { parent, name } => t
                        .iter()
                        .find(|((p, n), _)| *p == parent && *n == name.as_slice())
                        .map(|(_, (id, kind, _))| Reply::Entry {
                            node: *id,
                            attr: attr(*id, *kind),
                        })
                        .ok_or(Errno::ENOENT),
                    Op::ReadLink { node } => t
                        .values()
                        .find(|(id, _, _)| *id == node)
                        .map(|(_, _, target)| Reply::Target(target.to_vec()))
                        .ok_or(Errno::EINVAL),
                    Op::ReadDir { .. } => Ok(Reply::Entries {
                        entries: vec![
                            DirEntry {
                                name: b"ok".to_vec(),
                                cookie: 1,
                                attr: attr(10, FileKind::File),
                            },
                            DirEntry {
                                name: b"../escape".to_vec(),
                                cookie: 2,
                                attr: attr(11, FileKind::File),
                            },
                            DirEntry {
                                name: b"..".to_vec(),
                                cookie: 3,
                                attr: attr(12, FileKind::Dir),
                            },
                        ],
                        eof: true,
                    }),
                    Op::Read { len, .. } => Ok(Reply::Data(vec![0; len as usize + 1])),
                    _ => Ok(Reply::Ok),
                };
                write_frame(
                    &mut server,
                    &ServerMsg::Response(Response { id: req.id, result }),
                )
                .unwrap();
            }
        });
        Stream::Unix(client)
    }

    fn client() -> Client {
        Client::handshake(fake_server(TOKEN, MAGIC, true), &TOKEN).unwrap()
    }

    fn path(r: &Resolved) -> String {
        r.path
            .iter()
            .map(|c| String::from_utf8_lossy(c).into_owned())
            .collect::<Vec<_>>()
            .join("/")
    }

    #[test]
    fn test_handshake_rejects_wrong_token_bad_magic_and_unproven_server() {
        let e = Client::handshake(fake_server(TOKEN, MAGIC, true), &[1; 32])
            .err()
            .unwrap();
        assert!(e.to_string().contains("rejected the session token"), "{e}");
        let e = Client::handshake(fake_server(TOKEN, *b"not-a-container", true), &TOKEN)
            .err()
            .unwrap();
        assert!(
            e.to_string().contains("not a container-mount server"),
            "{e}"
        );
        let e = Client::handshake(fake_server(TOKEN, MAGIC, false), &TOKEN)
            .err()
            .unwrap();
        assert!(e.to_string().contains("failed to prove"), "{e}");
    }

    #[test]
    fn test_resolve_follows_intermediate_symlinks_and_confines_them() {
        let c = client();
        let r = c.resolve(b"/bin/sh", false).unwrap();
        assert_eq!((r.node, path(&r).as_str()), (7, "usr/bin/sh"));
        // The last component is a link: kept unless asked to follow.
        assert_eq!(c.resolve(b"/bin", false).unwrap().node, 4);
        assert_eq!(c.resolve(b"/bin", true).unwrap().node, 6);
        // `..` is lexical and clamps at the root.
        assert_eq!(c.resolve(b"/../../etc/./passwd", false).unwrap().node, 3);
        assert_eq!(c.resolve(b"/", false).unwrap().node, ROOT);
    }

    #[test]
    fn test_escaping_link_resolves_inside_the_guest() {
        let c = client();
        // `/evil -> ../../../../Users/me` must look up `Users` under the
        // guest root (and fail there), not reach the host.
        let e = c.resolve(b"/evil/x", false).unwrap_err();
        assert!(matches!(e, Error::Guest(Errno::ENOENT)), "{e:?}");
        let link = c.resolve(b"/evil", false).unwrap();
        assert_eq!(c.readlink_confined(&link).unwrap(), b"Users/me".to_vec());
    }

    #[test]
    fn test_symlink_loops_end_in_eloop() {
        let e = client().resolve(b"/loop/x", false).unwrap_err();
        assert!(matches!(e, Error::Guest(Errno::ELOOP)), "{e:?}");
    }

    #[test]
    fn test_hostile_entry_names_and_oversized_reads_are_rejected() {
        let c = client();
        let names: Vec<Vec<u8>> = c
            .readdir_all(ROOT)
            .unwrap()
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert_eq!(names, vec![b"ok".to_vec()]);
        assert!(matches!(c.read(1, 0, 16), Err(Error::Protocol(_))));
        assert!(matches!(
            c.lookup(ROOT, b"a/b"),
            Err(Error::Guest(Errno::EINVAL))
        ));
    }

    #[test]
    fn test_calls_after_the_server_goes_away_fail_cleanly() {
        let (a, b) = UnixStream::pair().unwrap();
        drop(b);
        assert!(Client::handshake(Stream::Unix(a), &TOKEN).is_err());
    }
}
