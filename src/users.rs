/*
 * Copyright (c) 2026 Daphne Pfister
 * SPDX-License-Identifier: BSD-2-Clause
 * See LICENSE file for full license text
 */

//! Guest user resolution — `NAME|UID[:GID]` to uid, gid, and home.
//!
//! Lookups go through a [`Db`]. The server chains two, like
//! container-distro's `getent passwd … || cat /etc/passwd`: libc's
//! `getpwnam_r`/`getpwuid_r`/`getgrnam_r` first (on musl: `/etc/passwd` plus an nscd
//! daemon when one runs), then [`Files`] — the guest's own
//! `/etc/passwd` and `/etc/group` read through the server's confined
//! lookups, which is also the only source that will be right once
//! `serve --root` serves a subtree. On top of whichever answers, the
//! rules (matching container-distro's `~` handling where they overlap):
//!
//! - a name or uid with a passwd entry gets that entry's primary gid
//!   and home;
//! - `root` / uid 0 without an entry (`FROM scratch`) is `0:0`, `/root`;
//! - a numeric uid without an entry gets gid 0 (the OCI runtimes' rule
//!   for an unknown uid) and home `/`;
//! - an unknown *name* is an error — there is no uid to own files as;
//! - `:GID` overrides the gid, numerically or by group name.
//!
//! musl has no NSS modules, so users only an LDAP/SSSD backend knows
//! resolve by number (or through nscd). Every source is guest-controlled:
//! resolution only picks guest ids and a guest path, which the client
//! still validates.

/// One resolved guest user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct User {
    pub uid: u32,
    pub gid: u32,
    /// The home directory as the guest names it (absolute).
    pub home: Vec<u8>,
}

/// Why a user spec didn't resolve.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// Empty, or not `NAME|UID[:GID]`.
    Malformed,
    /// A name with no passwd entry.
    UnknownUser,
    /// A group name with no group entry.
    UnknownGroup,
}

/// A passwd entry's fields we use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub uid: u32,
    pub gid: u32,
    pub home: Vec<u8>,
}

/// A source of passwd and group entries.
pub trait Db {
    fn by_name(&self, name: &[u8]) -> Option<Entry>;
    fn by_uid(&self, uid: u32) -> Option<Entry>;
    /// A group name's gid.
    fn group(&self, name: &[u8]) -> Option<u32>;
}

/// Try `.0`, then `.1` — each lookup on its own.
impl<A: Db, B: Db> Db for (A, B) {
    fn by_name(&self, name: &[u8]) -> Option<Entry> {
        self.0.by_name(name).or_else(|| self.1.by_name(name))
    }

    fn by_uid(&self, uid: u32) -> Option<Entry> {
        self.0.by_uid(uid).or_else(|| self.1.by_uid(uid))
    }

    fn group(&self, name: &[u8]) -> Option<u32> {
        self.0.group(name).or_else(|| self.1.group(name))
    }
}

/// `/etc/passwd` and `/etc/group` text; either may be empty (absent).
pub struct Files<'a> {
    pub passwd: &'a [u8],
    pub group: &'a [u8],
}

impl Db for Files<'_> {
    fn by_name(&self, name: &[u8]) -> Option<Entry> {
        passwd_entries(self.passwd)
            .find(|e| e.name == name)
            .map(PasswdEntry::entry)
    }

    fn by_uid(&self, uid: u32) -> Option<Entry> {
        passwd_entries(self.passwd)
            .find(|e| e.uid == uid)
            .map(PasswdEntry::entry)
    }

    fn group(&self, name: &[u8]) -> Option<u32> {
        self.group.split(|&b| b == b'\n').find_map(|line| {
            let f: Vec<&[u8]> = line.split(|&b| b == b':').collect();
            (f.len() >= 3 && f[0] == name)
                .then(|| number(f[2]))
                .flatten()
        })
    }
}

/// One `/etc/passwd` line's fields we use.
struct PasswdEntry<'a> {
    name: &'a [u8],
    uid: u32,
    gid: u32,
    home: &'a [u8],
}

impl PasswdEntry<'_> {
    fn entry(self) -> Entry {
        Entry {
            uid: self.uid,
            gid: self.gid,
            home: self.home.to_vec(),
        }
    }
}

fn passwd_entries(text: &[u8]) -> impl Iterator<Item = PasswdEntry<'_>> {
    text.split(|&b| b == b'\n').filter_map(|line| {
        let f: Vec<&[u8]> = line.split(|&b| b == b':').collect();
        // name:passwd:uid:gid:gecos:home:shell — skip anything shorter
        // or with non-numeric ids rather than guess.
        if f.len() < 7 || f[0].is_empty() || f[0].starts_with(b"#") {
            return None;
        }
        Some(PasswdEntry {
            name: f[0],
            uid: number(f[2])?,
            gid: number(f[3])?,
            home: f[5],
        })
    })
}

/// A plain decimal id — no sign, no spaces.
fn number(s: &[u8]) -> Option<u32> {
    if s.is_empty() || !s.iter().all(u8::is_ascii_digit) {
        return None;
    }
    std::str::from_utf8(s).ok()?.parse().ok()
}

/// A usable home: absolute, otherwise the conventional default.
fn home_or(home: &[u8], default: &[u8]) -> Vec<u8> {
    if home.starts_with(b"/") {
        home.to_vec()
    } else {
        default.to_vec()
    }
}

/// Resolve `spec` (`NAME`, `UID`, `NAME:GID`, `UID:GROUP`, …) against `db`.
pub fn resolve(spec: &[u8], db: &impl Db) -> Result<User, Error> {
    let (user, group_spec) = match spec.iter().position(|&b| b == b':') {
        Some(i) => (&spec[..i], Some(&spec[i + 1..])),
        None => (spec, None),
    };
    if user.is_empty() || group_spec.is_some_and(<[u8]>::is_empty) {
        return Err(Error::Malformed);
    }
    let uid = number(user);
    let entry = match uid {
        Some(u) => db.by_uid(u),
        None => db.by_name(user),
    };
    let mut resolved = match (entry, uid) {
        (Some(e), _) => User {
            uid: e.uid,
            gid: e.gid,
            home: home_or(&e.home, if e.uid == 0 { b"/root" } else { b"/" }),
        },
        (None, Some(0)) => root(),
        (None, None) if user == b"root" => root(),
        (None, Some(u)) => User {
            uid: u,
            gid: 0,
            home: b"/".to_vec(),
        },
        (None, None) => return Err(Error::UnknownUser),
    };
    if let Some(g) = group_spec {
        resolved.gid = match number(g) {
            Some(n) => n,
            None => db.group(g).ok_or(Error::UnknownGroup)?,
        };
    }
    Ok(resolved)
}

fn root() -> User {
    User {
        uid: 0,
        gid: 0,
        home: b"/root".to_vec(),
    }
}

#[cfg(test)]
mod tests {
    // cspell:ignore ldapuser ldapgroup relhome notanumber nogroup
    use super::*;

    const PASSWD: &[u8] = b"root:x:0:0:root:/root:/bin/sh\n\
        # a comment\n\
        daphne:x:1027:20:Daphne:/home/daphne:/bin/zsh\n\
        www:x:33:33:www:/var/www:/usr/sbin/nologin\n\
        broken:x:notanumber:1::/home/broken:/bin/sh\n\
        short:x:5\n\
        relhome:x:44:44::relative:/bin/sh\n";
    const GROUP: &[u8] = b"root:x:0:\nstaff:x:20:\nwheel:x:10:daphne\n";

    /// [`resolve`] against the given passwd/group text.
    fn r(spec: &[u8], passwd: &[u8], group: &[u8]) -> Result<User, Error> {
        resolve(spec, &Files { passwd, group })
    }

    fn user(uid: u32, gid: u32, home: &str) -> Result<User, Error> {
        Ok(User {
            uid,
            gid,
            home: home.as_bytes().to_vec(),
        })
    }

    /// A source that only knows one directory-service user and group.
    struct Directory;

    impl Db for Directory {
        fn by_name(&self, name: &[u8]) -> Option<Entry> {
            (name == b"ldapuser").then(|| self.by_uid(5000)).flatten()
        }

        fn by_uid(&self, uid: u32) -> Option<Entry> {
            (uid == 5000).then(|| Entry {
                uid: 5000,
                gid: 5000,
                home: b"/home/ldapuser".to_vec(),
            })
        }

        fn group(&self, name: &[u8]) -> Option<u32> {
            (name == b"ldapgroup").then_some(5001)
        }
    }

    #[test]
    fn test_resolve_by_name_and_uid() {
        assert_eq!(r(b"daphne", PASSWD, GROUP), user(1027, 20, "/home/daphne"));
        assert_eq!(r(b"1027", PASSWD, GROUP), user(1027, 20, "/home/daphne"));
        // `www`'s home is wherever passwd says, not /home/www.
        assert_eq!(r(b"www", PASSWD, GROUP), user(33, 33, "/var/www"));
        assert_eq!(r(b"root", PASSWD, GROUP), user(0, 0, "/root"));
    }

    #[test]
    fn test_resolve_without_entries() {
        // FROM scratch: no passwd at all.
        assert_eq!(r(b"root", b"", b""), user(0, 0, "/root"));
        assert_eq!(r(b"0", b"", b""), user(0, 0, "/root"));
        // An unlisted uid still owns files; its home is `/`.
        assert_eq!(r(b"4242", PASSWD, GROUP), user(4242, 0, "/"));
        assert_eq!(r(b"nobody", PASSWD, GROUP), Err(Error::UnknownUser));
    }

    #[test]
    fn test_resolve_group_override() {
        assert_eq!(
            r(b"daphne:10", PASSWD, GROUP),
            user(1027, 10, "/home/daphne")
        );
        assert_eq!(
            r(b"daphne:wheel", PASSWD, GROUP),
            user(1027, 10, "/home/daphne")
        );
        assert_eq!(r(b"4242:staff", PASSWD, GROUP), user(4242, 20, "/"));
        assert_eq!(
            r(b"daphne:nogroup", PASSWD, GROUP),
            Err(Error::UnknownGroup)
        );
    }

    #[test]
    fn test_resolve_rejects_malformed_and_skips_bad_lines() {
        for bad in [&b""[..], b":", b"daphne:", b":20"] {
            assert_eq!(r(bad, PASSWD, GROUP), Err(Error::Malformed), "{bad:?}");
        }
        // Lines with non-numeric ids or too few fields never match.
        assert_eq!(r(b"broken", PASSWD, GROUP), Err(Error::UnknownUser));
        assert_eq!(r(b"short", PASSWD, GROUP), Err(Error::UnknownUser));
        // A relative home falls back to `/`.
        assert_eq!(r(b"relhome", PASSWD, GROUP), user(44, 44, "/"));
        assert_eq!(number(b"+5"), None);
        assert_eq!(number(b"99999999999"), None);
    }

    #[test]
    fn test_chain_prefers_first_source_and_falls_back() {
        let db = (
            Directory,
            Files {
                passwd: PASSWD,
                group: GROUP,
            },
        );
        assert_eq!(
            resolve(b"ldapuser:ldapgroup", &db),
            user(5000, 5001, "/home/ldapuser")
        );
        // The first source misses: the files answer, rules unchanged.
        assert_eq!(resolve(b"www:staff", &db), user(33, 20, "/var/www"));
        assert_eq!(resolve(b"4242", &db), user(4242, 0, "/"));
    }
}
