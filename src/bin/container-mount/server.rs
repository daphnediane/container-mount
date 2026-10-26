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
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use container_mount::connect::Stream;
use container_mount::proto::*;

use container_mount::users;

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

// The clone ioctls aren't in the `libc` crate's musl bindings; the
// numbers are asm-generic, so they're stable across arches.
const FICLONE: libc::Ioctl = 0x4004_9409;
const FICLONERANGE: libc::Ioctl = 0x4020_940D;

#[repr(C)]
struct FileCloneRange {
    src_fd: i64,
    src_offset: u64,
    src_length: u64,
    dest_offset: u64,
}

fn lseek(fd: RawFd, off: u64, whence: libc::c_int) -> R<u64> {
    // SAFETY: off is clamped to off_t's range.
    let r = unsafe { libc::lseek(fd, off.min(i64::MAX as u64) as libc::off_t, whence) };
    if r < 0 {
        Err(last_errno())
    } else {
        Ok(r as u64)
    }
}

/// One `copy_file_range` call — a raw syscall so the answer doesn't
/// depend on the libc flavor's wrapper or its kernel-emulation quirks.
/// Advances `*src_off`/`*dst_off` by what it copied.
fn cfr(src: RawFd, src_off: &mut u64, dst: RawFd, dst_off: &mut u64, len: u64) -> R<u64> {
    let (mut a, mut b) = (*src_off as libc::loff_t, *dst_off as libc::loff_t);
    // SAFETY: `a`/`b` are valid in/out offset pointers.
    let n = unsafe {
        libc::syscall(
            libc::SYS_copy_file_range,
            src,
            &mut a,
            dst,
            &mut b,
            len.min(usize::MAX as u64) as usize,
            0,
        )
    };
    if n < 0 {
        return Err(last_errno());
    }
    *src_off = a as u64;
    *dst_off = b as u64;
    Ok(n as u64)
}

/// Copy the range `[pos, pos+len)` from `src` to `dst` at `dst_pos`,
/// skipping holes so a sparse source stays sparse. Returns `EINVAL`
/// when the filesystem can't report extents — the caller retries with
/// a dense copy. Returns the whole `len` on success (skipped holes
/// count: they copied as holes).
fn copy_extents(src: RawFd, mut pos: u64, dst: RawFd, mut dst_pos: u64, len: u64) -> R<u64> {
    let stop = pos + len;
    while pos < stop {
        let start = match lseek(src, pos, libc::SEEK_DATA) {
            Err(Errno::ENXIO) => break,
            r => r?,
        };
        if start >= stop {
            break;
        }
        let end = lseek(src, start, libc::SEEK_HOLE)?.min(stop);
        dst_pos += start - pos;
        let mut cur = start;
        while cur < end {
            let want = end - cur;
            if cfr(src, &mut cur, dst, &mut dst_pos, want)? == 0 {
                break;
            }
        }
        pos = end;
    }
    Ok(len)
}

/// `copy_file_range` between two open files: shared extents where the
/// filesystem does it itself, hole-skipping copies elsewhere. `len` 0
/// means to the end of the source. Returns `(bytes covered, shared)`.
fn copy_range(
    src: &File,
    src_off: u64,
    dst: &File,
    dst_off: u64,
    len: u64,
    require_shared: bool,
) -> R<(u64, bool)> {
    let st = fstat(src.as_raw_fd())?;
    if FileKind::from_mode(st.st_mode) != Some(FileKind::File) {
        return Err(Errno::EINVAL);
    }
    let size = st.st_size.max(0) as u64;
    let avail = size.saturating_sub(src_off);
    let len = if len == 0 { avail } else { len.min(avail) };
    if len == 0 {
        return Ok((0, false));
    }
    if require_shared {
        let range = FileCloneRange {
            src_fd: src.as_raw_fd() as i64,
            src_offset: src_off,
            src_length: len,
            dest_offset: dst_off,
        };
        // SAFETY: `range` is valid; the ioctl is all-or-nothing.
        cvt(unsafe { libc::ioctl(dst.as_raw_fd(), FICLONERANGE, &range) })?;
        return Ok((len, true));
    }
    match copy_extents(src.as_raw_fd(), src_off, dst.as_raw_fd(), dst_off, len) {
        Err(Errno::EINVAL) => {}
        r => return r.map(|n| (n, false)),
    }
    // Dense fallback — the fs can't report extents.
    let (mut so, mut dof) = (src_off, dst_off);
    let mut done = 0;
    while done < len {
        match cfr(
            src.as_raw_fd(),
            &mut so,
            dst.as_raw_fd(),
            &mut dof,
            len - done,
        ) {
            Ok(0) => break,
            Ok(n) => done += n,
            Err(Errno(4)) => {} // EINTR
            Err(e) => return Err(e),
        }
    }
    Ok((done, false))
}

/// Our wire `FALLOC_*` bits to the kernel's `FALLOC_FL_*`.
fn falloc_mode(m: u32) -> R<libc::c_int> {
    const KNOWN: u32 = FALLOC_KEEP_SIZE
        | FALLOC_PUNCH_HOLE
        | FALLOC_COLLAPSE_RANGE
        | FALLOC_ZERO_RANGE
        | FALLOC_INSERT_RANGE
        | FALLOC_UNSHARE_RANGE;
    if m & !KNOWN != 0 {
        return Err(Errno::EINVAL);
    }
    let mut out = 0;
    for (bit, c) in [
        (FALLOC_KEEP_SIZE, libc::FALLOC_FL_KEEP_SIZE),
        (FALLOC_PUNCH_HOLE, libc::FALLOC_FL_PUNCH_HOLE),
        (FALLOC_COLLAPSE_RANGE, libc::FALLOC_FL_COLLAPSE_RANGE),
        (FALLOC_ZERO_RANGE, libc::FALLOC_FL_ZERO_RANGE),
        (FALLOC_INSERT_RANGE, libc::FALLOC_FL_INSERT_RANGE),
        (FALLOC_UNSHARE_RANGE, libc::FALLOC_FL_UNSHARE_RANGE),
    ] {
        if m & bit != 0 {
            out |= c;
        }
    }
    Ok(out)
}

fn xattr_name(name: &[u8]) -> R<CString> {
    if name.is_empty() || name.len() > XATTR_NAME_MAX {
        return Err(if name.len() > XATTR_NAME_MAX {
            Errno::ENAMETOOLONG
        } else {
            Errno::EINVAL
        });
    }
    CString::new(name).map_err(|_| Errno::EINVAL)
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

    /// `spec` resolved against the guest's `/etc/passwd` and
    /// `/etc/group`, read fresh each time.
    fn lookup_user(&self, spec: &[u8]) -> R<users::User> {
        if spec.len() > USER_SPEC_MAX || spec.contains(&0) {
            return Err(Errno::EINVAL);
        }
        let (passwd, group) = (self.read_etc(c"passwd"), self.read_etc(c"group"));
        let files = users::Files {
            passwd: &passwd,
            group: &group,
        };
        // libc first, like container-distro's `getent … || cat`.
        users::resolve(spec, &(Libc, files)).map_err(|e| match e {
            users::Error::Malformed => Errno::EINVAL,
            users::Error::UnknownUser | users::Error::UnknownGroup => Errno::ENOENT,
        })
    }

    /// `/etc/<name>` from the guest root, through the same no-follow,
    /// served-filesystem rules as every other lookup — a symlinked
    /// `/etc` or file, a host share, or anything but a small regular
    /// file reads as absent (empty), and resolution falls back.
    fn read_etc(&self, name: &CStr) -> Vec<u8> {
        const MAX: u64 = 4 << 20;
        let read = || -> R<Vec<u8>> {
            let root = openat(libc::AT_FDCWD, c"/", libc::O_PATH | libc::O_DIRECTORY, 0)?;
            let etc = openat(
                root.as_raw_fd(),
                c"etc",
                libc::O_PATH | libc::O_DIRECTORY | libc::O_NOFOLLOW,
                0,
            )?;
            let fd = openat(
                etc.as_raw_fd(),
                name,
                libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NOCTTY | libc::O_NONBLOCK,
                0,
            )?;
            let st = fstat(fd.as_raw_fd())?;
            if FileKind::from_mode(st.st_mode) != Some(FileKind::File)
                || st.st_size as u64 > MAX
                || !self.served(st.st_dev as u64)
            {
                return Err(Errno::ENOENT);
            }
            let mut buf = Vec::new();
            use std::io::Read;
            File::from(fd)
                .take(MAX)
                .read_to_end(&mut buf)
                .map_err(|e| io_errno(&e))?;
            Ok(buf)
        };
        read().unwrap_or_default()
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

/// libc's passwd/group lookups — on musl, the guest's `/etc/passwd`
/// and `/etc/group` by path plus an nscd daemon when one runs. Ahead of
/// [`users::Files`], which covers what this misses.
struct Libc;

impl users::Db for Libc {
    fn by_name(&self, name: &[u8]) -> Option<users::Entry> {
        let name = CString::new(name).ok()?;
        // SAFETY: every pointer comes from `passwd_lookup` and outlives the call.
        passwd_lookup(|pw, buf, len, out| unsafe {
            libc::getpwnam_r(name.as_ptr(), pw, buf, len, out)
        })
    }

    fn by_uid(&self, uid: u32) -> Option<users::Entry> {
        // SAFETY: every pointer comes from `passwd_lookup` and outlives the call.
        passwd_lookup(|pw, buf, len, out| unsafe { libc::getpwuid_r(uid, pw, buf, len, out) })
    }

    fn group(&self, name: &[u8]) -> Option<u32> {
        let name = CString::new(name).ok()?;
        let mut len = 1024;
        loop {
            let mut buf = vec![0 as libc::c_char; len];
            // SAFETY: `group` is plain data; zeroed is a valid start.
            let mut gr: libc::group = unsafe { std::mem::zeroed() };
            let mut out = std::ptr::null_mut();
            // SAFETY: `buf` is writable for `len` bytes; `gr`/`out` are live.
            let rc = unsafe {
                libc::getgrnam_r(name.as_ptr(), &mut gr, buf.as_mut_ptr(), len, &mut out)
            };
            if rc == libc::ERANGE && len < PASSWD_BUF_MAX {
                len *= 4;
                continue;
            }
            return (rc == 0 && !out.is_null()).then_some(gr.gr_gid);
        }
    }
}

/// Cap on the lookup scratch buffer (it grows on `ERANGE`).
const PASSWD_BUF_MAX: usize = 1 << 20;

/// Run `getpwnam_r`/`getpwuid_r` with a growing buffer; the entry it
/// found.
fn passwd_lookup(
    call: impl Fn(*mut libc::passwd, *mut libc::c_char, usize, *mut *mut libc::passwd) -> libc::c_int,
) -> Option<users::Entry> {
    let mut len = 1024;
    loop {
        let mut buf = vec![0 as libc::c_char; len];
        // SAFETY: `passwd` is plain data; zeroed is a valid start.
        let mut pw: libc::passwd = unsafe { std::mem::zeroed() };
        let mut out = std::ptr::null_mut();
        let rc = call(&mut pw, buf.as_mut_ptr(), len, &mut out);
        if rc == libc::ERANGE && len < PASSWD_BUF_MAX {
            len *= 4;
            continue;
        }
        if rc != 0 || out.is_null() {
            return None;
        }
        let home = if pw.pw_dir.is_null() {
            Vec::new()
        } else {
            // SAFETY: on success pw_dir points into `buf`, NUL-terminated.
            unsafe { CStr::from_ptr(pw.pw_dir) }.to_bytes().to_vec()
        };
        return Some(users::Entry {
            uid: pw.pw_uid,
            gid: pw.pw_gid,
            home,
        });
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
            Op::Copy {
                src,
                dst_parent,
                dst_name,
                flags,
            } => self.copy(src, dst_parent, &dst_name, flags),
            Op::CopyRange {
                src_fh,
                src_off,
                dst_fh,
                dst_off,
                len,
                flags,
            } => {
                if flags & !COPY_REQUIRE_SHARED != 0 {
                    return Err(Errno::EINVAL);
                }
                let (s, d) = (self.file(src_fh)?, self.file(dst_fh)?);
                let (n, shared) = copy_range(
                    &s,
                    src_off,
                    &d,
                    dst_off,
                    len,
                    flags & COPY_REQUIRE_SHARED != 0,
                )?;
                Ok(Reply::CopiedRange { n, shared })
            }
            Op::Seek { fh, offset, whence } => {
                let f = self.file(fh)?;
                let w = match whence {
                    SeekWhence::Data => libc::SEEK_DATA,
                    SeekWhence::Hole => libc::SEEK_HOLE,
                };
                match lseek(f.as_raw_fd(), offset, w) {
                    // EINVAL here means the fs can't report extents.
                    Err(Errno::EINVAL) => Err(Errno::EOPNOTSUPP),
                    r => r.map(Reply::Offset),
                }
            }
            Op::Fallocate {
                fh,
                mode,
                offset,
                len,
            } => {
                let f = self.file(fh)?;
                // SAFETY: args are a live fd and clamped off_t values.
                let rc = unsafe {
                    libc::syscall(
                        libc::SYS_fallocate,
                        f.as_raw_fd(),
                        falloc_mode(mode)?,
                        offset.min(i64::MAX as u64) as i64,
                        len.min(i64::MAX as u64) as i64,
                    )
                };
                if rc < 0 {
                    return Err(last_errno());
                }
                Ok(Reply::Ok)
            }
            Op::FsyncDir { node } => {
                let n = self.node(node)?;
                let fd = openat(n.raw(), c".", libc::O_RDONLY | libc::O_DIRECTORY, 0)?;
                File::from(fd).sync_all().map_err(|e| io_errno(&e))?;
                Ok(Reply::Ok)
            }
            Op::SyncFs { node } => {
                let n = self.node(node)?;
                // syncfs wants a real fd; the node's is O_PATH.
                let f = File::from(openat(
                    libc::AT_FDCWD,
                    &proc_path(n.raw()),
                    libc::O_RDONLY | libc::O_NOCTTY,
                    0,
                )?);
                // SAFETY: `f` is live.
                if unsafe { libc::syscall(libc::SYS_syncfs, f.as_raw_fd()) } < 0 {
                    return Err(last_errno());
                }
                Ok(Reply::Ok)
            }
            Op::GetXattr { node, name } => {
                let n = self.node(node)?;
                // A hidden mountpoint presents as an empty dir.
                if n.hidden {
                    return Err(Errno::ENODATA);
                }
                self.get_xattr(&n, &name)
            }
            Op::ListXattr { node } => {
                let n = self.node(node)?;
                if n.hidden {
                    return Ok(Reply::Names(Vec::new()));
                }
                self.list_xattr(&n)
            }
            Op::SetXattr {
                node,
                name,
                value,
                flags,
            } => {
                let n = self.node(node)?.writable()?;
                if value.len() > XATTR_VALUE_MAX || flags > 2 {
                    return Err(Errno::EINVAL);
                }
                let f = self.xattr_fd(&n)?;
                let xattr_flags = match flags {
                    0 => 0,
                    1 => libc::XATTR_CREATE,
                    _ => libc::XATTR_REPLACE,
                };
                // SAFETY: name/value are valid buffers for their lengths.
                cvt(unsafe {
                    libc::fsetxattr(
                        f.as_raw_fd(),
                        xattr_name(&name)?.as_ptr(),
                        value.as_ptr().cast(),
                        value.len(),
                        xattr_flags,
                    )
                })?;
                Ok(Reply::Ok)
            }
            Op::RemoveXattr { node, name } => {
                let n = self.node(node)?.writable()?;
                let f = self.xattr_fd(&n)?;
                // SAFETY: name is NUL-terminated; the fd is live.
                cvt(unsafe { libc::fremovexattr(f.as_raw_fd(), xattr_name(&name)?.as_ptr()) })?;
                Ok(Reply::Ok)
            }
            Op::LookupUser { user } => {
                let u = self.shared.lookup_user(&user)?;
                Ok(Reply::User {
                    uid: u.uid,
                    gid: u.gid,
                    home: u.home,
                })
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

    /// Whole-file copy into a fresh exclusive dentry; on any error the
    /// partial destination is unlinked.
    fn copy(&self, src: NodeId, dst_parent: NodeId, dst_name: &[u8], flags: u32) -> R<Reply> {
        if flags & !COPY_REQUIRE_SHARED != 0 {
            return Err(Errno::EINVAL);
        }
        let s = self.node(src)?;
        let p = self.node(dst_parent)?.writable()?;
        let c = cname(dst_name)?;
        let st = fstat(s.raw())?;
        if FileKind::from_mode(st.st_mode) != Some(FileKind::File) {
            return Err(Errno::EINVAL);
        }
        let df = File::from(openat(
            p.raw(),
            &c,
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_NOCTTY,
            st.st_mode & 0o7777,
        )?);
        match self.copy_fill(&s, &df, &st, flags) {
            Err(e) => {
                // SAFETY: best-effort removal of the partial copy.
                unsafe { libc::unlinkat(p.raw(), c.as_ptr(), 0) };
                Err(e)
            }
            Ok(shared) => {
                self.chown_new(&p, &c)?;
                let (node, attr) = self.lookup_fd(&p, &c)?;
                Ok(Reply::Copied { node, attr, shared })
            }
        }
    }

    /// Populate the fresh `df` from `s`'s inode — reflink where the fs
    /// allows, else an extent copy — then take the source's times.
    fn copy_fill(&self, s: &NodeRef, df: &File, st: &libc::stat, flags: u32) -> R<bool> {
        // Reopen the inode the O_PATH fd pins, not a path.
        let sf = File::from(openat(
            libc::AT_FDCWD,
            &proc_path(s.raw()),
            libc::O_RDONLY | libc::O_NOCTTY,
            0,
        )?);
        // SAFETY: both are live fds of regular files.
        let rc = unsafe { libc::ioctl(df.as_raw_fd(), FICLONE, sf.as_raw_fd()) };
        let shared = if rc == 0 {
            true
        } else {
            let e = last_errno();
            let try_copy = matches!(
                e,
                Errno::EINVAL | Errno::EOPNOTSUPP | Errno::ENOTTY | Errno::EXDEV
            );
            if !try_copy || flags & COPY_REQUIRE_SHARED != 0 {
                return Err(if try_copy { Errno::EOPNOTSUPP } else { e });
            }
            copy_range(&sf, 0, df, 0, st.st_size.max(0) as u64, false)?;
            false
        };
        let times = [
            libc::timespec {
                tv_sec: st.st_atime,
                tv_nsec: st.st_atime_nsec,
            },
            libc::timespec {
                tv_sec: st.st_mtime,
                tv_nsec: st.st_mtime_nsec,
            },
        ];
        // SAFETY: the proc path resolves to `df`'s inode; `times` has
        // both entries.
        cvt(unsafe {
            libc::utimensat(
                libc::AT_FDCWD,
                proc_path(df.as_raw_fd()).as_ptr(),
                times.as_ptr(),
                0,
            )
        })?;
        Ok(shared)
    }

    /// A real fd for `f*xattr` on a node — `O_PATH` fds can't carry
    /// them. `O_NONBLOCK` keeps a FIFO node from hanging on open.
    /// Symlinks refuse: a proc-path reopen would follow to the target.
    fn xattr_fd(&self, n: &NodeRef) -> R<File> {
        if FileKind::from_mode(fstat(n.raw())?.st_mode) == Some(FileKind::Symlink) {
            return Err(Errno::EOPNOTSUPP);
        }
        openat(
            libc::AT_FDCWD,
            &proc_path(n.raw()),
            libc::O_RDONLY | libc::O_NONBLOCK | libc::O_NOCTTY,
            0,
        )
        .map(File::from)
    }

    fn get_xattr(&self, n: &NodeRef, name: &[u8]) -> R<Reply> {
        let f = self.xattr_fd(n)?;
        let name = xattr_name(name)?;
        // SAFETY: name is NUL-terminated; null/0 is a size query.
        let sz = unsafe { libc::fgetxattr(f.as_raw_fd(), name.as_ptr(), std::ptr::null_mut(), 0) };
        if sz < 0 {
            return Err(last_errno());
        }
        if sz as usize > XATTR_VALUE_MAX {
            return Err(Errno::EIO);
        }
        let mut buf = vec![0u8; sz as usize];
        // SAFETY: `buf` is writable for its length.
        let n2 = unsafe {
            libc::fgetxattr(
                f.as_raw_fd(),
                name.as_ptr(),
                buf.as_mut_ptr().cast(),
                sz as usize,
            )
        };
        if n2 < 0 {
            return Err(last_errno());
        }
        buf.truncate(n2 as usize);
        Ok(Reply::Value(buf))
    }

    fn list_xattr(&self, n: &NodeRef) -> R<Reply> {
        let f = self.xattr_fd(n)?;
        // SAFETY: null/0 is a size query.
        let sz = unsafe { libc::flistxattr(f.as_raw_fd(), std::ptr::null_mut(), 0) };
        if sz < 0 {
            return Err(last_errno());
        }
        if sz as usize > 4 * XATTR_VALUE_MAX {
            return Err(Errno::EIO);
        }
        let mut buf = vec![0u8; sz as usize];
        // SAFETY: `buf` is writable for its length.
        let n2 = unsafe { libc::flistxattr(f.as_raw_fd(), buf.as_mut_ptr().cast(), sz as usize) };
        if n2 < 0 {
            return Err(last_errno());
        }
        buf.truncate(n2 as usize);
        Ok(Reply::Names(
            buf.split(|&b| b == 0)
                .filter(|s| !s.is_empty())
                .map(<[u8]>::to_vec)
                .collect(),
        ))
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
    Vsock(OwnedFd),
}

impl Listener {
    fn accept(&self) -> io::Result<Stream> {
        match self {
            Listener::Unix(l) => l.accept().map(|(s, _)| Stream::Unix(s)),
            Listener::Tcp(l) => l.accept().map(|(s, _)| Stream::Tcp(s)),
            Listener::Vsock(fd) => {
                // SAFETY: plain accept on a listening socket we own; the
                // peer address isn't wanted.
                let conn = unsafe {
                    libc::accept4(
                        fd.as_raw_fd(),
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                        libc::SOCK_CLOEXEC,
                    )
                };
                if conn < 0 {
                    return Err(io::Error::last_os_error());
                }
                // A vsock stream speaks the same read/write/shutdown/
                // SO_RCVTIMEO calls as a Unix one, so `UnixStream` serves
                // as the wrapper rather than a third `Stream` variant.
                // SAFETY: `conn` is a fresh fd nothing else owns.
                Ok(Stream::Unix(UnixStream::from(unsafe {
                    OwnedFd::from_raw_fd(conn)
                })))
            }
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

/// Listen on vsock `port` for any CID (the host dials in).
fn bind_vsock(port: u32) -> io::Result<OwnedFd> {
    // SAFETY: socket() has no memory preconditions.
    let fd = unsafe { libc::socket(libc::AF_VSOCK, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `fd` is a fresh socket nothing else owns.
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    // SAFETY: sockaddr_vm is plain data; all-zero is a valid start.
    let mut addr: libc::sockaddr_vm = unsafe { std::mem::zeroed() };
    addr.svm_family = libc::AF_VSOCK as libc::sa_family_t;
    addr.svm_cid = libc::VMADDR_CID_ANY;
    addr.svm_port = port;
    // SAFETY: `addr` is a valid sockaddr_vm for the length passed.
    let rc = unsafe {
        libc::bind(
            fd.as_raw_fd(),
            (&addr as *const libc::sockaddr_vm).cast(),
            std::mem::size_of::<libc::sockaddr_vm>() as libc::socklen_t,
        )
    };
    // SAFETY: listen() on a socket we own.
    if rc < 0 || unsafe { libc::listen(fd.as_raw_fd(), 16) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(fd)
}

/// The listener for `--socket`/`--listen`.
fn listen(spec: &Listen) -> io::Result<Listener> {
    match spec {
        Listen::Unix(path) => bind_unix(path).map(Listener::Unix),
        Listen::Tcp(addr) => TcpListener::bind(addr).map(Listener::Tcp),
        Listen::Vsock(port) => bind_vsock(*port).map(Listener::Vsock),
    }
}

/// Unlink the running binary and, if that empties it, its directory.
/// Best effort: a leftover copy is only clutter.
fn remove_own_binary() {
    let Ok(exe) = fs::read_link("/proc/self/exe") else {
        return;
    };
    if fs::remove_file(&exe).is_ok()
        && let Some(dir) = exe.parent()
    {
        let _ = fs::remove_dir(dir);
    }
}

/// One client: handshake, then requests fanned out to a worker pool
/// with responses written back in completion order. Returns whether
/// the peer proved the token.
fn session(mut s: Stream, token: &[u8], shared: Arc<Shared>) -> io::Result<bool> {
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
        return Ok(false);
    }
    let (conn, root) = Conn::new(shared).map_err(|e| io::Error::from_raw_os_error(e.0))?;
    write_frame(
        &mut s,
        &ServerAuth {
            mac: server_mac(token, &auth.nonce),
            caps: CAP_XATTR | CAP_COPY | CAP_SEEK | CAP_FALLOCATE | CAP_DIR_SYNC | CAP_USERS,
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
    Ok(true)
}

pub fn serve(args: &ServeArgs) -> io::Result<()> {
    let token_env = &args.token_env;
    let token = std::env::var(token_env)
        .ok()
        .and_then(|t| unhex(&t))
        .filter(|t| t.len() == 32)
        .ok_or_else(|| io::Error::other(format!("missing or malformed {token_env}")))?;
    // SAFETY: still single-threaded; nothing else reads the environment.
    unsafe { std::env::remove_var(token_env) };
    let token: Arc<[u8]> = token.into();

    let listener = listen(&args.listen)?;
    if args.remove_binary {
        remove_own_binary();
    }
    // The owner for host-created files: `--user` resolved by the guest's
    // own rules, then `--uid`/`--gid` overriding either half.
    let probe = Shared {
        uid: 0,
        gid: 0,
        mounts: Mutex::new(Shared::load_mounts()),
    };
    let (mut uid, mut gid) = (0, 0);
    if let Some(spec) = &args.user {
        let u = probe.lookup_user(spec.as_bytes()).map_err(|e| {
            io::Error::other(format!(
                "--user {spec}: {}",
                io::Error::from_raw_os_error(e.0)
            ))
        })?;
        (uid, gid) = (u.uid, u.gid);
    }
    let shared = Arc::new(Shared {
        uid: args.uid.unwrap_or(uid),
        gid: args.gid.unwrap_or(gid),
        mounts: probe.mounts,
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
        let once = args.once;
        let (token, shared, active, last) = (
            Arc::clone(&token),
            Arc::clone(&shared),
            Arc::clone(&active),
            Arc::clone(&last),
        );
        active.fetch_add(1, Ordering::SeqCst);
        thread::spawn(move || {
            match session(stream, &token, shared) {
                // Only an authenticated session counts, so a guest process
                // that connects first can't end the host's server.
                Ok(true) if once => std::process::exit(0),
                Ok(_) => {}
                Err(e) => eprintln!("container-mount: session ended: {e}"),
            }
            if let Ok(mut l) = last.lock() {
                *l = Instant::now();
            }
            active.fetch_sub(1, Ordering::SeqCst);
        });
    }
}
