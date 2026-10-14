/*
 * Copyright (c) 2026 Daphne Pfister
 * SPDX-License-Identifier: BSD-2-Clause
 * See LICENSE file for full license text
 */

//! Symlink target rewriting for host presentation.
//!
//! A macOS VFS resolves symlinks itself, so a guest link
//! `/etc/x -> /Users/me/.ssh` would point into the *host*, and enough
//! `../` would climb out of the mount. Targets are therefore resolved
//! lexically with chroot semantics inside the guest root (absolute ⇒
//! from the root, `..` at the root stays there) and re-emitted as a
//! relative path from the link's directory, which can never leave the
//! volume. Lexical resolution can disagree with the guest kernel when
//! an intermediate component is itself a symlink followed by `..`; that
//! only changes *which* in-volume file is reached, never whether it
//! stays in the volume.

/// Rewrite `target` of a link living in directory `link_dir` (the
/// directory's components from the guest root) into a relative,
/// volume-confined path. An empty target stays empty (invalid anyway).
#[must_use]
pub fn confine(link_dir: &[&[u8]], target: &[u8]) -> Vec<u8> {
    if target.is_empty() {
        return Vec::new();
    }
    let mut resolved: Vec<&[u8]> = if target.first() == Some(&b'/') {
        Vec::new()
    } else {
        link_dir.to_vec()
    };
    for comp in target.split(|&b| b == b'/') {
        match comp {
            b"" | b"." => {}
            b".." => {
                resolved.pop();
            }
            c => resolved.push(c),
        }
    }
    let common = link_dir
        .iter()
        .zip(&resolved)
        .take_while(|(a, b)| a == b)
        .count();
    let mut parts: Vec<&[u8]> = vec![b".."; link_dir.len() - common];
    parts.extend_from_slice(&resolved[common..]);
    if parts.is_empty() {
        return b".".to_vec();
    }
    parts.join(&b'/')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(dir: &str, target: &str) -> String {
        let comps: Vec<&[u8]> = dir
            .split('/')
            .filter(|s| !s.is_empty())
            .map(str::as_bytes)
            .collect();
        String::from_utf8(confine(&comps, target.as_bytes())).unwrap()
    }

    #[test]
    fn test_absolute_targets_become_relative_to_the_link() {
        assert_eq!(c("/usr/bin", "/usr/lib/x"), "../lib/x");
        assert_eq!(c("/etc", "/Users/me/.ssh"), "../Users/me/.ssh");
        assert_eq!(c("/", "/etc/passwd"), "etc/passwd");
        assert_eq!(c("/a/b", "/"), "../..");
        assert_eq!(c("/a", "/a"), ".");
    }

    #[test]
    fn test_relative_targets_are_clamped_at_the_root() {
        assert_eq!(c("/a", "../../../../etc/passwd"), "../etc/passwd");
        assert_eq!(c("/", ".."), ".");
        assert_eq!(c("/a/b", "../c"), "../c");
        assert_eq!(c("/a/b", "c/./d//e"), "c/d/e");
        assert_eq!(c("/a/b", "../../.."), "../..");
    }

    #[test]
    fn test_result_never_climbs_above_the_link_depth() {
        for (dir, t) in [
            ("/x/y/z", "/../../../../../q"),
            ("/x", "../../../../.."),
            ("/", "../../a"),
        ] {
            let depth = dir.split('/').filter(|s| !s.is_empty()).count();
            let out = c(dir, t);
            let ups = out.split('/').take_while(|p| *p == "..").count();
            assert!(ups <= depth, "{dir} -> {t} gave {out}");
            assert!(!out.starts_with('/'), "{out}");
        }
    }

    #[test]
    fn test_non_utf8_components_survive() {
        let dir: Vec<&[u8]> = vec![b"d\xff"];
        assert_eq!(confine(&dir, b"/d\xff/x\xfe"), b"x\xfe".to_vec());
        assert!(confine(&dir, b"").is_empty());
    }
}
