/*
 * Copyright (c) 2026 Daphne Pfister
 * SPDX-License-Identifier: BSD-2-Clause
 * See LICENSE file for full license text
 */

//! The host-side file commands: `ls`, `stat`, `get`, `put`, `check`.
//! Guest paths are absolute from the guest root; symlink targets are
//! confined to the volume ([`distrofs::symlink`]).

use std::fs::File;
use std::io::{self, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use anyhow::{Context, Result, bail};
use distrofs::client::{Client, Resolved};
use distrofs::proto::{Attr, FileKind, MAX_IO, OpenFlags, ROOT, VERSION};

use crate::table::columns;

/// Dispatch one file command over an already-open session.
/// `via` describes the transport for `check`'s report.
pub fn run(cmd: &str, args: &[String], c: &Client, via: &str, peer_verified: bool) -> Result<()> {
    match cmd {
        "ls" => {
            let (long, path) = flag_l(args, 'l')?;
            ls(c, &path, long)
        }
        "stat" => {
            let (deref, path) = flag_l(args, 'L')?;
            stat(c, &path, deref)
        }
        "get" => match args {
            [path] => get(c, path, None),
            [path, dest] => get(c, path, Some(Path::new(dest))),
            _ => bail!("usage: distrofs get PATH [DEST]"),
        },
        "put" => match args {
            [src, path] => put(c, Path::new(src), path),
            _ => bail!("usage: distrofs put SRC PATH"),
        },
        "check" => check(c, via, peer_verified),
        _ => bail!("unknown command `{cmd}`"),
    }
}

/// `ls`/`stat` argument shape: an optional `-l`/`-L`, then the path.
fn flag_l(args: &[String], flag: char) -> Result<(bool, String)> {
    let short = format!("-{flag}");
    let long = match flag {
        'l' => Some("--long"),
        'L' => Some("--dereference"),
        _ => None,
    };
    let mut on = false;
    let mut path = None;
    for a in args {
        if a == &short || Some(a.as_str()) == long {
            on = true;
        } else if path.is_none() && !a.starts_with('-') {
            path = Some(a.clone());
        } else {
            bail!("unexpected argument `{a}`");
        }
    }
    Ok((on, path.unwrap_or_else(|| "/".into())))
}

/// `check` — prove the whole connect path worked and show the root.
fn check(c: &Client, via: &str, peer_verified: bool) -> Result<()> {
    let root = c.root_attr();
    println!(
        "connected over {via}; session handshake ok (protocol {VERSION}); \
         root is inode {} owned by {}:{}",
        root.ino, root.uid, root.gid
    );
    if peer_verified {
        println!("unix peer verified against --peer-requirement");
    }
    Ok(())
}

fn display(path: &[Vec<u8>]) -> String {
    let joined: Vec<u8> = path.iter().flat_map(|c| [&b"/"[..], c].concat()).collect();
    if joined.is_empty() {
        "/".into()
    } else {
        String::from_utf8_lossy(&joined).into_owned()
    }
}

/// `drwxr-xr-x`-style, with setuid/setgid/sticky folded in.
fn mode_string(a: &Attr) -> String {
    let kind = match a.kind {
        FileKind::File => '-',
        FileKind::Dir => 'd',
        FileKind::Symlink => 'l',
        FileKind::Char => 'c',
        FileKind::Block => 'b',
        FileKind::Fifo => 'p',
        FileKind::Socket => 's',
    };
    let mut s = String::from(kind);
    for (i, special, ch) in [(6, 0o4000, 's'), (3, 0o2000, 's'), (0, 0o1000, 't')] {
        let bits = (a.perm >> i) & 7;
        s.push(if bits & 4 != 0 { 'r' } else { '-' });
        s.push(if bits & 2 != 0 { 'w' } else { '-' });
        let x = bits & 1 != 0;
        s.push(match (a.perm & special != 0, x) {
            (true, true) => ch,
            (true, false) => ch.to_ascii_uppercase(),
            (false, true) => 'x',
            (false, false) => '-',
        });
    }
    s
}

/// Local `YYYY-MM-DD HH:MM`.
// libc::time_t is a deprecated alias on musl (64-bit there anyway).
#[allow(deprecated)]
fn local_time(sec: i64) -> String {
    let t = sec as libc::time_t;
    // SAFETY: `tm` is a valid out-param.
    let mut tm = unsafe { std::mem::zeroed::<libc::tm>() };
    if unsafe { libc::localtime_r(&t, &mut tm) }.is_null() {
        return "-".into();
    }
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min
    )
}

fn long_row(name: &str, a: &Attr, target: Option<Vec<u8>>) -> Vec<String> {
    let mut n = name.to_string();
    if let Some(t) = target {
        n.push_str(" -> ");
        n.push_str(&String::from_utf8_lossy(&t));
    }
    vec![
        mode_string(a),
        a.nlink.to_string(),
        a.uid.to_string(),
        a.gid.to_string(),
        a.size.to_string(),
        local_time(a.mtime.sec),
        n,
    ]
}

fn ls(c: &Client, path: &str, long: bool) -> Result<()> {
    let r = c.resolve(path.as_bytes(), true)?;
    if r.attr.kind != FileKind::Dir {
        let target = (r.attr.kind == FileKind::Symlink)
            .then(|| c.readlink_confined(&r))
            .transpose()?;
        let name = display(&r.path);
        if long {
            println!(
                "{}",
                columns(&[long_row(&name, &r.attr, target)], 1).join("\n")
            );
        } else {
            println!("{name}");
        }
        return Ok(());
    }
    let mut entries = c.readdir_all(r.node)?;
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    if !long {
        for e in &entries {
            println!("{}", String::from_utf8_lossy(&e.name));
        }
        return Ok(());
    }
    let mut rows = Vec::new();
    for e in &entries {
        let target = if e.attr.kind == FileKind::Symlink {
            let (node, attr) = c.lookup(r.node, &e.name)?;
            let mut p = r.path.clone();
            p.push(e.name.clone());
            Some(c.readlink_confined(&Resolved {
                node,
                attr,
                path: p,
            })?)
        } else {
            None
        };
        rows.push(long_row(&String::from_utf8_lossy(&e.name), &e.attr, target));
    }
    for line in columns(&rows, 1) {
        println!("{}", line.trim_end());
    }
    Ok(())
}

fn stat(c: &Client, path: &str, dereference: bool) -> Result<()> {
    let r = c.resolve(path.as_bytes(), dereference)?;
    let a = &r.attr;
    println!("  path: {}", display(&r.path));
    println!("  type: {:?}", a.kind);
    println!("  mode: {} ({:04o})", mode_string(a), a.perm);
    println!(" owner: {}:{}", a.uid, a.gid);
    println!("  size: {}", a.size);
    println!(" links: {}", a.nlink);
    println!(" inode: {}", a.ino);
    println!("modify: {}", local_time(a.mtime.sec));
    println!("change: {}", local_time(a.ctime.sec));
    if a.kind == FileKind::Symlink {
        println!(
            "target: {}",
            String::from_utf8_lossy(&c.readlink_confined(&r)?)
        );
    }
    Ok(())
}

fn get(c: &Client, path: &str, dest: Option<&Path>) -> Result<()> {
    let r = c.resolve(path.as_bytes(), true)?;
    match r.attr.kind {
        FileKind::File => {}
        FileKind::Dir => bail!("{} is a directory", display(&r.path)),
        _ => bail!("{} is not a regular file", display(&r.path)),
    }
    let mut out: Box<dyn Write> = match dest {
        None => Box::new(io::stdout().lock()),
        Some(p) if p.as_os_str() == "-" => Box::new(io::stdout().lock()),
        Some(p) => {
            let p = if p.is_dir() {
                p.join(
                    String::from_utf8_lossy(r.path.last().map_or(&b"file"[..], Vec::as_slice))
                        .as_ref(),
                )
            } else {
                p.to_path_buf()
            };
            Box::new(File::create(&p).with_context(|| format!("failed to create {}", p.display()))?)
        }
    };
    let fh = c.open(
        r.node,
        OpenFlags {
            read: true,
            ..OpenFlags::default()
        },
    )?;
    let result = (|| -> Result<()> {
        let mut off = 0u64;
        loop {
            let chunk = c.read(fh, off, MAX_IO)?;
            if chunk.is_empty() {
                return Ok(());
            }
            out.write_all(&chunk)?;
            off += chunk.len() as u64;
        }
    })();
    let _ = c.release(fh);
    // A closed reader (`… | head`) is a normal way to stop.
    let quiet_pipe = |e: &anyhow::Error| {
        e.downcast_ref::<io::Error>()
            .is_some_and(|e| e.kind() == io::ErrorKind::BrokenPipe)
    };
    match result.and_then(|()| out.flush().map_err(Into::into)) {
        Err(e) if quiet_pipe(&e) => Ok(()),
        r => r,
    }
}

fn put(c: &Client, src: &Path, path: &str) -> Result<()> {
    let stdin = src.as_os_str() == "-";
    let (mut input, perm): (Box<dyn Read>, u32) = if stdin {
        (Box::new(io::stdin().lock()), 0o644)
    } else {
        let f = File::open(src).with_context(|| format!("failed to open {}", src.display()))?;
        let perm = f.metadata()?.permissions().mode() & 0o777;
        (Box::new(f), perm)
    };
    // `cp` semantics: into an existing directory, keep the source name.
    let (parent, name) = match c.resolve(path.as_bytes(), true) {
        Ok(r) if r.attr.kind == FileKind::Dir => {
            if stdin {
                bail!("{path} is a directory — name the destination file when reading stdin");
            }
            let base = src.file_name().context("source has no file name")?;
            (r.node, base.as_bytes().to_vec())
        }
        _ => {
            let trimmed = path.trim_end_matches('/');
            let (dir, base) = trimmed.rsplit_once('/').unwrap_or(("", trimmed));
            let parent = if dir.is_empty() {
                ROOT
            } else {
                c.resolve(dir.as_bytes(), true)?.node
            };
            (parent, base.as_bytes().to_vec())
        }
    };
    let (_, _, fh) = c.create(
        parent,
        &name,
        perm,
        false,
        OpenFlags {
            write: true,
            truncate: true,
            ..OpenFlags::default()
        },
    )?;
    let result = (|| -> Result<()> {
        let mut buf = vec![0u8; MAX_IO as usize];
        let mut off = 0u64;
        loop {
            let n = input.read(&mut buf)?;
            if n == 0 {
                return Ok(c.fsync(fh)?);
            }
            let mut done = 0;
            while done < n {
                let w = c.write(fh, off, &buf[done..n])? as usize;
                if w == 0 {
                    bail!("guest accepted no bytes");
                }
                done += w;
                off += w as u64;
            }
        }
    })();
    let _ = c.release(fh);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use distrofs::proto::Time;

    fn attr(kind: FileKind, perm: u32) -> Attr {
        Attr {
            ino: 1,
            kind,
            perm,
            nlink: 1,
            uid: 0,
            gid: 0,
            rdev: 0,
            size: 0,
            blocks: 0,
            blksize: 0,
            atime: Time::default(),
            mtime: Time::default(),
            ctime: Time::default(),
        }
    }

    #[test]
    fn test_mode_string() {
        assert_eq!(mode_string(&attr(FileKind::Dir, 0o755)), "drwxr-xr-x");
        assert_eq!(mode_string(&attr(FileKind::File, 0o4755)), "-rwsr-xr-x");
        assert_eq!(mode_string(&attr(FileKind::Dir, 0o1777)), "drwxrwxrwt");
        assert_eq!(mode_string(&attr(FileKind::File, 0o2640)), "-rw-r-S---");
        assert_eq!(mode_string(&attr(FileKind::Symlink, 0o777)), "lrwxrwxrwx");
    }

    #[test]
    fn test_display_path() {
        assert_eq!(display(&[]), "/");
        assert_eq!(display(&[b"usr".to_vec(), b"bin".to_vec()]), "/usr/bin");
    }

    #[test]
    fn test_flag_l() {
        assert_eq!(
            flag_l(&["-l".into(), "/etc".into()], 'l').unwrap(),
            (true, "/etc".into())
        );
        assert_eq!(flag_l(&[], 'l').unwrap(), (false, "/".into()));
        assert!(flag_l(&["--bogus".into()], 'l').is_err());
    }
}
