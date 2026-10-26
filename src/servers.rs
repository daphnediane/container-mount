/*
 * Copyright (c) 2026 Daphne Pfister
 * SPDX-License-Identifier: BSD-2-Clause
 * See LICENSE file for full license text
 */

//! Locating guest server binaries — installed release copies and
//! in-tree builds — and reporting their versions without running them.
//!
//! A `container-mount` binary carries a scannable stamp
//! (`container-mount-version:<ver>:protocol:<n>\0`, see `main.rs`'s
//! `VERSION_STAMP`), so `servers`/`server-path` can report exactly what
//! would be deployed. Installed copies live in
//! `<prefix>/libexec/container-mount/<triple>/container-mount` (where
//! `<prefix>` is derived from the running binary the way `layout.rs`
//! does for container-distro), mirroring the in-tree
//! `target/<triple>/<profile>/container-mount` builds that `--dev`
//! searches first (docs/design/install-layout.md).

use std::env;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

/// The stamp embedded in every `container-mount` binary; followed by
/// `<pkg-version>:protocol:<n>\0`.
pub const STAMP_MARK: &[u8] = b"container-mount-version:";
/// Cap on how much of a candidate binary we read looking for a stamp.
const SCAN_MAX: u64 = 32 << 20;

/// A guest platform the server can be built for — one row per Rust
/// target triple (docs/design/install-layout.md "Guest platforms").
/// Rows for other guest OSes join with their `serve` ports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    LinuxArm64,
    LinuxAmd64,
}

/// The server's file name inside its `<triple>/` directory.
pub const SERVER_BINARY: &str = "container-mount";

impl Platform {
    pub const ALL: [Platform; 2] = [Platform::LinuxArm64, Platform::LinuxAmd64];

    /// The triple, the label, or an alias: `arm64`/`aarch64` and
    /// `amd64`/`x86_64` name the linux-musl rows.
    pub fn parse(s: &str) -> Option<Platform> {
        Platform::ALL
            .into_iter()
            .find(|p| s == p.triple() || s == p.label() || p.aliases().contains(&s))
    }

    /// The Rust target triple — also the install subdirectory name.
    #[must_use]
    pub fn triple(self) -> &'static str {
        match self {
            Platform::LinuxArm64 => "aarch64-unknown-linux-musl",
            Platform::LinuxAmd64 => "x86_64-unknown-linux-musl",
        }
    }

    /// The display label (`linux-arm64`).
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Platform::LinuxArm64 => "linux-arm64",
            Platform::LinuxAmd64 => "linux-amd64",
        }
    }

    fn aliases(self) -> &'static [&'static str] {
        match self {
            Platform::LinuxArm64 => &["arm64", "aarch64"],
            Platform::LinuxAmd64 => &["amd64", "x86_64"],
        }
    }
}

/// `<dir>/<triple>/container-mount` — where a server for `platform`
/// lives under a servers directory (an install's
/// `libexec/container-mount`, or an unpacked servers bundle).
#[must_use]
pub fn installed_path(dir: &Path, platform: Platform) -> PathBuf {
    dir.join(platform.triple()).join(SERVER_BINARY)
}

/// A server binary found on disk.
#[derive(Debug, Clone)]
pub struct ServerBuild {
    pub platform: Platform,
    pub path: PathBuf,
    /// The package version from its stamp, when readable.
    pub version: Option<String>,
    /// The wire protocol version from its stamp, when readable.
    pub protocol: Option<u16>,
}

/// The running binary, canonicalized.
fn current_exe() -> PathBuf {
    let exe = env::current_exe().unwrap_or_else(|_| PathBuf::from("container-mount"));
    exe.canonicalize().unwrap_or(exe)
}

/// The install prefix of `exe`: the parent of the `bin` directory it
/// lives in, or `None` outside one (dev builds, bare paths).
fn prefix_of(exe: &Path) -> Option<PathBuf> {
    let dir = exe.parent()?;
    (dir.file_name() == Some(OsStr::new("bin")))
        .then(|| dir.parent().map(Path::to_path_buf))
        .flatten()
}

/// The install prefix of `exe`: the parent of the `bin` directory it
/// lives in, or `None` outside one (dev builds, bare paths). Public so
/// `update`/`fetch-servers` can derive install dirs the same way.
#[must_use]
pub fn prefix_of_pub(exe: &Path) -> Option<PathBuf> {
    prefix_of(exe)
}

/// The directory installed servers live in for `exe`:
/// `<prefix>/libexec/container-mount`, or — when `exe` is itself a
/// server at `…/container-mount/<triple>/container-mount` — that
/// servers directory.
#[must_use]
pub fn server_dir_for(exe: &Path) -> Option<PathBuf> {
    let triple_dir = exe.parent()?;
    let servers = triple_dir.parent()?;
    if servers.file_name() == Some(OsStr::new("container-mount"))
        && triple_dir
            .file_name()
            .and_then(OsStr::to_str)
            .is_some_and(|t| Platform::ALL.iter().any(|p| p.triple() == t))
    {
        return Some(servers.to_path_buf());
    }
    prefix_of(exe).map(|p| p.join("libexec").join("container-mount"))
}

/// `<triple>/container-mount` in each installed location, plus — under
/// `--dev` — the in-tree musl target dirs first.
fn candidates(platform: Platform, dev: bool) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if dev && let Some(root) = option_env!("CARGO_MANIFEST_DIR").map(PathBuf::from) {
        for profile in ["release", "debug"] {
            out.push(
                root.join("target")
                    .join(platform.triple())
                    .join(profile)
                    .join(SERVER_BINARY),
            );
        }
    }
    if let Some(dir) = server_dir_for(&current_exe()) {
        out.push(installed_path(&dir, platform));
    }
    out
}

/// Read the version stamp out of a binary's bytes.
/// `Some((version, protocol))`; either half may be absent.
///
/// Every occurrence of [`STAMP_MARK`] is tried: the mark's own string
/// constant is in the binary too, and a release link can place it —
/// followed by whatever strings the linker merged after it — ahead of
/// the real stamp.
#[must_use]
pub fn scan_stamp(bytes: &[u8]) -> Option<(String, Option<u16>)> {
    bytes
        .windows(STAMP_MARK.len())
        .enumerate()
        .filter(|(_, w)| *w == STAMP_MARK)
        .find_map(|(pos, _)| parse_stamp(&bytes[pos + STAMP_MARK.len()..]))
}

/// `<version>:protocol:<n>[:key:value…]` at the start of `rest`.
fn parse_stamp(rest: &[u8]) -> Option<(String, Option<u16>)> {
    let end = rest
        .iter()
        .position(|&b| !(0x21..=0x7e).contains(&b))
        .unwrap_or(rest.len());
    let stamp = std::str::from_utf8(&rest[..end]).ok()?;
    let (version, tail) = stamp.split_once(':')?;
    if !version.starts_with(|c: char| c.is_ascii_digit()) {
        return None;
    }
    let protocol = tail
        .strip_prefix("protocol:")?
        .split(':')
        .next()?
        .parse()
        .ok();
    Some((version.to_string(), protocol))
}

/// One [`ServerBuild`] per platform whose binary exists.
/// `dev` puts in-tree musl builds ahead of installed copies.
#[must_use]
pub fn servers(dev: bool) -> Vec<ServerBuild> {
    Platform::ALL
        .iter()
        .filter_map(|&platform| {
            let path = candidates(platform, dev)
                .into_iter()
                .find(|p| p.is_file())?;
            let (version, protocol) = std::fs::File::open(&path)
                .ok()
                .and_then(|f| {
                    use std::io::Read;
                    let mut buf = Vec::new();
                    f.take(SCAN_MAX).read_to_end(&mut buf).ok()?;
                    scan_stamp(&buf)
                })
                .map_or((None, None), |(v, p)| (Some(v), p));
            Some(ServerBuild {
                platform,
                path,
                version,
                protocol,
            })
        })
        .collect()
}

/// The best server binary for `platform`, or an error naming what was
/// searched.
pub fn server_path(platform: Platform, dev: bool) -> Result<PathBuf> {
    let tried = candidates(platform, dev);
    for p in &tried {
        if p.is_file() {
            return Ok(p.clone());
        }
    }
    bail!(
        "no container-mount server for {} — run `container-mount fetch-servers` to download one, or \
         build {} (tried: {})",
        platform.label(),
        platform.triple(),
        tried
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    )
}

/// Where [`server_path`] for every platform would look — for `servers -v`.
#[must_use]
pub fn search_paths(dev: bool) -> Vec<PathBuf> {
    Platform::ALL
        .iter()
        .flat_map(|&p| candidates(p, dev))
        .collect()
}

/// Validate `path` names a container-mount build: it must carry the stamp or
/// be explicitly trusted by the caller.
pub fn stamped(path: &Path) -> Result<(String, Option<u16>)> {
    let bytes = std::fs::read(path).with_context(|| format!("cannot read {}", path.display()))?;
    scan_stamp(&bytes).with_context(|| {
        format!(
            "{} carries no container-mount version stamp — is it a container-mount build?",
            path.display()
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_platform_parse() {
        use Platform::*;
        for (s, want) in [
            ("aarch64-unknown-linux-musl", Some(LinuxArm64)),
            ("linux-arm64", Some(LinuxArm64)),
            ("arm64", Some(LinuxArm64)),
            ("aarch64", Some(LinuxArm64)),
            ("x86_64-unknown-linux-musl", Some(LinuxAmd64)),
            ("linux-amd64", Some(LinuxAmd64)),
            ("amd64", Some(LinuxAmd64)),
            ("x86_64", Some(LinuxAmd64)),
            ("riscv64", None),
            ("aarch64-apple-darwin", None),
        ] {
            assert_eq!(Platform::parse(s), want, "{s}");
        }
    }

    #[test]
    fn test_server_dir_for_prefix_and_sibling() {
        assert_eq!(
            server_dir_for(Path::new("/usr/local/bin/container-mount")),
            Some(PathBuf::from("/usr/local/libexec/container-mount"))
        );
        // A server inside the servers dir finds its sibling platforms.
        assert_eq!(
            server_dir_for(Path::new(
                "/opt/x/libexec/container-mount/x86_64-unknown-linux-musl/container-mount"
            )),
            Some(PathBuf::from("/opt/x/libexec/container-mount"))
        );
        // A non-triple subdirectory is not mistaken for one.
        assert_eq!(
            server_dir_for(Path::new("/a/container-mount/junk/container-mount")),
            None
        );
        assert_eq!(
            installed_path(Path::new("/s"), Platform::LinuxArm64),
            PathBuf::from("/s/aarch64-unknown-linux-musl/container-mount")
        );
    }

    #[test]
    fn test_scan_stamp() {
        let blob = b"\x7fELF stuff container-mount-version:0.1.0-dev:protocol:1\0more";
        assert_eq!(scan_stamp(blob), Some(("0.1.0-dev".to_string(), Some(1))));
        assert_eq!(scan_stamp(b"nothing here"), None);
        // A corrupt suffix still yields the version half.
        let bad = b"container-mount-version:1.2.3:junk";
        assert_eq!(scan_stamp(bad), None);
        // Later releases append `key:value` pairs (commit, dirty —
        // docs/design/install-layout.md); this release must still read
        // version + protocol out of them.
        let newer = b"container-mount-version:0.2.0:protocol:3:commit:abc123:dirty\0";
        assert_eq!(scan_stamp(newer), Some(("0.2.0".to_string(), Some(3))));
        // The mark's own constant, merged with neighboring strings,
        // can come first (seen in release macOS builds).
        // cspell:ignore mountlibexec
        let shadowed =
            b"container-mount-version:container-mountlibexec\0..container-mount-version:0.1.0:protocol:2\0";
        assert_eq!(scan_stamp(shadowed), Some(("0.1.0".to_string(), Some(2))));
    }
}
