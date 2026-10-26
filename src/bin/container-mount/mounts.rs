/*
 * Copyright (c) 2026 Daphne Pfister
 * SPDX-License-Identifier: BSD-2-Clause
 * See LICENSE file for full license text
 */

//! Mount-crossing policy: which filesystems the server presents.
//!
//! The server runs in the container's mount namespace, so besides the
//! rootfs it sees the virtiofs host shares and the pseudo filesystems.
//! Only local filesystem types are served; any other mountpoint is
//! presented as an empty read-only directory (no serving host
//! directories back to the host, no `/proc` in Finder).

use std::collections::HashMap;

/// Filesystem types whose contents are served.
const SERVED: &[&str] = &[
    "ext2", "ext3", "ext4", "xfs", "btrfs", "f2fs", "tmpfs", "ramfs", "overlay", "squashfs",
    "vfat", "exfat",
];

/// Whether a filesystem type's contents are served.
#[must_use]
pub fn served(fstype: &str) -> bool {
    SERVED.contains(&fstype)
}

/// `(major, minor)` → filesystem type, from `/proc/self/mountinfo`.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Table {
    fs: HashMap<(u32, u32), String>,
}

impl Table {
    /// Parse mountinfo text (proc(5)): field 3 is `major:minor`, and the
    /// filesystem type follows the ` - ` separator. Malformed lines are
    /// skipped.
    #[must_use]
    pub fn parse(text: &str) -> Table {
        let mut fs = HashMap::new();
        for line in text.lines() {
            let mut fields = line.split(' ');
            let Some(dev) = fields.nth(2) else { continue };
            let Some((maj, min)) = dev.split_once(':') else {
                continue;
            };
            let (Ok(maj), Ok(min)) = (maj.parse(), min.parse()) else {
                continue;
            };
            let Some(fstype) = fields.skip_while(|f| *f != "-").nth(1) else {
                continue;
            };
            fs.insert((maj, min), fstype.to_string());
        }
        Table { fs }
    }

    #[must_use]
    pub fn fstype(&self, major: u32, minor: u32) -> Option<&str> {
        self.fs.get(&(major, minor)).map(String::as_str)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
22 1 254:16 / / rw,relatime - ext4 /dev/vdb rw
23 22 0:5 / /dev rw,nosuid - devtmpfs devtmpfs rw,size=4k
24 22 0:21 / /proc rw,nosuid,nodev,noexec - proc proc rw
25 22 0:30 / /Users/dp rw,relatime shared:3 master:1 - virtiofs ab12cd rw
26 22 0:31 / /run rw,nosuid,nodev - tmpfs tmpfs rw,mode=755
garbage line
27 22 x:y / /bad rw - ext4 /dev/x rw
";

    #[test]
    fn test_parse_mountinfo() {
        let t = Table::parse(SAMPLE);
        assert_eq!(t.fstype(254, 16), Some("ext4"));
        assert_eq!(t.fstype(0, 21), Some("proc"));
        // Optional fields (shared:/master:) sit before the separator.
        assert_eq!(t.fstype(0, 30), Some("virtiofs"));
        assert_eq!(t.fstype(0, 31), Some("tmpfs"));
        assert_eq!(t.fstype(9, 9), None);
    }

    #[test]
    fn test_served_policy() {
        for ok in ["ext4", "tmpfs", "overlay", "btrfs"] {
            assert!(served(ok), "{ok}");
        }
        for hidden in [
            "virtiofs", "proc", "sysfs", "devtmpfs", "devpts", "cgroup2", "fuse",
        ] {
            assert!(!served(hidden), "{hidden}");
        }
    }
}
