/*
 * Copyright (c) 2026 Daphne Pfister
 * SPDX-License-Identifier: BSD-2-Clause
 * See LICENSE file for full license text
 */

//! The Linux server: listener, handshake, and the operations.
//!
//! Every resolution is `*at(parent_fd, single_component, …NOFOLLOW)`
//! against `O_PATH` node fds, so there is no path traversal surface.
//! Node and file-handle tables are per connection: a client that goes
//! away takes its references with it.

// `stat`/`statfs` field widths differ across architectures; the casts
// keep the code portable even where they're no-ops.
#![allow(clippy::unnecessary_cast)]

use std::collections::HashMap;
use std::ffi::{CStr, CString};
use std::fs::{self, File};
use std::io::{self, ErrorKind};
use std::net::TcpListener;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::fs::{FileExt, PermissionsExt};
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use distrofs::connect::Stream;
use distrofs::proto::*;

use crate::mounts;
use crate::serve::{Listen, ServeArgs};

/// Requests in flight per connection.
const WORKERS: usize = 4;
/// Most entries one `ReadDir` reply carries.
const MAX_ENTRIES: u32 = 1024;
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

type R<T> = Result<T, Errno>;

fn last_errno() -> Errno {
    Errno(io::Error::last_os_error().raw_os_error().unwrap_or(5))
}

fn io_errno(e: &io::Error) -> Errno {
    Errno(e.raw_os_error().unwrap_or(5))
}

fn cvt(rc: libc::c_int) -> R<libc::c_int> {
    if rc < 0 { Err(last_errno()) } else { Ok(rc) }
}

fn cname(name: &[u8]) -> R<CString> {
    if !valid_name(name) {
        return Err(if name.len() > NAME_MAX {
            Errno::ENAMETOOLONG
        } else {
            Errno::EINVAL
        });
    }
    CString::new(name).map_err(|_| Errno::EINVAL)
}

fn proc_path(fd: RawFd) -> CString {
    CString::new(format!("/proc/self/fd/{fd}")).unwrap_or_default()
}

fn openat(dirfd: RawFd, name: &CStr, flags: libc::c_int, mode: libc::mode_t) -> R<OwnedFd> {
    // SAFETY: `name` is NUL-terminated; a non-negative return is a fresh fd we own.
    let fd = cvt(unsafe { libc::openat(dirfd, name.as_ptr(), flags | libc::O_CLOEXEC, mode) })?;
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn fstat(fd: RawFd) -> R<libc::stat> {
    // SAFETY: `st` is a valid out-param.
    let mut st = unsafe { std::mem::zeroed::<libc::stat>() };
    cvt(unsafe { libc::fstat(fd, &mut st) })?;
    Ok(st)
}

fn fstatat(dirfd: RawFd, name: &CStr) -> R<libc::stat> {
    // SAFETY: as `fstat`; `name` is NUL-terminated.
    let mut st = unsafe { std::mem::zeroed::<libc::stat>() };
    cvt(unsafe { libc::fstatat(dirfd, name.as_ptr(), &mut st, libc::AT_SYMLINK_NOFOLLOW) })?;
    Ok(st)
}

fn time(sec: i64, nsec: i64) -> Time {
    Time {
        sec,
        nsec: nsec.clamp(0, 999_999_999) as u32,
    }
}

/// The attributes a client sees. A hidden mountpoint is an empty,
/// read-only, root-owned directory.
fn present(st: &libc::stat, hidden: bool) -> Attr {
    let atime = time(st.st_atime as i64, st.st_atime_nsec as i64);
    let mtime = time(st.st_mtime as i64, st.st_mtime_nsec as i64);
    let ctime = time(st.st_ctime as i64, st.st_ctime_nsec as i64);
    if hidden {
        return Attr {
            ino: st.st_ino as u64,
            kind: FileKind::Dir,
            perm: 0o555,
            nlink: 2,
            uid: 0,
            gid: 0,
            rdev: 0,
            size: 0,
            blocks: 0,
            blksize: 4096,
            atime,
            mtime,
            ctime,
        };
    }
    Attr {
        ino: st.st_ino as u64,
        kind: FileKind::from_mode(st.st_mode as u32).unwrap_or(FileKind::File),
        perm: st.st_mode as u32 & 0o7777,
        nlink: st.st_nlink as u64,
        uid: st.st_uid,
        gid: st.st_gid,
        rdev: st.st_rdev as u64,
        size: st.st_size.max(0) as u64,
        blocks: st.st_blocks.max(0) as u64,
        blksize: st.st_blksize.max(0) as u32,
        atime,
        mtime,
        ctime,
    }
}

/// State shared by every connection.
struct Shared {
    uid: u32,
    gid: u32,
    mounts: Mutex<mounts::Table>,
}

impl Shared {
    fn load_mounts() -> mounts::Table {
        mounts::Table::parse(&fs::read_to_string("/proc/self/mountinfo").unwrap_or_default())
    }

    /// Whether the filesystem on `dev` is served; mountinfo is re-read
    /// once for a device it hasn't seen (mounts change at run time).
    fn served(&self, dev: u64) -> bool {
        let (maj, min) = (
            libc::major(dev as libc::dev_t),
            libc::minor(dev as libc::dev_t),
        );
        let Ok(mut t) = self.mounts.lock() else {
            return false;
        };
        if t.fstype(maj, min).is_none() {
            *t = Self::load_mounts();
        }
        t.fstype(maj, min).is_some_and(mounts::served)
    }
}

struct Node {
    fd: Arc<OwnedFd>,
    dev: u64,
    ino: u64,
    lookups: u64,
    hidden: bool,
}

#[derive(Default)]
struct Tables {
    nodes: HashMap<NodeId, Node>,
    by_ino: HashMap<(u64, u64), NodeId>,
    next_node: NodeId,
    files: HashMap<FileHandle, Arc<File>>,
    next_fh: FileHandle,
}

/// One authenticated client session.
struct Conn {
    shared: Arc<Shared>,
    t: Mutex<Tables>,
}

/// A node's fd and facts, copied out of the table so I/O runs unlocked.
struct NodeRef {
    fd: Arc<OwnedFd>,
    dev: u64,
    hidden: bool,
}

impl NodeRef {
    fn raw(&self) -> RawFd {
        self.fd.as_raw_fd()
    }

    /// The node as a parent for a mutation: hidden mountpoints are read-only.
    fn writable(self) -> R<NodeRef> {
        if self.hidden {
            Err(Errno::EROFS)
        } else {
            Ok(self)
        }
    }
}

impl Conn {
    fn new(shared: Arc<Shared>) -> R<(Conn, Attr)> {
        let fd = openat(libc::AT_FDCWD, c"/", libc::O_PATH | libc::O_DIRECTORY, 0)?;
        let st = fstat(fd.as_raw_fd())?;
        let mut t = Tables {
            next_node: ROOT + 1,
            next_fh: 1,
            ..Tables::default()
        };
        t.by_ino.insert((st.st_dev as u64, st.st_ino as u64), ROOT);
        t.nodes.insert(
            ROOT,
            Node {
                fd: Arc::new(fd),
                dev: st.st_dev as u64,
                ino: st.st_ino as u64,
                lookups: 1,
                hidden: false,
            },
        );
        Ok((
            Conn {
                shared,
                t: Mutex::new(t),
            },
            present(&st, false),
        ))
    }

    fn tables(&self) -> R<std::sync::MutexGuard<'_, Tables>> {
        self.t.lock().map_err(|_| Errno::EIO)
    }

    fn node(&self, id: NodeId) -> R<NodeRef> {
        let t = self.tables()?;
        let n = t.nodes.get(&id).ok_or(Errno::EBADF)?;
        Ok(NodeRef {
            fd: Arc::clone(&n.fd),
            dev: n.dev,
            hidden: n.hidden,
        })
    }

    fn file(&self, fh: FileHandle) -> R<Arc<File>> {
        self.tables()?.files.get(&fh).cloned().ok_or(Errno::EBADF)
    }

    /// Register `fd` (just resolved under `parent`) as a node, reusing
    /// the id when the inode is already known; adds one lookup reference.
    fn insert(&self, parent: &NodeRef, fd: OwnedFd) -> R<(NodeId, Attr)> {
        let st = fstat(fd.as_raw_fd())?;
        let (dev, ino) = (st.st_dev as u64, st.st_ino as u64);
        let hidden = dev != parent.dev && !self.shared.served(dev);
        let mut t = self.tables()?;
        if let Some(&id) = t.by_ino.get(&(dev, ino))
            && let Some(n) = t.nodes.get_mut(&id)
        {
            n.lookups += 1;
            let hidden = n.hidden;
            return Ok((id, present(&st, hidden)));
        }
        let id = t.next_node;
        t.next_node += 1;
        t.by_ino.insert((dev, ino), id);
        t.nodes.insert(
            id,
            Node {
                fd: Arc::new(fd),
                dev,
                ino,
                lookups: 1,
                hidden,
            },
        );
        Ok((id, present(&st, hidden)))
    }

    fn add_file(&self, f: File) -> R<FileHandle> {
        let mut t = self.tables()?;
        let fh = t.next_fh;
        t.next_fh += 1;
        t.files.insert(fh, Arc::new(f));
        Ok(fh)
    }

    fn lookup_fd(&self, parent: &NodeRef, name: &CStr) -> R<(NodeId, Attr)> {
        let fd = openat(parent.raw(), name, libc::O_PATH | libc::O_NOFOLLOW, 0)?;
        self.insert(parent, fd)
    }

    /// Give a just-created entry to the provisioned user.
    fn chown_new(&self, parent: &NodeRef, name: &CStr) -> R<()> {
        // SAFETY: `name` is NUL-terminated; parent fd is live.
        cvt(unsafe {
            libc::fchownat(
                parent.raw(),
                name.as_ptr(),
                self.shared.uid,
                self.shared.gid,
                libc::AT_SYMLINK_NOFOLLOW,
            )
        })
        .map(drop)
    }

    fn dispatch(&self, op: Op) -> R<Reply> {
        match op {
            Op::Lookup { parent, name } => {
                let p = self.node(parent)?;
                if p.hidden {
                    return Err(Errno::ENOENT);
                }
                let (node, attr) = self.lookup_fd(&p, &cname(&name)?)?;
                Ok(Reply::Entry { node, attr })
            }
            Op::Forget { node, n } => {
                let mut t = self.tables()?;
                if node != ROOT
                    && let Some(e) = t.nodes.get_mut(&node)
                {
                    e.lookups = e.lookups.saturating_sub(n);
                    if e.lookups == 0 {
                        let key = (e.dev, e.ino);
                        t.nodes.remove(&node);
                        t.by_ino.remove(&key);
                    }
                }
                Ok(Reply::Ok)
            }
            Op::GetAttr { node } => {
                let n = self.node(node)?;
                Ok(Reply::Attr(present(&fstat(n.raw())?, n.hidden)))
            }
            Op::SetAttr { node, set } => self.setattr(self.node(node)?.writable()?, &set),
            Op::ReadDir {
                node,
                cookie,
                max_entries,
            } => self.readdir(&self.node(node)?, cookie, max_entries),
            Op::Open { node, flags } => {
                let n = self.node(node)?;
                if n.hidden {
                    return Err(Errno::EISDIR);
                }
                match FileKind::from_mode(fstat(n.raw())?.st_mode) {
                    Some(FileKind::File) => {}
                    Some(FileKind::Dir) => return Err(Errno::EISDIR),
                    _ => return Err(Errno::EOPNOTSUPP),
                }
                // Re-open the inode the O_PATH fd pins, not a path.
                let path = proc_path(n.raw());
                let fd = openat(libc::AT_FDCWD, &path, open_flags(flags) | libc::O_NOCTTY, 0)?;
                Ok(Reply::Opened {
                    fh: self.add_file(File::from(fd))?,
                })
            }
            Op::Read { fh, offset, len } => {
                let f = self.file(fh)?;
                let mut buf = vec![0u8; len.min(MAX_IO) as usize];
                let mut got = 0;
                while got < buf.len() {
                    match f.read_at(&mut buf[got..], offset + got as u64) {
                        Ok(0) => break,
                        Ok(n) => got += n,
                        Err(e) if e.kind() == ErrorKind::Interrupted => {}
                        Err(e) => return Err(io_errno(&e)),
                    }
                }
                buf.truncate(got);
                Ok(Reply::Data(buf))
            }
            Op::Write { fh, offset, data } => {
                if data.len() > MAX_IO as usize {
                    return Err(Errno::EINVAL);
                }
                self.file(fh)?
                    .write_all_at(&data, offset)
                    .map_err(|e| io_errno(&e))?;
                Ok(Reply::Written(data.len() as u32))
            }
            Op::Fsync { fh } => {
                self.file(fh)?.sync_all().map_err(|e| io_errno(&e))?;
                Ok(Reply::Ok)
            }
            Op::Release { fh } => {
                self.tables()?.files.remove(&fh);
                Ok(Reply::Ok)
            }
            Op::Create {
                parent,
                name,
                perm,
                exclusive,
                flags,
            } => self.create(
                self.node(parent)?.writable()?,
                &cname(&name)?,
                perm,
                exclusive,
                flags,
            ),
            Op::Mkdir { parent, name, perm } => {
                let p = self.node(parent)?.writable()?;
                let c = cname(&name)?;
                // SAFETY: `c` is NUL-terminated; parent fd is live.
                cvt(unsafe { libc::mkdirat(p.raw(), c.as_ptr(), perm & 0o7777) })?;
                self.chown_new(&p, &c)?;
                let (node, attr) = self.lookup_fd(&p, &c)?;
                Ok(Reply::Entry { node, attr })
            }
            Op::Unlink { parent, name } => self.unlink(parent, &name, 0),
            Op::Rmdir { parent, name } => self.unlink(parent, &name, libc::AT_REMOVEDIR),
            Op::Rename {
                parent,
                name,
                new_parent,
                new_name,
                flags,
            } => {
                let (p, np) = (
                    self.node(parent)?.writable()?,
                    self.node(new_parent)?.writable()?,
                );
                let (c, nc) = (cname(&name)?, cname(&new_name)?);
                let flags = flags & (RENAME_NOREPLACE | RENAME_EXCHANGE);
                // SAFETY: names are NUL-terminated; parent fds are live.
                let rc = unsafe {
                    libc::syscall(
                        libc::SYS_renameat2,
                        p.raw(),
                        c.as_ptr(),
                        np.raw(),
                        nc.as_ptr(),
                        flags,
                    )
                };
                if rc < 0 {
                    return Err(last_errno());
                }
                Ok(Reply::Ok)
            }
            Op::Symlink {
                parent,
                name,
                target,
            } => {
                let p = self.node(parent)?.writable()?;
                let c = cname(&name)?;
                if target.is_empty() || target.len() > TARGET_MAX {
                    return Err(Errno::EINVAL);
                }
                let t = CString::new(target).map_err(|_| Errno::EINVAL)?;
                // SAFETY: both strings are NUL-terminated; parent fd is live.
                cvt(unsafe { libc::symlinkat(t.as_ptr(), p.raw(), c.as_ptr()) })?;
                self.chown_new(&p, &c)?;
                let (node, attr) = self.lookup_fd(&p, &c)?;
                Ok(Reply::Entry { node, attr })
            }
            Op::ReadLink { node } => {
                let n = self.node(node)?;
                let mut buf = vec![0u8; TARGET_MAX];
                // SAFETY: `buf` is writable for its length; an empty path
                // reads the O_PATH symlink fd itself.
                let len = unsafe {
                    libc::readlinkat(n.raw(), c"".as_ptr(), buf.as_mut_ptr().cast(), buf.len())
                };
                if len < 0 {
                    return Err(last_errno());
                }
                buf.truncate(len as usize);
                Ok(Reply::Target(buf))
            }
            Op::Link {
                node,
                new_parent,
                new_name,
            } => {
                let n = self.node(node)?;
                let np = self.node(new_parent)?.writable()?;
                let nc = cname(&new_name)?;
                // SAFETY: names are NUL-terminated; fds are live.
                cvt(unsafe {
                    libc::linkat(
                        n.raw(),
                        c"".as_ptr(),
                        np.raw(),
                        nc.as_ptr(),
                        libc::AT_EMPTY_PATH,
                    )
                })?;
                let (node, attr) = self.lookup_fd(&np, &nc)?;
                Ok(Reply::Entry { node, attr })
            }
            Op::StatFs { node } => {
                let n = self.node(node)?;
                // SAFETY: `s` is a valid out-param.
                let mut s = unsafe { std::mem::zeroed::<libc::statfs>() };
                cvt(unsafe { libc::fstatfs(n.raw(), &mut s) })?;
                Ok(Reply::StatFs(StatFs {
                    bsize: s.f_bsize as u32,
                    blocks: s.f_blocks as u64,
                    bfree: s.f_bfree as u64,
                    bavail: s.f_bavail as u64,
                    files: s.f_files as u64,
                    ffree: s.f_ffree as u64,
                    namemax: s.f_namelen as u32,
                }))
            }
        }
    }

    fn unlink(&self, parent: NodeId, name: &[u8], flags: libc::c_int) -> R<Reply> {
        let p = self.node(parent)?.writable()?;
        let c = cname(name)?;
        // SAFETY: `c` is NUL-terminated; parent fd is live.
        cvt(unsafe { libc::unlinkat(p.raw(), c.as_ptr(), flags) })?;
        Ok(Reply::Ok)
    }

    fn create(
        &self,
        p: NodeRef,
        c: &CStr,
        perm: u32,
        exclusive: bool,
        flags: OpenFlags,
    ) -> R<Reply> {
        let base = open_flags(flags) | libc::O_NOFOLLOW | libc::O_NOCTTY;
        // Try a fresh create first so only files we made get chowned.
        let (fd, created) = match openat(
            p.raw(),
            c,
            base | libc::O_CREAT | libc::O_EXCL,
            perm & 0o7777,
        ) {
            Ok(fd) => (fd, true),
            Err(e) if e == Errno::EEXIST && !exclusive => (openat(p.raw(), c, base, 0)?, false),
            Err(e) => return Err(e),
        };
        if created {
            // SAFETY: fd is live.
            cvt(unsafe { libc::fchown(fd.as_raw_fd(), self.shared.uid, self.shared.gid) })?;
        }
        if FileKind::from_mode(fstat(fd.as_raw_fd())?.st_mode) != Some(FileKind::File) {
            return Err(Errno::EOPNOTSUPP);
        }
        let node_fd = fd.try_clone().map_err(|e| io_errno(&e))?;
        let (node, attr) = self.insert(&p, node_fd)?;
        let fh = self.add_file(File::from(fd))?;
        Ok(Reply::Created { node, attr, fh })
    }

    fn setattr(&self, n: NodeRef, set: &SetAttr) -> R<Reply> {
        let fd = n.raw();
        let kind = FileKind::from_mode(fstat(fd)?.st_mode);
        let path = proc_path(fd);
        if let Some(perm) = set.perm {
            if kind == Some(FileKind::Symlink) {
                return Err(Errno::EOPNOTSUPP);
            }
            // SAFETY: `path` is NUL-terminated; chmod has no fd form for O_PATH.
            cvt(unsafe { libc::chmod(path.as_ptr(), perm & 0o7777) })?;
        }
        if set.uid.is_some() || set.gid.is_some() {
            let (u, g) = (set.uid.unwrap_or(u32::MAX), set.gid.unwrap_or(u32::MAX));
            // SAFETY: empty path + AT_EMPTY_PATH operates on the fd itself.
            cvt(unsafe { libc::fchownat(fd, c"".as_ptr(), u, g, libc::AT_EMPTY_PATH) })?;
        }
        if let Some(size) = set.size {
            if kind != Some(FileKind::File) {
                return Err(Errno::EINVAL);
            }
            let f = File::from(openat(
                libc::AT_FDCWD,
                &path,
                libc::O_WRONLY | libc::O_NOCTTY,
                0,
            )?);
            f.set_len(size).map_err(|e| io_errno(&e))?;
        }
        if set.atime.is_some() || set.mtime.is_some() {
            if kind == Some(FileKind::Symlink) {
                return Err(Errno::EOPNOTSUPP);
            }
            let ts = |t: Option<TimeSet>| match t {
                None => libc::timespec {
                    tv_sec: 0,
                    tv_nsec: libc::UTIME_OMIT,
                },
                Some(TimeSet::Now) => libc::timespec {
                    tv_sec: 0,
                    tv_nsec: libc::UTIME_NOW,
                },
                Some(TimeSet::At(t)) => libc::timespec {
                    tv_sec: t.sec,
                    tv_nsec: t.nsec.min(999_999_999).into(),
                },
            };
            let times = [ts(set.atime), ts(set.mtime)];
            // SAFETY: `path` is NUL-terminated; `times` has two entries.
            cvt(unsafe { libc::utimensat(libc::AT_FDCWD, path.as_ptr(), times.as_ptr(), 0) })?;
        }
        Ok(Reply::Attr(present(&fstat(fd)?, n.hidden)))
    }

    fn readdir(&self, n: &NodeRef, cookie: u64, max: u32) -> R<Reply> {
        if n.hidden {
            return Ok(Reply::Entries {
                entries: Vec::new(),
                eof: true,
            });
        }
        let fd = openat(n.raw(), c".", libc::O_RDONLY | libc::O_DIRECTORY, 0)?;
        // SAFETY: fdopendir takes ownership of the fd on success.
        let dir = unsafe { libc::fdopendir(fd.as_raw_fd()) };
        if dir.is_null() {
            return Err(last_errno());
        }
        std::mem::forget(fd);
        let dir = Dir(dir);
        if cookie != 0 {
            // SAFETY: `dir` is open; the cookie came from `telldir`.
            unsafe { libc::seekdir(dir.0, cookie as libc::c_long) };
        }
        // SAFETY: `dir` is open.
        let dfd = unsafe { libc::dirfd(dir.0) };
        let mut entries = Vec::new();
        let max = max.clamp(1, MAX_ENTRIES) as usize;
        loop {
            if entries.len() >= max {
                return Ok(Reply::Entries {
                    entries,
                    eof: false,
                });
            }
            // SAFETY: `dir` is open; the entry is valid until the next readdir.
            let ent = unsafe { libc::readdir(dir.0) };
            if ent.is_null() {
                return Ok(Reply::Entries { entries, eof: true });
            }
            // SAFETY: d_name is NUL-terminated within the entry.
            let name = unsafe { CStr::from_ptr((*ent).d_name.as_ptr()) };
            if name == c"." || name == c".." {
                continue;
            }
            // SAFETY: `dir` is open.
            let next = unsafe { libc::telldir(dir.0) } as u64;
            // Entries can vanish between readdir and stat; skip those.
            let Ok(st) = fstatat(dfd, name) else { continue };
            let hidden = st.st_dev as u64 != n.dev && !self.shared.served(st.st_dev as u64);
            entries.push(DirEntry {
                name: name.to_bytes().to_vec(),
                cookie: next,
                attr: present(&st, hidden),
            });
        }
    }
}

/// Closes a `DIR*` on drop.
struct Dir(*mut libc::DIR);

impl Drop for Dir {
    fn drop(&mut self) {
        // SAFETY: the pointer came from a successful fdopendir.
        unsafe { libc::closedir(self.0) };
    }
}

fn open_flags(f: OpenFlags) -> libc::c_int {
    let mut flags = match (f.read, f.write) {
        (_, false) => libc::O_RDONLY,
        (false, true) => libc::O_WRONLY,
        (true, true) => libc::O_RDWR,
    };
    if f.write && f.truncate {
        flags |= libc::O_TRUNC;
    }
    if f.append {
        flags |= libc::O_APPEND;
    }
    flags
}

/// The socket `serve` listens on.
enum Listener {
    Unix(UnixListener),
    Tcp(TcpListener),
}

impl Listener {
    fn accept(&self) -> io::Result<Stream> {
        match self {
            Listener::Unix(l) => l.accept().map(|(s, _)| Stream::Unix(s)),
            Listener::Tcp(l) => l.accept().map(|(s, _)| Stream::Tcp(s)),
        }
    }
}

/// Bind a Unix socket in a `0700` directory owned by this user,
/// replacing any previous instance's socket ("last starter wins": the
/// relay connects by path, so older instances only keep the clients
/// they already have).
fn bind_unix(socket: &Path) -> io::Result<UnixListener> {
    if let Some(dir) = socket.parent() {
        fs::create_dir_all(dir)?;
        let meta = fs::symlink_metadata(dir)?;
        if !meta.is_dir() {
            return Err(io::Error::other(format!(
                "{} is not a directory",
                dir.display()
            )));
        }
        // SAFETY: geteuid/getegid cannot fail.
        let (uid, gid) = unsafe { (libc::geteuid(), libc::getegid()) };
        std::os::unix::fs::lchown(dir, Some(uid), Some(gid))?;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    }
    match fs::remove_file(socket) {
        Err(e) if e.kind() != ErrorKind::NotFound => return Err(e),
        _ => {}
    }
    UnixListener::bind(socket)
}

/// The listener for `--socket`/`--listen`.
fn listen(spec: &Listen) -> io::Result<Listener> {
    match spec {
        Listen::Unix(path) => bind_unix(path).map(Listener::Unix),
        Listen::Tcp(addr) => TcpListener::bind(addr).map(Listener::Tcp),
    }
}

/// One client: handshake, then requests fanned out to a worker pool
/// with responses written back in completion order.
fn session(mut s: Stream, token: &[u8], shared: Arc<Shared>) -> io::Result<()> {
    s.set_read_timeout(Some(HANDSHAKE_TIMEOUT))?;
    let nonce = random32()?;
    write_frame(
        &mut s,
        &ServerHello {
            magic: MAGIC,
            version: VERSION,
            nonce,
        },
    )?;
    let auth: ClientAuth = read_frame(&mut s)?;
    if auth.version != VERSION || !ct_eq(&auth.mac, &client_mac(token, &nonce)) {
        // No detail for a peer that couldn't prove the token.
        return Ok(());
    }
    let (conn, root) = Conn::new(shared).map_err(|e| io::Error::from_raw_os_error(e.0))?;
    write_frame(
        &mut s,
        &ServerAuth {
            mac: server_mac(token, &auth.nonce),
            caps: 0,
            root,
        },
    )?;
    s.set_read_timeout(None)?;

    let conn = Arc::new(conn);
    let writer = Arc::new(Mutex::new(s.try_clone()?));
    let (tx, rx) = mpsc::channel::<Request>();
    let rx = Arc::new(Mutex::new(rx));
    let workers: Vec<_> = (0..WORKERS)
        .map(|_| {
            let (conn, writer, rx) = (Arc::clone(&conn), Arc::clone(&writer), Arc::clone(&rx));
            thread::spawn(move || {
                loop {
                    let Ok(req) = rx.lock().map_err(drop).and_then(|r| r.recv().map_err(drop))
                    else {
                        return;
                    };
                    let msg = ServerMsg::Response(Response {
                        id: req.id,
                        result: conn.dispatch(req.op),
                    });
                    let Ok(mut w) = writer.lock() else { return };
                    if write_frame(&mut *w, &msg).is_err() {
                        return;
                    }
                }
            })
        })
        .collect();
    while let Ok(req) = read_frame::<_, Request>(&mut s) {
        if tx.send(req).is_err() {
            break;
        }
    }
    drop(tx);
    for w in workers {
        let _ = w.join();
    }
    Ok(())
}

pub fn serve(args: &ServeArgs) -> io::Result<()> {
    let token = std::env::var(TOKEN_ENV)
        .ok()
        .and_then(|t| unhex(&t))
        .filter(|t| t.len() == 32)
        .ok_or_else(|| io::Error::other(format!("missing or malformed {TOKEN_ENV}")))?;
    // SAFETY: still single-threaded; nothing else reads the environment.
    unsafe { std::env::remove_var(TOKEN_ENV) };
    let token: Arc<[u8]> = token.into();

    let listener = listen(&args.listen)?;
    let shared = Arc::new(Shared {
        uid: args.uid,
        gid: args.gid,
        mounts: Mutex::new(Shared::load_mounts()),
    });
    let active = Arc::new(AtomicUsize::new(0));
    let last = Arc::new(Mutex::new(Instant::now()));
    {
        let (active, last, idle) = (Arc::clone(&active), Arc::clone(&last), args.idle);
        thread::spawn(move || {
            loop {
                thread::sleep(Duration::from_secs(1));
                let quiet = last.lock().map(|l| l.elapsed()).unwrap_or_default();
                if active.load(Ordering::SeqCst) == 0 && quiet >= idle {
                    std::process::exit(0);
                }
            }
        });
    }
    loop {
        let Ok(stream) = listener.accept() else {
            continue;
        };
        let (token, shared, active, last) = (
            Arc::clone(&token),
            Arc::clone(&shared),
            Arc::clone(&active),
            Arc::clone(&last),
        );
        active.fetch_add(1, Ordering::SeqCst);
        thread::spawn(move || {
            if let Err(e) = session(stream, &token, shared) {
                eprintln!("distrofs: session ended: {e}");
            }
            if let Ok(mut l) = last.lock() {
                *l = Instant::now();
            }
            active.fetch_sub(1, Ordering::SeqCst);
        });
    }
}
