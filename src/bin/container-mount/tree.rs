/*
 * Copyright (c) 2026 Daphne Pfister
 * SPDX-License-Identifier: BSD-2-Clause
 * See LICENSE file for full license text
 */

//! Recursive copies: host to guest, guest to host, and within a guest.
//!
//! `container copy` / `cp -r` naming: an existing directory destination
//! receives the source under its own name, anything else is the new
//! name (its parent must exist). Regular files, directories, and
//! symlinks are copied; devices, FIFOs, and sockets are skipped with a
//! warning. Modes keep their permission bits; owners are whoever does
//! the writing (the host user, or the server's `--user` in the guest).
//!
//! The host side is the trusted one, so copies *out* follow two rules:
//!
//! - a guest symlink is recreated only if its target, resolved from
//!   where the link sits, stays inside the copied tree — confinement to
//!   the guest root means nothing once a subtree lands in `~/out`, where
//!   `../../etc/passwd` would name a real host file. Absolute and
//!   escaping links are skipped and reported;
//! - nothing is written through an existing host symlink (files open
//!   `O_NOFOLLOW`, directories must be real directories), and setuid,
//!   setgid, and sticky bits are dropped.

use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io::ErrorKind;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use container_mount::client::Client;
use container_mount::proto::{Attr, FileKind, NodeId, ROOT};

use crate::fscli;

/// Deepest tree a copy walks — a loop guard, far past real layouts.
const MAX_DEPTH: usize = 256;

/// What a copy left out, reported once at the end.
#[derive(Default)]
struct Skipped {
    links: Vec<String>,
    special: Vec<String>,
}

impl Skipped {
    fn report(&self) {
        if !self.links.is_empty() {
            eprintln!(
                "skipped {} symlink(s) pointing outside the copied tree: {}",
                self.links.len(),
                self.links.join(", ")
            );
        }
        if !self.special.is_empty() {
            eprintln!(
                "skipped {} device/FIFO/socket file(s): {}",
                self.special.len(),
                self.special.join(", ")
            );
        }
    }
}

/// Where a guest destination lands.
struct Slot {
    parent: NodeId,
    /// The parent's resolved components from the guest root.
    parent_path: Vec<Vec<u8>>,
    name: Vec<u8>,
}

/// An existing directory `path` receives the source's `name`;
/// otherwise `path` names the new entry.
fn guest_slot(c: &Client, path: &str, name: &[u8]) -> Result<Slot> {
    if let Ok(d) = c.resolve(path.as_bytes(), true)
        && d.attr.kind == FileKind::Dir
    {
        return Ok(Slot {
            parent: d.node,
            parent_path: d.path,
            name: name.to_vec(),
        });
    }
    let trimmed = path.trim_end_matches('/');
    let (dir, base) = trimmed.rsplit_once('/').unwrap_or(("", trimmed));
    if base.is_empty() {
        bail!("`{path}` doesn't name an entry");
    }
    let (parent, parent_path) = if dir.is_empty() {
        (ROOT, Vec::new())
    } else {
        let d = c.resolve(dir.as_bytes(), true)?;
        (d.node, d.path)
    };
    Ok(Slot {
        parent,
        parent_path,
        name: base.as_bytes().to_vec(),
    })
}

/// The host counterpart of [`guest_slot`].
fn host_slot(dest: &Path, name: &OsStr) -> PathBuf {
    if fs::symlink_metadata(dest).is_ok_and(|m| m.is_dir()) {
        dest.join(name)
    } else {
        dest.to_path_buf()
    }
}

/// A guest directory under `parent`, made if absent (an existing
/// directory is reused, like `cp -r` into a partial tree).
fn guest_dir(c: &Client, parent: NodeId, name: &[u8], perm: u32) -> Result<NodeId> {
    match c.mkdir(parent, name, perm) {
        Ok((node, _)) => Ok(node),
        Err(e) => match c.lookup(parent, name) {
            Ok((node, a)) if a.kind == FileKind::Dir => Ok(node),
            _ => Err(e).context(format!("can't create {}", String::from_utf8_lossy(name))),
        },
    }
}

/// `src` (a host file or directory) into the guest at `dest`.
pub(crate) fn copy_in(c: &Client, src: &Path, dest: &str) -> Result<()> {
    let meta = fs::metadata(src).with_context(|| format!("can't read {}", src.display()))?;
    if !meta.is_dir() {
        return fscli::put(c, src, dest);
    }
    let name = src
        .file_name()
        .context("source directory has no name")?
        .as_bytes()
        .to_vec();
    let slot = guest_slot(c, dest, &name)?;
    let mut skipped = Skipped::default();
    let dir = guest_dir(
        c,
        slot.parent,
        &slot.name,
        meta.permissions().mode() & 0o777,
    )?;
    walk_in(c, src, dir, 1, &mut skipped)?;
    skipped.report();
    Ok(())
}

fn walk_in(c: &Client, src: &Path, dir: NodeId, depth: usize, skipped: &mut Skipped) -> Result<()> {
    if depth > MAX_DEPTH {
        bail!("{} is nested too deeply", src.display());
    }
    let mut entries: Vec<_> = fs::read_dir(src)
        .with_context(|| format!("can't list {}", src.display()))?
        .collect::<std::io::Result<_>>()?;
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let path = e.path();
        let name = e.file_name();
        let meta = fs::symlink_metadata(&path)?;
        let ft = meta.file_type();
        let perm = meta.permissions().mode() & 0o777;
        if ft.is_dir() {
            let child = guest_dir(c, dir, name.as_bytes(), perm)?;
            walk_in(c, &path, child, depth + 1, skipped)?;
        } else if ft.is_symlink() {
            let target = fs::read_link(&path)?;
            c.symlink(dir, name.as_bytes(), target.as_os_str().as_bytes())
                .with_context(|| format!("can't link {}", path.display()))?;
        } else if ft.is_file() {
            let f = File::open(&path).with_context(|| format!("can't open {}", path.display()))?;
            fscli::upload(c, dir, name.as_bytes(), Some(&f), perm)
                .with_context(|| format!("can't copy {}", path.display()))?;
        } else {
            skipped.special.push(path.display().to_string());
        }
    }
    Ok(())
}

/// Guest `src` (file or directory) to the host at `dest`.
pub(crate) fn copy_out(c: &Client, src: &str, dest: &Path) -> Result<()> {
    let r = c.resolve(src.as_bytes(), true)?;
    if r.attr.kind != FileKind::Dir {
        return fscli::get(c, src, Some(dest));
    }
    let name = r.path.last().map_or(&b"root"[..], Vec::as_slice);
    let to = host_slot(dest, OsStr::from_bytes(name));
    let mut skipped = Skipped::default();
    host_dir(&to)?;
    walk_out(c, r.node, &to, &mut Vec::new(), &mut skipped)?;
    finish_dir(&to, &r.attr)?;
    skipped.report();
    Ok(())
}

/// A real host directory at `path`, made if absent — never a symlink.
fn host_dir(path: &Path) -> Result<()> {
    match fs::create_dir(path) {
        Ok(()) => {}
        Err(e) if e.kind() == ErrorKind::AlreadyExists => {
            if !fs::symlink_metadata(path).is_ok_and(|m| m.is_dir()) {
                bail!("{} exists and isn't a directory", path.display());
            }
        }
        Err(e) => return Err(e).with_context(|| format!("can't create {}", path.display())),
    }
    // Writable while we fill it; the guest's mode lands afterwards.
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

/// The guest directory's permission bits, minus setuid/setgid/sticky.
fn finish_dir(path: &Path, attr: &Attr) -> Result<()> {
    fs::set_permissions(path, fs::Permissions::from_mode(attr.perm & 0o777))
        .with_context(|| format!("can't set the mode of {}", path.display()))
}

/// `rel` holds the names from the copied tree's root down to `dir`.
fn walk_out(
    c: &Client,
    dir: NodeId,
    to: &Path,
    rel: &mut Vec<Vec<u8>>,
    skipped: &mut Skipped,
) -> Result<()> {
    if rel.len() >= MAX_DEPTH {
        bail!("{} is nested too deeply", to.display());
    }
    for e in c.readdir_all(dir)? {
        // Names are single validated components (no `/`, `.`, `..`).
        let path = to.join(OsStr::from_bytes(&e.name));
        match e.attr.kind {
            FileKind::Dir => {
                let (node, attr) = c.lookup(dir, &e.name)?;
                host_dir(&path)?;
                rel.push(e.name.clone());
                walk_out(c, node, &path, rel, skipped)?;
                rel.pop();
                finish_dir(&path, &attr)?;
            }
            FileKind::File => {
                let (node, attr) = c.lookup(dir, &e.name)?;
                let f = OpenOptions::new()
                    .write(true)
                    .create(true)
                    .truncate(true)
                    .mode(attr.perm & 0o777)
                    .custom_flags(libc::O_NOFOLLOW)
                    .open(&path)
                    .with_context(|| format!("can't write {}", path.display()))?;
                f.set_permissions(fs::Permissions::from_mode(attr.perm & 0o777))?;
                fscli::download(c, node, &attr, f)
                    .with_context(|| format!("can't copy to {}", path.display()))?;
            }
            FileKind::Symlink => {
                let (node, _) = c.lookup(dir, &e.name)?;
                let target = c.readlink_raw(node)?;
                if stays_inside(rel, &target) {
                    std::os::unix::fs::symlink(OsStr::from_bytes(&target), &path)
                        .with_context(|| format!("can't link {}", path.display()))?;
                } else {
                    skipped.links.push(path.display().to_string());
                }
            }
            _ => skipped.special.push(path.display().to_string()),
        }
    }
    Ok(())
}

/// Whether a link whose directory sits at `rel` (names from the copied
/// root) can only resolve inside the copied tree. Accepted: relative
/// targets of the canonical form `../…/name/…` — every `..` before any
/// name, and no more of them than `rel` is deep. A lexical check alone
/// isn't enough: with `y -> .` at the root, `y/../etc` reads as inside
/// but climbs out through `y`. Here `..` only ever climbs real copied
/// directories, and every name — a link included, by the same rule —
/// lands inside, so whole chains stay in.
fn stays_inside(rel: &[Vec<u8>], target: &[u8]) -> bool {
    if target.starts_with(b"/") || target.is_empty() {
        return false;
    }
    let (mut ups, mut named) = (0, false);
    for part in target.split(|&b| b == b'/') {
        match part {
            b"" | b"." => {}
            b".." if named => return false,
            b".." => ups += 1,
            _ => named = true,
        }
    }
    ups <= rel.len()
}

/// Guest `src` to guest `dest` in the same session: files by
/// server-side `Copy` (reflinked where possible), links verbatim.
pub(crate) fn copy_within(c: &Client, src: &str, dest: &str) -> Result<()> {
    let r = c.resolve(src.as_bytes(), true)?;
    if r.attr.kind != FileKind::Dir {
        return fscli::cp(c, src, dest);
    }
    let name = r.path.last().map_or(&b"root"[..], Vec::as_slice);
    let slot = guest_slot(c, dest, name)?;
    // Both paths are resolved (links followed), so a prefix means the
    // destination is the source or inside it — a copy that never ends.
    if slot.parent_path.starts_with(&r.path) {
        bail!("can't copy {src} into itself");
    }
    let mut skipped = Skipped::default();
    let to = guest_dir(c, slot.parent, &slot.name, r.attr.perm & 0o7777)?;
    walk_within(c, r.node, to, 1, &mut skipped)?;
    skipped.report();
    Ok(())
}

fn walk_within(
    c: &Client,
    from: NodeId,
    to: NodeId,
    depth: usize,
    skipped: &mut Skipped,
) -> Result<()> {
    if depth > MAX_DEPTH {
        bail!("tree is nested too deeply");
    }
    for e in c.readdir_all(from)? {
        let (node, attr) = c.lookup(from, &e.name)?;
        let shown = String::from_utf8_lossy(&e.name).into_owned();
        match attr.kind {
            FileKind::Dir => {
                let child = guest_dir(c, to, &e.name, attr.perm & 0o7777)?;
                walk_within(c, node, child, depth + 1, skipped)?;
            }
            FileKind::File => {
                c.copy(node, to, &e.name, 0)
                    .with_context(|| format!("can't copy {shown}"))?;
            }
            FileKind::Symlink => {
                let target = c.readlink_raw(node)?;
                c.symlink(to, &e.name, &target)
                    .with_context(|| format!("can't link {shown}"))?;
            }
            _ => skipped.special.push(shown),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rel(parts: &[&str]) -> Vec<Vec<u8>> {
        parts.iter().map(|p| p.as_bytes().to_vec()).collect()
    }

    #[test]
    fn test_stays_inside_the_copied_tree() {
        assert!(stays_inside(&rel(&[]), b"sibling"));
        assert!(stays_inside(&rel(&["a", "b"]), b"../../top"));
        assert!(stays_inside(&rel(&["a"]), b"./../x/./y"));
        assert!(!stays_inside(&rel(&[]), b"../outside"));
        // `..` after a name could climb out through a link (`y -> .`).
        assert!(!stays_inside(&rel(&["a"]), b"y/../etc"));
        assert!(!stays_inside(&rel(&["a"]), b"./x/../y"));
        assert!(!stays_inside(&rel(&["a"]), b"../../etc/passwd"));
        assert!(!stays_inside(&rel(&["a"]), b"/etc/passwd"));
        assert!(!stays_inside(&rel(&[]), b""));
    }
}
