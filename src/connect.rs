/*
 * Copyright (c) 2026 Daphne Pfister
 * SPDX-License-Identifier: BSD-2-Clause
 * See LICENSE file for full license text
 */

//! Reaching a container-mount server: transports, socket-path hygiene, and
//! peer identity.
//!
//! Whoever launches `serve` also decides how the host reaches it —
//! typically a VMM-relayed Unix socket (Apple `container`'s
//! `--publish-socket`, a Lima/Colima-style relay), an already-connected
//! fd handed down by an orchestrator, or a TCP port for engines that
//! can only publish those.
//!
//! There is no public `connectat`/`bindat` on macOS, so `connect(2)`
//! resolves a path and follows symlinks; the defence is layered
//! instead (docs/design/security.md):
//!
//! 1. the socket's directory may not be replaceable or its contents
//!    swapped by another user: opened `O_DIRECTORY | O_NOFOLLOW`, then
//!    either owned by us/root with no group/other write, or a sticky
//!    shared dir (`/tmp` shape) where non-owners can't rename entries;
//! 2. the leaf must be a socket owned by us or root
//!    (`AT_SYMLINK_NOFOLLOW`);
//! 3. after connecting, when a `--peer-requirement` is given, the
//!    peer's audit token must satisfy that code-signing requirement —
//!    which makes the check-then-connect race irrelevant. Which
//!    *server* answered is settled by the session handshake
//!    ([`crate::client`]).
//!
//! `socket-path KEY` hands callers a managed location under the
//! per-user temp dir (`<temp>/container-mount/s/<key-hash>.sock`), so an
//! embedder doesn't have to invent a safe layout itself.

use std::ffi::{CStr, CString};
use std::fs::File;
use std::io;
use std::net::{Shutdown, TcpStream};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

use crate::client::Client;
use crate::proto::hex;

/// `sun_path` is 104 bytes including the NUL.
const SUN_PATH_MAX: usize = 103;

/// Directory namespace for [`socket_path`] under the per-user temp dir.
const SOCKET_NS: &str = "container-mount";

/// How a client reaches the server.
#[derive(Debug, Clone, PartialEq)]
pub enum Target {
    /// Connect to a Unix socket at this path.
    Socket(PathBuf),
    /// Use an inherited, already-connected Unix socket fd.
    Fd(RawFd),
    /// Connect to `host:port` — for engines that can only publish TCP.
    Tcp(String),
}

/// A connected stream to a server: a Unix socket or a TCP connection.
pub enum Stream {
    Unix(UnixStream),
    Tcp(TcpStream),
}

impl Stream {
    pub fn try_clone(&self) -> io::Result<Stream> {
        Ok(match self {
            Stream::Unix(s) => Stream::Unix(s.try_clone()?),
            Stream::Tcp(s) => Stream::Tcp(s.try_clone()?),
        })
    }

    pub fn set_read_timeout(&self, d: Option<Duration>) -> io::Result<()> {
        match self {
            Stream::Unix(s) => s.set_read_timeout(d),
            Stream::Tcp(s) => s.set_read_timeout(d),
        }
    }

    pub fn shutdown(&self) -> io::Result<()> {
        match self {
            Stream::Unix(s) => s.shutdown(Shutdown::Both),
            Stream::Tcp(s) => s.shutdown(Shutdown::Both),
        }
    }
}

impl io::Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Stream::Unix(s) => s.read(buf),
            Stream::Tcp(s) => s.read(buf),
        }
    }
}

impl io::Write for Stream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Stream::Unix(s) => s.write(buf),
            Stream::Tcp(s) => s.write(buf),
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        match self {
            Stream::Unix(s) => s.flush(),
            Stream::Tcp(s) => s.flush(),
        }
    }
}

fn cvt(rc: libc::c_int) -> io::Result<libc::c_int> {
    if rc < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(rc)
    }
}

fn cstr(s: &[u8]) -> io::Result<CString> {
    CString::new(s).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))
}

/// `confstr(_CS_DARWIN_USER_TEMP_DIR)` — `/var/folders/…/T/`.
#[cfg(target_os = "macos")]
fn darwin_user_temp() -> Option<PathBuf> {
    let mut buf = vec![0u8; 1024];
    // SAFETY: `buf` is writable for its length.
    let n = unsafe {
        libc::confstr(
            libc::_CS_DARWIN_USER_TEMP_DIR,
            buf.as_mut_ptr().cast(),
            buf.len(),
        )
    };
    if n == 0 || n > buf.len() {
        return None;
    }
    let s = CStr::from_bytes_until_nul(&buf).ok()?;
    Some(PathBuf::from(std::ffi::OsStr::from_bytes(s.to_bytes())))
}

/// The base directory managed socket paths live under: the per-user
/// Darwin temp dir on macOS, `$XDG_RUNTIME_DIR` elsewhere, `/tmp` as
/// the last resort (a sticky shared dir is fine — we create a private
/// `0700` namespace inside it).
fn user_temp_base() -> io::Result<PathBuf> {
    #[cfg(target_os = "macos")]
    if let Some(p) = darwin_user_temp() {
        return Ok(p);
    }
    if let Some(p) = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
    {
        return Ok(p);
    }
    let tmp = std::env::temp_dir();
    if tmp.as_os_str().is_empty() {
        return Err(io::Error::other("cannot resolve a temporary directory"));
    }
    Ok(tmp)
}

/// `fstat` an already-open fd.
fn fstat_fd(fd: RawFd) -> io::Result<libc::stat> {
    // SAFETY: `st` is a valid out-param.
    let mut st = unsafe { std::mem::zeroed::<libc::stat>() };
    cvt(unsafe { libc::fstat(fd, &mut st) })?;
    Ok(st)
}

/// Whether a directory holding a socket is safe to use: sockets inside
/// it can't be swapped out from under a connecting client by an
/// unrelated user. True when no group/other write bit is set, or when
/// the directory is sticky (`/tmp`-style — non-owners can create but
/// never rename or remove entries).
fn dir_keeps_leaf(st: &libc::stat) -> bool {
    st.st_mode & libc::S_IFMT == libc::S_IFDIR
        && (st.st_mode & 0o022 == 0 || st.st_mode & 0o1000 != 0)
}

/// The leaf itself must be a socket owned by us or root.
fn leaf_ok(st: &libc::stat) -> bool {
    // SAFETY: getuid cannot fail.
    st.st_mode & libc::S_IFMT == libc::S_IFSOCK
        && (st.st_uid == unsafe { libc::getuid() } || st.st_uid == 0)
}

/// Open `fd/name` as a directory without following a final symlink,
/// and require it to be ours with no group/other access — the rule for
/// directories *we* create and populate (managed socket dirs).
fn open_private_dir(dirfd: RawFd, name: &CStr) -> io::Result<OwnedFd> {
    // SAFETY: `name` is NUL-terminated; a non-negative return is ours.
    let fd = cvt(unsafe {
        libc::openat(
            dirfd,
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    })?;
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    let meta = File::from(fd.try_clone()?).metadata()?;
    use std::os::unix::fs::MetadataExt;
    // SAFETY: getuid cannot fail.
    if meta.uid() != unsafe { libc::getuid() } || meta.mode() & 0o077 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "{} is not a private directory of this user",
                name.to_string_lossy()
            ),
        ));
    }
    Ok(fd)
}

/// Whether `base` is an acceptable root for managed dirs: private to
/// us, or a root-owned sticky shared dir (`/tmp`) we can create a
/// private namespace inside.
fn base_ok(st: &libc::stat) -> bool {
    // SAFETY: getuid cannot fail.
    let uid = unsafe { libc::getuid() };
    (st.st_uid == uid && st.st_mode & 0o077 == 0)
        || (st.st_uid == 0 && st.st_mode & 0o7777 == 0o1777)
}

/// Create (if needed) and verify `base/<SOCKET_NS>/s`, returning its
/// path. `base` must satisfy [`base_ok`]; the two managed components
/// must be ours and `0700`.
fn private_socket_dir(base: &Path) -> io::Result<PathBuf> {
    let c = cstr(base.as_os_str().as_bytes())?;
    // SAFETY: `c` is NUL-terminated.
    let fd = cvt(unsafe {
        libc::open(
            c.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    })?;
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    let st = fstat_fd(fd.as_raw_fd())?;
    if !base_ok(&st) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "{} is neither a private directory of this user nor a sticky shared temp dir",
                base.display()
            ),
        ));
    }
    let mut fd = fd;
    for comp in [SOCKET_NS, "s"] {
        let c = cstr(comp.as_bytes())?;
        // SAFETY: `c` is NUL-terminated; fd is live. EEXIST is fine —
        // the open below verifies whatever is there.
        if unsafe { libc::mkdirat(fd.as_raw_fd(), c.as_ptr(), 0o700) } < 0 {
            let e = io::Error::last_os_error();
            if e.raw_os_error() != Some(libc::EEXIST) {
                return Err(e);
            }
        }
        fd = open_private_dir(fd.as_raw_fd(), &c)?;
    }
    Ok(base.join(SOCKET_NS).join("s"))
}

/// The managed host socket path for `key`:
/// `<user temp>/container-mount/s/<16 hex of SHA-256(key)>.sock`.
/// Creates and verifies the private directory chain; fails if the
/// path would exceed `sun_path`.
pub fn socket_path(key: &str) -> Result<PathBuf> {
    let base = user_temp_base().context("can't resolve a per-user temporary directory")?;
    let dir = private_socket_dir(&base)
        .with_context(|| format!("unsafe socket directory under {}", base.display()))?;
    let path = dir.join(format!(
        "{}.sock",
        &hex(&Sha256::digest(key.as_bytes()))[..16]
    ));
    if path.as_os_str().len() > SUN_PATH_MAX {
        bail!(
            "socket path {} exceeds the {SUN_PATH_MAX}-byte Unix socket limit",
            path.display()
        );
    }
    Ok(path)
}

/// Connect to a Unix socket after checking the directory it lives in
/// keeps the leaf stable ([`dir_keeps_leaf`]) and the leaf is a socket
/// owned by us or root. Defence in depth — the peer check after connect
/// is what makes a residual race irrelevant.
fn unix_transport(path: &Path) -> io::Result<UnixStream> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "socket path has no directory")
        })?;
    let c = cstr(parent.as_os_str().as_bytes())?;
    // SAFETY: `c` is NUL-terminated; a non-negative return is ours.
    let dirfd = cvt(unsafe {
        libc::open(
            c.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    })?;
    let dirfd = unsafe { OwnedFd::from_raw_fd(dirfd) };
    let st = fstat_fd(dirfd.as_raw_fd())?;
    if !dir_keeps_leaf(&st) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "{} is writable by other users without a sticky bit — \
                 a socket there can be swapped between check and connect",
                parent.display()
            ),
        ));
    }
    let name = cstr(
        path.file_name()
            .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?
            .as_bytes(),
    )?;
    // SAFETY: `st` is a valid out-param; `name` is NUL-terminated.
    let mut st = unsafe { std::mem::zeroed::<libc::stat>() };
    cvt(unsafe {
        libc::fstatat(
            dirfd.as_raw_fd(),
            name.as_ptr(),
            &mut st,
            libc::AT_SYMLINK_NOFOLLOW,
        )
    })?;
    if !leaf_ok(&st) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "{} is not a socket owned by this user or root",
                path.display()
            ),
        ));
    }
    UnixStream::connect(path)
}

/// Adopt an inherited fd: it must already be a socket.
fn fd_transport(fd: RawFd) -> io::Result<UnixStream> {
    let st = fstat_fd(fd)?;
    if st.st_mode & libc::S_IFMT != libc::S_IFSOCK {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("fd {fd} is not a socket"),
        ));
    }
    // SAFETY: the caller passed ownership of a live socket fd.
    Ok(unsafe { UnixStream::from(OwnedFd::from_raw_fd(fd)) })
}

/// Open `target`, without the peer check — hygiene failures come back
/// as `PermissionDenied` so [`open`] can treat them as final.
fn transport(target: &Target) -> io::Result<Stream> {
    match target {
        Target::Socket(p) => unix_transport(p).map(Stream::Unix),
        Target::Fd(fd) => fd_transport(*fd).map(Stream::Unix),
        Target::Tcp(addr) => TcpStream::connect(addr).map(Stream::Tcp),
    }
}

/// Whether a [`Target`] is Unix-socket based (peer checks apply).
fn is_unix(target: &Target) -> bool {
    matches!(target, Target::Socket(_) | Target::Fd(_))
}

/// Connect and handshake, retrying until `timeout` elapses. The relay
/// accepts before a fresh server listens, and an older server instance
/// may still own the socket path briefly ("last starter wins" — new
/// connects converge on the newest instance), so connect/handshake
/// failures retry; trust failures — unsafe path, peer check — are final.
pub fn open(
    target: &Target,
    token: &[u8; 32],
    peer_requirement: Option<&str>,
    timeout: Duration,
) -> Result<Client> {
    if peer_requirement.is_some() && !is_unix(target) {
        bail!("--peer-requirement applies to Unix socket transports");
    }
    let deadline = Instant::now() + timeout;
    loop {
        let attempt = (|| -> std::result::Result<Client, anyhow::Error> {
            let stream = transport(target).map_err(anyhow::Error::from)?;
            if let Some(req) = peer_requirement {
                verify_peer(&stream, req)?;
            }
            Client::handshake(stream, token).map_err(anyhow::Error::new)
        })();
        match attempt {
            Ok(client) => return Ok(client),
            Err(e) => {
                let trust = e
                    .downcast_ref::<io::Error>()
                    .is_some_and(|io| io.kind() == io::ErrorKind::PermissionDenied);
                if trust || Instant::now() >= deadline {
                    return Err(e).context("couldn't open a container-mount session");
                }
            }
        }
        thread::sleep(Duration::from_millis(100));
    }
}

#[cfg(target_os = "macos")]
const SOL_LOCAL: libc::c_int = 0;
#[cfg(target_os = "macos")]
const LOCAL_PEERTOKEN: libc::c_int = 6;

/// The connected peer's audit token (`LOCAL_PEERTOKEN`).
#[cfg(target_os = "macos")]
fn peer_audit_token(stream: &Stream) -> io::Result<[u8; 32]> {
    let mut token = [0u8; 32];
    let mut len = token.len() as libc::socklen_t;
    // SAFETY: `token` is writable for `len` bytes.
    cvt(unsafe {
        libc::getsockopt(
            match stream {
                Stream::Unix(s) => s.as_raw_fd(),
                Stream::Tcp(s) => s.as_raw_fd(),
            },
            SOL_LOCAL,
            LOCAL_PEERTOKEN,
            token.as_mut_ptr().cast(),
            &mut len,
        )
    })?;
    if len as usize != token.len() {
        return Err(io::Error::other("short audit token"));
    }
    Ok(token)
}

/// Require the process on the other end of `stream` to satisfy the
/// code requirement `requirement` — checked against the live process
/// named by its audit token, so pid reuse can't substitute another.
#[cfg(target_os = "macos")]
pub fn verify_peer(stream: &Stream, requirement: &str) -> Result<()> {
    let token = peer_audit_token(stream).context("can't read the socket peer's audit token")?;
    let status = sec::check_audit_token(&token, requirement);
    if status != 0 {
        // PermissionDenied so `open` treats a failed peer check as
        // final — retrying can't change the peer's signature.
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "socket peer failed its code-signature check (OSStatus {status}): {requirement}"
            ),
        )
        .into());
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
pub fn verify_peer(_stream: &Stream, _requirement: &str) -> Result<()> {
    bail!("--peer-requirement is only supported on macOS")
}

/// Minimal Security.framework / CoreFoundation bindings for one check.
#[cfg(target_os = "macos")]
mod sec {
    use std::os::raw::c_void;

    type CFTypeRef = *const c_void;
    type OSStatus = i32;

    #[repr(C)]
    struct Opaque {
        _p: [u8; 0],
    }

    const UTF8: u32 = 0x0800_0100;
    const ERR_PARAM: OSStatus = -50;

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        static kCFTypeDictionaryKeyCallBacks: Opaque;
        static kCFTypeDictionaryValueCallBacks: Opaque;
        fn CFDataCreate(alloc: CFTypeRef, bytes: *const u8, len: isize) -> CFTypeRef;
        fn CFStringCreateWithBytes(
            alloc: CFTypeRef,
            bytes: *const u8,
            len: isize,
            enc: u32,
            bom: u8,
        ) -> CFTypeRef;
        fn CFDictionaryCreate(
            alloc: CFTypeRef,
            keys: *const CFTypeRef,
            values: *const CFTypeRef,
            n: isize,
            key_cb: *const Opaque,
            value_cb: *const Opaque,
        ) -> CFTypeRef;
        fn CFRelease(cf: CFTypeRef);
    }

    #[link(name = "Security", kind = "framework")]
    unsafe extern "C" {
        static kSecGuestAttributeAudit: CFTypeRef;
        fn SecCodeCopyGuestWithAttributes(
            host: CFTypeRef,
            attrs: CFTypeRef,
            flags: u32,
            guest: *mut CFTypeRef,
        ) -> OSStatus;
        fn SecRequirementCreateWithString(
            text: CFTypeRef,
            flags: u32,
            req: *mut CFTypeRef,
        ) -> OSStatus;
        fn SecCodeCheckValidity(code: CFTypeRef, flags: u32, req: CFTypeRef) -> OSStatus;
    }

    /// Releases a CF object on drop.
    struct Cf(CFTypeRef);

    impl Drop for Cf {
        fn drop(&mut self) {
            if !self.0.is_null() {
                // SAFETY: we own one reference to a live CF object.
                unsafe { CFRelease(self.0) };
            }
        }
    }

    /// `errSecSuccess` (0) when the process named by `token` satisfies
    /// `requirement`; otherwise the failing OSStatus.
    pub fn check_audit_token(token: &[u8; 32], requirement: &str) -> OSStatus {
        // SAFETY: every pointer passed is valid for the stated length,
        // CF objects are released by `Cf`, and out-params are checked
        // for success before use.
        unsafe {
            let data = Cf(CFDataCreate(
                std::ptr::null(),
                token.as_ptr(),
                token.len() as isize,
            ));
            let text = Cf(CFStringCreateWithBytes(
                std::ptr::null(),
                requirement.as_ptr(),
                requirement.len() as isize,
                UTF8,
                0,
            ));
            if data.0.is_null() || text.0.is_null() {
                return ERR_PARAM;
            }
            let keys = [kSecGuestAttributeAudit];
            let values = [data.0];
            let attrs = Cf(CFDictionaryCreate(
                std::ptr::null(),
                keys.as_ptr(),
                values.as_ptr(),
                1,
                &raw const kCFTypeDictionaryKeyCallBacks,
                &raw const kCFTypeDictionaryValueCallBacks,
            ));
            if attrs.0.is_null() {
                return ERR_PARAM;
            }
            let mut code: CFTypeRef = std::ptr::null();
            let s = SecCodeCopyGuestWithAttributes(std::ptr::null(), attrs.0, 0, &mut code);
            let code = Cf(code);
            if s != 0 {
                return s;
            }
            let mut req: CFTypeRef = std::ptr::null();
            let s = SecRequirementCreateWithString(text.0, 0, &mut req);
            let req = Cf(req);
            if s != 0 {
                return s;
            }
            SecCodeCheckValidity(code.0, 0, req.0)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::UnixListener;

    fn scratch() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        std::fs::set_permissions(d.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        d
    }

    #[test]
    fn test_socket_path_is_short_private_and_stable() {
        let a = socket_path("some-key-0123456789").unwrap();
        let b = socket_path("some-key-0123456789").unwrap();
        assert_eq!(a, b);
        assert_ne!(a, socket_path("other-key").unwrap());
        assert!(a.as_os_str().len() <= SUN_PATH_MAX, "{}", a.display());
        assert!(a.parent().unwrap().ends_with("container-mount/s"));
    }

    #[test]
    fn test_private_socket_dir_creates_and_accepts_its_own_chain() {
        let d = scratch();
        let p = private_socket_dir(d.path()).unwrap();
        assert_eq!(p, d.path().join(SOCKET_NS).join("s"));
        // Idempotent.
        private_socket_dir(d.path()).unwrap();
    }

    #[test]
    fn test_private_socket_dir_rejects_open_modes_and_symlinks() {
        let d = scratch();
        std::fs::create_dir(d.path().join(SOCKET_NS)).unwrap();
        std::fs::set_permissions(
            d.path().join(SOCKET_NS),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        assert!(
            private_socket_dir(d.path()).is_err(),
            "group/other access must be refused"
        );

        let d = scratch();
        let elsewhere = scratch();
        std::os::unix::fs::symlink(elsewhere.path(), d.path().join(SOCKET_NS)).unwrap();
        assert!(
            private_socket_dir(d.path()).is_err(),
            "a planted symlink must not be followed"
        );

        let open = tempfile::tempdir().unwrap();
        std::fs::set_permissions(open.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(
            private_socket_dir(open.path()).is_err(),
            "the base must be private or sticky-shared"
        );
    }

    #[test]
    fn test_unix_transport_rejects_swappable_dirs_and_bad_leaves() {
        // A dir others can't write to is fine to hold a socket; the
        // connect itself succeeds against the listener's backlog.
        let d = scratch();
        let path = d.path().join("x.sock");
        let _listener = UnixListener::bind(&path).unwrap();
        unix_transport(&path).unwrap();

        // A missing leaf fails.
        assert!(unix_transport(&d.path().join("none.sock")).is_err());
        // A regular file isn't a socket.
        std::fs::write(d.path().join("f"), b"x").unwrap();
        assert!(unix_transport(&d.path().join("f")).is_err());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn test_verify_peer_rejects_a_random_listener() {
        let d = scratch();
        let path = d.path().join("x.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let stream = Stream::Unix(UnixStream::connect(&path).unwrap());
        let _server = listener.accept().unwrap();
        let e = verify_peer(
            &stream,
            "identifier \"com.apple.container.container-runtime-linux\" and anchor apple generic",
        )
        .unwrap_err();
        assert!(e.to_string().contains("code-signature check"), "{e}");
    }
}
