/*
 * Copyright (c) 2026 Daphne Pfister
 * SPDX-License-Identifier: BSD-2-Clause
 * See LICENSE file for full license text
 */

//! The distrofs wire protocol (docs/design/distrofs.md).
//!
//! Shared by the host client ([`crate::client`]) and the guest server
//! (the `distrofs serve` subcommand), so it may depend only on `std`,
//! `serde`, `postcard`, and `sha2`.
//!
//! Framing is a `u32` little-endian length followed by a postcard
//! payload. A session opens with a mutual HMAC handshake
//! ([`ServerHello`] → [`ClientAuth`] → [`ServerAuth`]) over a
//! per-instance token; afterwards the client sends [`Request`]s and the
//! server answers with [`ServerMsg`]s, out of order.
//!
//! Enum variants are encoded by position: append new ones, never
//! reorder or insert.

use std::fs::File;
use std::io::{self, Read, Write};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Protocol major version; peers reject any other.
pub const VERSION: u16 = 1;
/// First bytes of [`ServerHello`] — rejects non-distrofs listeners early.
pub const MAGIC: [u8; 8] = *b"distrofs";
/// Upper bound on one frame's payload, checked before allocating.
pub const MAX_FRAME: usize = 16 << 20;
/// Upper bound on one `Read`/`Write` payload.
pub const MAX_IO: u32 = 1 << 20;
/// Longest single path component (Linux `NAME_MAX`).
pub const NAME_MAX: usize = 255;
/// Upper bound on a symlink target (Linux `PATH_MAX`).
pub const TARGET_MAX: usize = 4096;
/// The node id of the guest root directory.
pub const ROOT: NodeId = 1;
/// Environment variable carrying the hex session token — the handshake
/// contract between whoever launches `serve` and the client.
pub const TOKEN_ENV: &str = "DISTROFS_TOKEN";
/// Environment variable holding a default socket path for `serve` and
/// the client commands (a `--socket`/`--listen` flag overrides it).
pub const SOCKET_ENV: &str = "DISTROFS_SOCKET";
/// The server's default socket path inside a guest — on the rootfs
/// itself, not a container-namespace tmpfs, because VM-level relays
/// (vminitd-style) resolve it above the guest mount namespace. Deployments
/// choose their own path and pass `--socket`/`--listen`; this default is
/// for standalone use.
pub const DEFAULT_SOCKET: &str = "/var/lib/distrofs/distrofs.sock";

/// `Rename` flag: fail if the destination exists.
pub const RENAME_NOREPLACE: u32 = 1;
/// `Rename` flag: atomically swap source and destination.
pub const RENAME_EXCHANGE: u32 = 2;

pub type NodeId = u64;
pub type FileHandle = u64;
pub type Nonce = [u8; 32];
pub type Mac = [u8; 32];

/// A Linux errno value. The host maps it with [`Errno::to_darwin`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Errno(pub i32);

impl Errno {
    pub const EPERM: Errno = Errno(1);
    pub const ENOENT: Errno = Errno(2);
    pub const EIO: Errno = Errno(5);
    pub const EBADF: Errno = Errno(9);
    pub const EACCES: Errno = Errno(13);
    pub const EEXIST: Errno = Errno(17);
    pub const ENOTDIR: Errno = Errno(20);
    pub const EISDIR: Errno = Errno(21);
    pub const EINVAL: Errno = Errno(22);
    pub const EROFS: Errno = Errno(30);
    pub const ENAMETOOLONG: Errno = Errno(36);
    pub const ENOTEMPTY: Errno = Errno(39);
    pub const ELOOP: Errno = Errno(40);
    pub const EOPNOTSUPP: Errno = Errno(95);
    pub const ETIMEDOUT: Errno = Errno(110);

    /// The Darwin errno for this Linux value. 1–34 agree except
    /// `EAGAIN`; above that the numbering diverges, so known values are
    /// mapped and the rest become `EIO`.
    #[must_use]
    pub fn to_darwin(self) -> i32 {
        match self.0 {
            11 => 35,
            n @ 1..=34 => n,
            35 => 11,  // EDEADLK
            36 => 63,  // ENAMETOOLONG
            37 => 77,  // ENOLCK
            38 => 78,  // ENOSYS
            39 => 66,  // ENOTEMPTY
            40 => 62,  // ELOOP
            61 => 93,  // ENODATA → ENOATTR (what xattr callers expect)
            71 => 100, // EPROTO
            75 => 84,  // EOVERFLOW
            95 => 45,  // EOPNOTSUPP → ENOTSUP
            110 => 60, // ETIMEDOUT
            116 => 70, // ESTALE
            122 => 69, // EDQUOT
            _ => 5,
        }
    }
}

impl std::fmt::Display for Errno {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "guest errno {}", self.0)
    }
}

impl std::error::Error for Errno {}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FileKind {
    File,
    Dir,
    Symlink,
    Char,
    Block,
    Fifo,
    Socket,
}

impl FileKind {
    /// The kind encoded in a `st_mode`'s `S_IFMT` bits (Linux and Darwin
    /// agree on these), or `None` for an unknown type.
    #[must_use]
    pub fn from_mode(mode: u32) -> Option<FileKind> {
        Some(match mode & 0o170000 {
            0o100000 => FileKind::File,
            0o040000 => FileKind::Dir,
            0o120000 => FileKind::Symlink,
            0o020000 => FileKind::Char,
            0o060000 => FileKind::Block,
            0o010000 => FileKind::Fifo,
            0o140000 => FileKind::Socket,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Time {
    pub sec: i64,
    pub nsec: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attr {
    pub ino: u64,
    pub kind: FileKind,
    /// Permission bits only (`0o7777`); the type is in `kind`.
    pub perm: u32,
    pub nlink: u64,
    pub uid: u32,
    pub gid: u32,
    pub rdev: u64,
    pub size: u64,
    pub blocks: u64,
    pub blksize: u32,
    pub atime: Time,
    pub mtime: Time,
    pub ctime: Time,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TimeSet {
    Now,
    At(Time),
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetAttr {
    pub perm: Option<u32>,
    pub uid: Option<u32>,
    pub gid: Option<u32>,
    pub size: Option<u64>,
    pub atime: Option<TimeSet>,
    pub mtime: Option<TimeSet>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenFlags {
    pub read: bool,
    pub write: bool,
    pub truncate: bool,
    pub append: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirEntry {
    pub name: Vec<u8>,
    /// Pass back as `ReadDir.cookie` to continue after this entry.
    pub cookie: u64,
    pub attr: Attr,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatFs {
    pub bsize: u32,
    pub blocks: u64,
    pub bfree: u64,
    pub bavail: u64,
    pub files: u64,
    pub ffree: u64,
    pub namemax: u32,
}

/// One client operation. Names are single path components (see
/// [`valid_name`]); every resolution is relative to a node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Op {
    Lookup {
        parent: NodeId,
        name: Vec<u8>,
    },
    /// Drop `n` lookup references (each `Entry`/`Created` reply adds one).
    Forget {
        node: NodeId,
        n: u64,
    },
    GetAttr {
        node: NodeId,
    },
    SetAttr {
        node: NodeId,
        set: SetAttr,
    },
    /// Entries after `cookie` (0 = from the start), `.`/`..` omitted.
    ReadDir {
        node: NodeId,
        cookie: u64,
        max_entries: u32,
    },
    Open {
        node: NodeId,
        flags: OpenFlags,
    },
    Read {
        fh: FileHandle,
        offset: u64,
        len: u32,
    },
    Write {
        fh: FileHandle,
        offset: u64,
        data: Vec<u8>,
    },
    Fsync {
        fh: FileHandle,
    },
    Release {
        fh: FileHandle,
    },
    Create {
        parent: NodeId,
        name: Vec<u8>,
        perm: u32,
        exclusive: bool,
        flags: OpenFlags,
    },
    Mkdir {
        parent: NodeId,
        name: Vec<u8>,
        perm: u32,
    },
    Unlink {
        parent: NodeId,
        name: Vec<u8>,
    },
    Rmdir {
        parent: NodeId,
        name: Vec<u8>,
    },
    Rename {
        parent: NodeId,
        name: Vec<u8>,
        new_parent: NodeId,
        new_name: Vec<u8>,
        flags: u32,
    },
    Symlink {
        parent: NodeId,
        name: Vec<u8>,
        target: Vec<u8>,
    },
    ReadLink {
        node: NodeId,
    },
    Link {
        node: NodeId,
        new_parent: NodeId,
        new_name: Vec<u8>,
    },
    StatFs {
        node: NodeId,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Reply {
    Ok,
    Entry {
        node: NodeId,
        attr: Attr,
    },
    Attr(Attr),
    Entries {
        entries: Vec<DirEntry>,
        eof: bool,
    },
    Opened {
        fh: FileHandle,
    },
    Data(Vec<u8>),
    Written(u32),
    Created {
        node: NodeId,
        attr: Attr,
        fh: FileHandle,
    },
    Target(Vec<u8>),
    StatFs(StatFs),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Request {
    pub id: u64,
    pub op: Op,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Response {
    pub id: u64,
    pub result: Result<Reply, Errno>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Invalidate {
    Attr,
    Data,
    Entries,
    Gone,
}

/// Server-pushed notifications (planned: inotify → cache invalidation).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Event {
    Invalidate { node: NodeId, what: Invalidate },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ServerMsg {
    Response(Response),
    Event(Event),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerHello {
    pub magic: [u8; 8],
    pub version: u16,
    pub nonce: Nonce,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientAuth {
    pub version: u16,
    pub nonce: Nonce,
    /// [`client_mac`] over the server's nonce.
    pub mac: Mac,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerAuth {
    /// [`server_mac`] over the client's nonce.
    pub mac: Mac,
    /// Capability bits; none defined in v1.
    pub caps: u64,
    pub root: Attr,
}

/// Serialize `msg` as one frame and write it with a single `write_all`,
/// so writers sharing a stream under a lock never interleave.
pub fn write_frame<W: Write, T: Serialize>(w: &mut W, msg: &T) -> io::Result<()> {
    let body =
        postcard::to_stdvec(msg).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    if body.len() > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "distrofs frame too large",
        ));
    }
    let mut buf = Vec::with_capacity(4 + body.len());
    buf.extend_from_slice(&(body.len() as u32).to_le_bytes());
    buf.extend_from_slice(&body);
    w.write_all(&buf)?;
    w.flush()
}

/// Read one frame, refusing oversized lengths before allocating.
pub fn read_frame<R: Read, T: DeserializeOwned>(r: &mut R) -> io::Result<T> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len)?;
    let len = u32::from_le_bytes(len) as usize;
    if len > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "distrofs frame too large",
        ));
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body)?;
    postcard::from_bytes(&body).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// Whether `name` is a single, ordinary path component: non-empty, at
/// most [`NAME_MAX`] bytes, no `/` or NUL, and not `.`/`..`.
#[must_use]
pub fn valid_name(name: &[u8]) -> bool {
    !name.is_empty()
        && name.len() <= NAME_MAX
        && name != b"."
        && name != b".."
        && !name.iter().any(|&b| b == b'/' || b == 0)
}

/// HMAC-SHA256 (RFC 2104) over the concatenation of `parts`.
#[must_use]
pub fn hmac_sha256(key: &[u8], parts: &[&[u8]]) -> Mac {
    let mut k = [0u8; 64];
    if key.len() > k.len() {
        k[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let mut inner = Sha256::new();
    inner.update(k.map(|b| b ^ 0x36));
    for p in parts {
        inner.update(p);
    }
    let inner = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(k.map(|b| b ^ 0x5c));
    outer.update(inner);
    outer.finalize().into()
}

/// The client's proof of the token, bound to the server's nonce.
#[must_use]
pub fn client_mac(token: &[u8], server_nonce: &Nonce) -> Mac {
    hmac_sha256(token, &[b"distrofs/c", server_nonce])
}

/// The server's proof of the token, bound to the client's nonce.
#[must_use]
pub fn server_mac(token: &[u8], client_nonce: &Nonce) -> Mac {
    hmac_sha256(token, &[b"distrofs/s", client_nonce])
}

/// Constant-time equality for MACs.
#[must_use]
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// 32 bytes from the kernel CSPRNG (`/dev/urandom` exists on both ends).
pub fn random32() -> io::Result<[u8; 32]> {
    let mut b = [0u8; 32];
    File::open("/dev/urandom")?.read_exact(&mut b)?;
    Ok(b)
}

#[must_use]
pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Decode lowercase or uppercase hex; `None` on odd length or bad digits.
#[must_use]
pub fn unhex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attr() -> Attr {
        Attr {
            ino: 42,
            kind: FileKind::Dir,
            perm: 0o755,
            nlink: 2,
            uid: 501,
            gid: 20,
            rdev: 0,
            size: 4096,
            blocks: 8,
            blksize: 4096,
            atime: Time { sec: 1, nsec: 2 },
            mtime: Time { sec: 3, nsec: 4 },
            ctime: Time { sec: -5, nsec: 6 },
        }
    }

    #[test]
    fn test_frame_round_trips_every_message_shape() {
        let msgs = vec![
            ServerMsg::Response(Response {
                id: 7,
                result: Ok(Reply::Entries {
                    entries: vec![DirEntry {
                        name: b"caf\xc3\xa9\xff".to_vec(),
                        cookie: 9,
                        attr: attr(),
                    }],
                    eof: true,
                }),
            }),
            ServerMsg::Response(Response {
                id: 8,
                result: Err(Errno::ENOENT),
            }),
            ServerMsg::Event(Event::Invalidate {
                node: 3,
                what: Invalidate::Gone,
            }),
        ];
        let mut buf = Vec::new();
        for m in &msgs {
            write_frame(&mut buf, m).unwrap();
        }
        let mut r = &buf[..];
        for m in &msgs {
            assert_eq!(&read_frame::<_, ServerMsg>(&mut r).unwrap(), m);
        }
        assert!(
            read_frame::<_, ServerMsg>(&mut r).is_err(),
            "EOF after the last frame"
        );

        let req = Request {
            id: 1,
            op: Op::SetAttr {
                node: 2,
                set: SetAttr {
                    perm: Some(0o644),
                    mtime: Some(TimeSet::Now),
                    ..SetAttr::default()
                },
            },
        };
        let mut buf = Vec::new();
        write_frame(&mut buf, &req).unwrap();
        assert_eq!(read_frame::<_, Request>(&mut &buf[..]).unwrap(), req);
    }

    #[test]
    fn test_read_frame_rejects_oversized_length_before_allocating() {
        let buf = ((MAX_FRAME + 1) as u32).to_le_bytes();
        let e = read_frame::<_, Request>(&mut &buf[..]).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn test_read_frame_rejects_garbage_payload() {
        let mut buf = 3u32.to_le_bytes().to_vec();
        buf.extend_from_slice(&[0xff, 0xff, 0xff]);
        assert!(read_frame::<_, Request>(&mut &buf[..]).is_err());
    }

    #[test]
    fn test_hmac_sha256_rfc4231_vectors() {
        // RFC 4231 test case 2.
        assert_eq!(
            hex(&hmac_sha256(
                b"Jefe",
                &[b"what do ya want ", b"for nothing?"]
            )),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
        // Test case 6: a key longer than the block size is hashed first.
        assert_eq!(
            hex(&hmac_sha256(
                &[0xaa; 131],
                &[b"Test Using Larger Than Block-Size Key - Hash Key First"]
            )),
            "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
        );
    }

    #[test]
    fn test_client_and_server_macs_are_domain_separated() {
        let token = [7u8; 32];
        let nonce = [1u8; 32];
        assert_ne!(client_mac(&token, &nonce), server_mac(&token, &nonce));
        assert_ne!(client_mac(&token, &nonce), client_mac(&[8u8; 32], &nonce));
        assert!(ct_eq(
            &client_mac(&token, &nonce),
            &client_mac(&token, &nonce)
        ));
        assert!(!ct_eq(b"ab", b"abc"));
    }

    #[test]
    fn test_valid_name() {
        for ok in [&b"a"[..], b"...", b"caf\xc3\xa9", &[b'x'; NAME_MAX]] {
            assert!(valid_name(ok), "{ok:?}");
        }
        for bad in [
            &b""[..],
            b".",
            b"..",
            b"a/b",
            b"/",
            b"a\0b",
            &[b'x'; NAME_MAX + 1],
        ] {
            assert!(!valid_name(bad), "{bad:?}");
        }
    }

    #[test]
    fn test_hex_round_trip() {
        let b = random32().unwrap();
        assert_eq!(unhex(&hex(&b)).unwrap(), b);
        assert_eq!(unhex("0aFF").unwrap(), vec![0x0a, 0xff]);
        assert!(unhex("abc").is_none());
        assert!(unhex("zz").is_none());
    }

    #[test]
    fn test_errno_to_darwin() {
        assert_eq!(Errno::ENOENT.to_darwin(), 2);
        assert_eq!(Errno(11).to_darwin(), 35);
        assert_eq!(Errno::ENOTEMPTY.to_darwin(), 66);
        assert_eq!(Errno::ELOOP.to_darwin(), 62);
        assert_eq!(Errno(9999).to_darwin(), 5);
    }

    #[test]
    fn test_file_kind_from_mode() {
        assert_eq!(FileKind::from_mode(0o100644), Some(FileKind::File));
        assert_eq!(FileKind::from_mode(0o040755), Some(FileKind::Dir));
        assert_eq!(FileKind::from_mode(0o120777), Some(FileKind::Symlink));
        assert_eq!(FileKind::from_mode(0o000644), None);
    }
}
