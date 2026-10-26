/*
 * Copyright (c) 2026 Daphne Pfister
 * SPDX-License-Identifier: BSD-2-Clause
 * See LICENSE file for full license text
 */

//! The host-side file commands: `ls`, `stat`, `get`, `put`, `check`.
//! Guest paths are absolute from the guest root; symlink targets are
//! confined to the volume ([`container_mount::symlink`]).

use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileExt, MetadataExt, PermissionsExt};
use std::path::Path;

use anyhow::{Context, Result, bail};
use container_mount::client::{Client, Error, Resolved};
use container_mount::proto::{
    Attr, CAP_SEEK, Errno, FileKind, MAX_IO, NodeId, OpenFlags, ROOT, SeekWhence, SetAttr, VERSION,
};

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
            _ => bail!("usage: container-mount get PATH [DEST]"),
        },
        "put" => match args {
            [src, path] => put(c, Path::new(src), path),
            _ => bail!("usage: container-mount put SRC PATH"),
        },
        "cp" => match args {
            [src, path] => cp(c, src, path),
            _ => bail!("usage: container-mount cp SRC PATH"),
        },
        "check" => check(c, via, peer_verified),
        "user" => match args {
            [spec] => {
                let u = c.lookup_user(spec.as_bytes())?;
                println!(
                    "uid={} gid={} home={}",
                    u.uid,
                    u.gid,
                    String::from_utf8_lossy(&u.home)
                );
                Ok(())
            }
            _ => bail!("usage: container-mount user NAME|UID[:GID]"),
        },
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

// cspell:ignore drwxr rwsr drwxrwxrwt lrwxrwxrwx
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

/// Stream `fh` to `out` in `MAX_IO` chunks.
fn pump_read(c: &Client, fh: u64, out: &mut impl Write) -> Result<()> {
    let mut off = 0u64;
    loop {
        let chunk = c.read(fh, off, MAX_IO)?;
        if chunk.is_empty() {
            return Ok(());
        }
        out.write_all(&chunk)?;
        off += chunk.len() as u64;
    }
}

/// The guest's data extents copied into `f`; holes stay unallocated.
/// APFS allocates the flushed gaps of an extended file, so the hole
/// ranges are punched back out before close.
fn get_sparse(c: &Client, fh: u64, size: u64, f: &mut File) -> Result<()> {
    let mut holes = Vec::new();
    let mut pos = 0;
    let mut end = 0; // end of the last copied extent
    while pos < size {
        let d = match c.seek(fh, pos, SeekWhence::Data) {
            Ok(d) => d,
            // ENXIO: the rest of the file is a hole.
            Err(Error::Guest(Errno::ENXIO)) => break,
            Err(e) => return Err(e.into()),
        };
        if d >= size {
            break;
        }
        if d > end {
            holes.push((end, d));
        }
        let h = c.seek(fh, d, SeekWhence::Hole)?.min(size);
        let mut off = d;
        while off < h {
            let chunk = c.read(fh, off, (h - off).min(MAX_IO as u64) as u32)?;
            if chunk.is_empty() {
                break;
            }
            f.write_all_at(&chunk, off)?;
            off += chunk.len() as u64;
        }
        end = h;
        pos = h;
    }
    if end < size {
        holes.push((end, size));
    }
    // A trailing hole carries no data — the size sets it.
    f.set_len(size)?;
    let blk = punch_align(f);
    for (off, end) in holes {
        punch_hole(f, blk, off, end - off);
    }
    Ok(())
}

/// Block size hole punches align to — `f_bsize`, 4096 if unknown.
fn punch_align(f: &File) -> u64 {
    // SAFETY: `s` is ours and the fd is live.
    unsafe {
        let mut s: libc::statfs = std::mem::zeroed();
        if libc::fstatfs(f.as_raw_fd(), &mut s) == 0 && s.f_bsize > 0 {
            s.f_bsize as u64
        } else {
            4096
        }
    }
}

/// Free whole filesystem blocks in `off .. off + len`. Best-effort —
/// where punching isn't supported the range keeps its (correct)
/// zeros, just allocated.
fn punch_hole(f: &File, blk: u64, off: u64, len: u64) {
    // `fpunchhole_t` and `F_PUNCHHOLE` from <fcntl.h> — libc doesn't
    // bind the struct.
    #[repr(C)]
    struct FPunchhole {
        fp_flags: u32,
        reserved: u32,
        fp_offset: i64,
        fp_length: i64,
    }
    const F_PUNCHHOLE: libc::c_int = 99;

    let start = off.div_ceil(blk);
    let end = (off + len) / blk;
    if end <= start {
        return;
    }
    let p = FPunchhole {
        fp_flags: 0,
        reserved: 0,
        fp_offset: (start * blk) as i64,
        fp_length: ((end - start) * blk) as i64,
    };
    // SAFETY: `p` is a valid fpunchhole_t for the call's duration.
    unsafe { libc::fcntl(f.as_raw_fd(), F_PUNCHHOLE, &p) };
}

/// Whether `e` says the server/fs can't scan extents.
fn extents_unsupported(e: &anyhow::Error) -> bool {
    matches!(e.downcast_ref::<Error>(), Some(Error::Guest(e))
        if *e == Errno::EOPNOTSUPP)
}

fn get(c: &Client, path: &str, dest: Option<&Path>) -> Result<()> {
    let r = c.resolve(path.as_bytes(), true)?;
    match r.attr.kind {
        FileKind::File => {}
        FileKind::Dir => bail!("{} is a directory", display(&r.path)),
        _ => bail!("{} is not a regular file", display(&r.path)),
    }
    let file = match dest {
        None => None,
        Some(p) if p.as_os_str() == "-" => None,
        Some(p) => {
            let p = if p.is_dir() {
                p.join(
                    String::from_utf8_lossy(r.path.last().map_or(&b"file"[..], Vec::as_slice))
                        .as_ref(),
                )
            } else {
                p.to_path_buf()
            };
            Some(File::create(&p).with_context(|| format!("failed to create {}", p.display()))?)
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
        match file {
            Some(mut f) => {
                // Worth probing only when the guest reports fewer
                // allocated blocks than the size implies.
                if r.attr.blocks.saturating_mul(512) < r.attr.size && c.caps() & CAP_SEEK != 0 {
                    match get_sparse(c, fh, r.attr.size, &mut f) {
                        Err(e) if extents_unsupported(&e) => {}
                        r => return r,
                    }
                }
                pump_read(c, fh, &mut f)
            }
            None => {
                let mut out = io::stdout().lock();
                pump_read(c, fh, &mut out)?;
                out.flush().map_err(Into::into)
            }
        }
    })();
    let _ = c.release(fh);
    // A closed reader (`… | head`) is a normal way to stop.
    let quiet_pipe = |e: &anyhow::Error| {
        e.downcast_ref::<io::Error>()
            .is_some_and(|e| e.kind() == io::ErrorKind::BrokenPipe)
    };
    match result {
        Err(e) if quiet_pipe(&e) => Ok(()),
        r => r,
    }
}

/// Data extents of a local file via `SEEK_DATA`/`SEEK_HOLE` — `None`
/// when the platform can't report them (treat the file as dense).
fn data_extents(f: &File, size: u64) -> Option<Vec<(u64, u64)>> {
    let fd = f.as_raw_fd();
    let mut out = Vec::new();
    let mut pos = 0;
    while pos < size {
        // SAFETY: lseek on a live fd.
        let d = unsafe { libc::lseek(fd, pos.min(i64::MAX as u64) as i64, libc::SEEK_DATA) };
        if d < 0 {
            match io::Error::last_os_error().raw_os_error() {
                // The rest of the file is a hole.
                Some(libc::ENXIO) => break,
                _ => return None,
            }
        }
        let d = d as u64;
        if d >= size {
            break;
        }
        // SAFETY: as above.
        let h = unsafe { libc::lseek(fd, d.min(i64::MAX as u64) as i64, libc::SEEK_HOLE) };
        if h < 0 {
            return None;
        }
        out.push((d, (h as u64).min(size)));
        pos = h as u64;
    }
    Some(out)
}

/// Write `f`'s data `extents` to `fh`, then size the file right —
/// the guest file's holes stay unallocated.
fn put_sparse(
    c: &Client,
    fh: u64,
    node: NodeId,
    f: &File,
    size: u64,
    extents: &[(u64, u64)],
) -> Result<()> {
    let mut buf = vec![0u8; MAX_IO as usize];
    for &(d, h) in extents {
        let mut off = d;
        while off < h {
            let n = f.read_at(&mut buf[..(h - off).min(MAX_IO as u64) as usize], off)?;
            if n == 0 {
                break;
            }
            let mut done = 0;
            while done < n {
                let w = c.write(fh, off + done as u64, &buf[done..n])? as usize;
                if w == 0 {
                    bail!("guest accepted no bytes");
                }
                done += w;
            }
            off += n as u64;
        }
    }
    // A trailing hole carries no data — the size sets it.
    c.setattr(
        node,
        SetAttr {
            size: Some(size),
            ..SetAttr::default()
        },
    )?;
    Ok(())
}

/// Stream `input` to `fh` in `MAX_IO` chunks.
fn pump_write(c: &Client, fh: u64, input: &mut impl Read) -> Result<()> {
    let mut buf = vec![0u8; MAX_IO as usize];
    let mut off = 0u64;
    loop {
        let n = input.read(&mut buf)?;
        if n == 0 {
            return Ok(());
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
}

fn put(c: &Client, src: &Path, path: &str) -> Result<()> {
    let stdin = src.as_os_str() == "-";
    let (file, perm) = if stdin {
        (None, 0o644)
    } else {
        let f = File::open(src).with_context(|| format!("failed to open {}", src.display()))?;
        let perm = f.metadata()?.permissions().mode() & 0o777;
        (Some(f), perm)
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
    let (node, _, fh) = c.create(
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
        match &file {
            None => pump_write(c, fh, &mut io::stdin().lock())?,
            Some(f) => {
                let m = f.metadata()?;
                let size = m.len();
                // Sparse only if the fs reports extents and there are
                // fewer allocated blocks than the size implies.
                let extents = (m.blocks().saturating_mul(512) < size)
                    .then(|| data_extents(f, size))
                    .flatten();
                match extents {
                    Some(e) => put_sparse(c, fh, node, f, size, &e)?,
                    None => pump_write(c, fh, &mut &*f)?,
                }
            }
        }
        Ok(c.fsync(fh)?)
    })();
    let _ = c.release(fh);
    result
}

/// `cp` — a guest-side copy: reflinked where the filesystem allows,
/// hole-preserving otherwise, never a wire round trip. The destination
/// must not exist (exclusive create).
fn cp(c: &Client, src: &str, path: &str) -> Result<()> {
    let r = c.resolve(src.as_bytes(), true)?;
    match r.attr.kind {
        FileKind::File => {}
        FileKind::Dir => bail!("{} is a directory", display(&r.path)),
        _ => bail!("{} is not a regular file", display(&r.path)),
    }
    // Same destination rules as `put`.
    let (parent, name) = match c.resolve(path.as_bytes(), true) {
        Ok(d) if d.attr.kind == FileKind::Dir => (
            d.node,
            r.path.last().map_or(&b"copy"[..], Vec::as_slice).to_vec(),
        ),
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
    c.copy(r.node, parent, &name, 0).map(drop)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use container_mount::proto::Time;

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
