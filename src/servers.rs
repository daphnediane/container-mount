/*
 * Copyright (c) 2026 Daphne Pfister
 * SPDX-License-Identifier: BSD-2-Clause
 * See LICENSE file for full license text
 */

//! Locating guest server binaries — installed release copies and
//! in-tree builds — and reporting their versions without running them.
//!
//! A `distrofs` binary carries a scannable stamp
//! (`distrofs-version:<ver>:protocol:<n>\0`, see `main.rs`'s
//! `VERSION_STAMP`), so `servers`/`server-path` can report exactly what
//! would be deployed. Installed copies live in
//! `<prefix>/libexec/distrofs/distrofs-server-<arch>` (where `<prefix>`
//! is derived from the running binary the way `layout.rs` does for
//! container-distro); under `--dev`, in-tree musl target dirs are
//! searched first.

use std::env;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

/// The stamp embedded in every `distrofs` binary; followed by
/// `<pkg-version>:protocol:<n>\0`.
pub const STAMP_MARK: &[u8] = b"distrofs-version:";
/// Cap on how much of a candidate binary we read looking for a stamp.
const SCAN_MAX: u64 = 32 << 20;

/// A guest CPU architecture the server can be built for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arch {
    Aarch64,
    X86_64,
}

impl Arch {
    /// `arm64`/`aarch64`, `amd64`/`x86_64` all parse.
    pub fn parse(s: &str) -> Option<Arch> {
        Some(match s {
            "aarch64" | "arm64" => Arch::Aarch64,
            "x86_64" | "amd64" => Arch::X86_64,
            _ => return None,
        })
    }

    /// The Rust musl target triple.
    #[must_use]
    pub fn target(self) -> &'static str {
        match self {
            Arch::Aarch64 => "aarch64-unknown-linux-musl",
            Arch::X86_64 => "x86_64-unknown-linux-musl",
        }
    }

    /// The `distrofs-server-<suffix>` file name.
    #[must_use]
    pub fn suffix(self) -> &'static str {
        match self {
            Arch::Aarch64 => "aarch64",
            Arch::X86_64 => "x86_64",
        }
    }

    /// The common name (`arm64`, `amd64`).
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Arch::Aarch64 => "arm64",
            Arch::X86_64 => "amd64",
        }
    }

    pub const ALL: [Arch; 2] = [Arch::Aarch64, Arch::X86_64];
}

/// A server binary found on disk.
#[derive(Debug, Clone)]
pub struct ServerBuild {
    pub arch: Arch,
    pub path: PathBuf,
    /// The package version from its stamp, when readable.
    pub version: Option<String>,
    /// The wire protocol version from its stamp, when readable.
    pub protocol: Option<u16>,
}

/// The running binary, canonicalized.
fn current_exe() -> PathBuf {
    let exe = env::current_exe().unwrap_or_else(|_| PathBuf::from("distrofs"));
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
/// `<prefix>/libexec/distrofs`, or `exe`'s own directory when it sits
/// in such a directory itself.
#[must_use]
pub fn server_dir_for(exe: &Path) -> Option<PathBuf> {
    let dir = exe.parent()?;
    if dir.file_name() == Some(OsStr::new("distrofs")) {
        return Some(dir.to_path_buf());
    }
    prefix_of(exe).map(|p| p.join("libexec").join("distrofs"))
}

/// `distrofs-server-<arch>` in each installed location, plus — under
/// `--dev` — the in-tree musl target dirs first.
fn candidates(arch: Arch, dev: bool) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if dev && let Some(root) = option_env!("CARGO_MANIFEST_DIR").map(PathBuf::from) {
        for profile in ["release", "debug"] {
            out.push(
                root.join("target")
                    .join(arch.target())
                    .join(profile)
                    .join("distrofs"),
            );
        }
    }
    if let Some(dir) = server_dir_for(&current_exe()) {
        out.push(dir.join(format!("distrofs-server-{}", arch.suffix())));
    }
    out
}

/// Read the version stamp out of a binary's bytes.
/// `Some((version, protocol))`; either half may be absent.
#[must_use]
pub fn scan_stamp(bytes: &[u8]) -> Option<(String, Option<u16>)> {
    let pos = bytes
        .windows(STAMP_MARK.len())
        .position(|w| w == STAMP_MARK)?;
    let rest = &bytes[pos + STAMP_MARK.len()..];
    let end = rest
        .iter()
        .position(|&b| !(0x21..=0x7e).contains(&b))
        .unwrap_or(rest.len());
    let stamp = std::str::from_utf8(&rest[..end]).ok()?;
    let (version, tail) = stamp.split_once(':')?;
    let protocol = tail
        .strip_prefix("protocol:")?
        .split(':')
        .next()?
        .parse()
        .ok();
    Some((version.to_string(), protocol))
}

/// One [`ServerBuild`] per architecture whose binary exists.
/// `dev` puts in-tree musl builds ahead of installed copies.
#[must_use]
pub fn servers(dev: bool) -> Vec<ServerBuild> {
    Arch::ALL
        .iter()
        .filter_map(|&arch| {
            let path = candidates(arch, dev).into_iter().find(|p| p.is_file())?;
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
                arch,
                path,
                version,
                protocol,
            })
        })
        .collect()
}

/// The best server binary for `arch`, or an error naming what was searched.
pub fn server_path(arch: Arch, dev: bool) -> Result<PathBuf> {
    let tried = candidates(arch, dev);
    for p in &tried {
        if p.is_file() {
            return Ok(p.clone());
        }
    }
    bail!(
        "no distrofs server for {} — run `distrofs fetch-servers` to download one, or \
         build {} (tried: {})",
        arch.label(),
        arch.target(),
        tried
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    )
}

/// Where [`server_path`] for every arch would look — for `servers -v`.
#[must_use]
pub fn search_paths(dev: bool) -> Vec<PathBuf> {
    Arch::ALL.iter().flat_map(|&a| candidates(a, dev)).collect()
}

/// Validate `path` names a distrofs build: it must carry the stamp or
/// be explicitly trusted by the caller.
pub fn stamped(path: &Path) -> Result<(String, Option<u16>)> {
    let bytes = std::fs::read(path).with_context(|| format!("cannot read {}", path.display()))?;
    scan_stamp(&bytes).with_context(|| {
        format!(
            "{} carries no distrofs version stamp — is it a distrofs build?",
            path.display()
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_arch_parse() {
        assert_eq!(Arch::parse("arm64"), Some(Arch::Aarch64));
        assert_eq!(Arch::parse("aarch64"), Some(Arch::Aarch64));
        assert_eq!(Arch::parse("amd64"), Some(Arch::X86_64));
        assert_eq!(Arch::parse("x86_64"), Some(Arch::X86_64));
        assert_eq!(Arch::parse("riscv64"), None);
    }

    #[test]
    fn test_scan_stamp() {
        let blob = b"\x7fELF stuff distrofs-version:0.1.0-dev:protocol:1\0more";
        assert_eq!(scan_stamp(blob), Some(("0.1.0-dev".to_string(), Some(1))));
        assert_eq!(scan_stamp(b"nothing here"), None);
        // A corrupt suffix still yields the version half.
        let bad = b"distrofs-version:1.2.3:junk";
        assert_eq!(scan_stamp(bad), None);
    }
}
