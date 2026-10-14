/*
 * Copyright (c) 2026 Daphne Pfister
 * SPDX-License-Identifier: BSD-2-Clause
 * See LICENSE file for full license text
 */

//! `distrofs update` and `distrofs fetch-servers`.
//!
//! `update` self-updates an installed `distrofs` to a GitHub release:
//! fetch the release, compare versions (consent for a downgrade or
//! reinstall), download the `distrofs-<triple>-v<ver>.tar.gz` asset,
//! verify the API-reported SHA-256 and — on macOS — that the binary
//! satisfies `anchor apple generic` under a Developer ID whose team
//! matches the installed copy's, then swap it into place (plus any
//! server binaries it carries). It never self-elevates and refuses to
//! run as root.
//!
//! `fetch-servers` is the package-installer path: download just the
//! `distrofs-servers-<ver>.tar.gz` asset into the libexec dir so a
//! `cargo install`/`cargo binstall` user gets guest servers without
//! building them.
//!
//! Both go through [`distrofs::release`]; `DISTROFS_RELEASES_URL`
//! applies only under `--dev` (see there for the security rule).

use std::cmp::Ordering;
use std::ffi::OsStr;
#[cfg(target_os = "macos")]
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
#[cfg(target_os = "macos")]
use std::process::Command;

use anyhow::{Context, Result, bail};
use distrofs::release::{
    Asset, cmp_version, consent, download, fetch_release, find_asset, is_root, untar,
};
use distrofs::servers;

/// Our code-signing identifier — what releases sign the binary as.
#[cfg(target_os = "macos")]
const IDENTIFIER: &str = "io.github.daphnediane.distrofs";

/// The `rustc --print target` triple for this build.
fn host_triple() -> &'static str {
    match (std::env::consts::ARCH, std::env::consts::OS) {
        ("aarch64", "macos") => "aarch64-apple-darwin",
        ("x86_64", "macos") => "x86_64-apple-darwin",
        ("x86_64", "linux") => "x86_64-unknown-linux-gnu",
        ("aarch64", "linux") => "aarch64-unknown-linux-gnu",
        _ => "unknown",
    }
}

/// The running binary, canonicalized — the thing `update` replaces.
fn current_exe() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("cannot locate the running binary")?;
    Ok(exe.canonicalize().unwrap_or(exe))
}

/// `distrofs update [--version V] [--dry-run] [--force]`.
pub fn run(args: &[String], dev: bool) -> Result<()> {
    let mut version = None;
    let mut dry_run = false;
    let mut force = false;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--version" => version = Some(it.next().context("--version needs a value")?.clone()),
            "--dry-run" | "-n" => dry_run = true,
            "--force" | "-f" => force = true,
            _ => bail!("unknown argument `{a}` (update wants [--version V] [--dry-run] [--force])"),
        }
    }
    update(version.as_deref(), force, dry_run, dev)
}

fn update(version: Option<&str>, force: bool, dry_run: bool, dev: bool) -> Result<()> {
    if is_root() {
        bail!("run without sudo — distrofs never self-elevates");
    }
    let exe = current_exe()?;
    // A copy managed by another package (container-distro's plugin
    // libexec dir) must not be swapped out from under it.
    if exe
        .components()
        .any(|c| c.as_os_str() == "container-plugins")
    {
        bail!(
            "{} lives in the container plugins directory — that copy belongs to \
             `container distro` and is replaced by it, not by distrofs update",
            exe.display()
        );
    }
    if exe
        .parent()
        .is_none_or(|d| d.file_name() != Some(OsStr::new("bin")))
    {
        consent(
            &format!(
                "{} isn't installed under a `bin` directory (cargo install, an \
                 install prefix) — updating will still replace just this binary",
                exe.display()
            ),
            "Update it anyway? [y/N] ",
            force,
        )?;
    }

    let (tag, release) = fetch_release(version, dev)?;
    let current = env!("CARGO_PKG_VERSION");
    match cmp_version(current, &tag) {
        Ordering::Equal if !force => {
            println!("distrofs {tag} is already installed (--force to reinstall)");
            return Ok(());
        }
        Ordering::Equal => eprintln!("reinstalling distrofs {tag}"),
        Ordering::Greater => consent(
            &format!("{tag} is older than the installed {current}"),
            &format!("Downgrade distrofs to {tag}? [y/N] "),
            force,
        )?,
        Ordering::Less => eprintln!("updating distrofs {current} -> {tag}"),
    }

    let asset = pick_client_asset(&release, &tag, force)?;
    let dir = download_dir()?;
    let tarball = download(&asset, &dir)?;

    // Extract into a private temp dir next to the cache.
    let work = dir.join(format!(".extract-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&work);
    untar(&tarball, &work)?;
    let unpacked = one_dir(&work)?;
    let new_bin = unpacked.join("distrofs");
    if !new_bin.is_file() {
        let _ = std::fs::remove_dir_all(&work);
        bail!("{} didn't contain a distrofs binary", asset.name);
    }

    // The stamp proves it's a distrofs build; on macOS codesign proves
    // provenance, and the team must match the installed copy's.
    let (ver, _) = servers::stamped(&new_bin)?;
    check_new_binary(&new_bin, &exe, force)?;
    let mut servers_ok = Vec::new();
    for arch in servers::Arch::ALL {
        let s = unpacked.join(format!("distrofs-server-{}", arch.suffix()));
        if s.is_file() {
            servers::stamped(&s)
                .with_context(|| format!("{} failed its version-stamp check", s.display()))?;
            servers_ok.push(s);
        }
    }

    if dry_run {
        println!(
            "dry run — would replace {} with {} (distrofs {ver}){}",
            exe.display(),
            asset.name,
            if servers_ok.is_empty() {
                "".into()
            } else {
                format!(" and install {} server binaries", servers_ok.len())
            }
        );
        let _ = std::fs::remove_dir_all(&work);
        return Ok(());
    }

    install(&exe, &new_bin, &servers_ok)?;
    let _ = std::fs::remove_dir_all(&work);
    println!("updated {} to distrofs {tag}", exe.display());
    Ok(())
}

/// `distrofs fetch-servers [--version V] [--prefix DIR]` — install the
/// release's server binaries into `<prefix>/libexec/distrofs` (default:
/// the running binary's own install dir).
pub fn fetch_servers(args: &[String], dev: bool) -> Result<()> {
    let mut version = None;
    let mut prefix = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--version" => version = Some(it.next().context("--version needs a value")?.clone()),
            "--prefix" => {
                prefix = Some(PathBuf::from(it.next().context("--prefix needs a value")?))
            }
            _ => bail!("unknown argument `{a}` (fetch-servers wants [--version V] [--prefix DIR])"),
        }
    }
    let (tag, rel) = fetch_release(version.as_deref(), dev)?;
    let name = format!("distrofs-servers-{tag}.tar.gz");
    let asset =
        find_asset(&rel, &name).with_context(|| format!("release {tag} has no {name} asset"))?;
    let dir = download_dir()?;
    let tarball = download(&asset, &dir)?;

    let work = dir.join(format!(".servers-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&work);
    untar(&tarball, &work)?;
    let unpacked = one_dir(&work)?;

    let dest = match prefix {
        Some(p) => p.join("libexec").join("distrofs"),
        None => servers::server_dir_for(&current_exe()?).with_context(|| {
            "this distrofs has no install prefix — pass --prefix DIR \
             (servers land in DIR/libexec/distrofs)"
                .to_string()
        })?,
    };
    std::fs::create_dir_all(&dest).with_context(|| format!("cannot create {}", dest.display()))?;

    let mut installed = 0;
    for arch in servers::Arch::ALL {
        let s = unpacked.join(format!("distrofs-server-{}", arch.suffix()));
        if !s.is_file() {
            continue;
        }
        let (v, p) = servers::stamped(&s)
            .with_context(|| format!("{} failed its version-stamp check", s.display()))?;
        let to = dest.join(s.file_name().unwrap());
        install_file(&s, &to)?;
        installed += 1;
        println!(
            "installed {} ({v}, protocol {})",
            to.display(),
            p.unwrap_or(0)
        );
    }
    let _ = std::fs::remove_dir_all(&work);
    if installed == 0 {
        bail!("{name} contained no distrofs-server-* binaries");
    }
    Ok(())
}

/// Where downloads are cached: the per-user Darwin cache on macOS,
/// else a per-user dir under the temp dir.
fn download_dir() -> Result<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        let mut buf = vec![0u8; 1024];
        // SAFETY: `buf` is writable for its length.
        let n = unsafe {
            libc::confstr(
                libc::_CS_DARWIN_USER_CACHE_DIR,
                buf.as_mut_ptr().cast(),
                buf.len(),
            )
        };
        if n > 0 && n <= buf.len() {
            let s = std::ffi::CStr::from_bytes_until_nul(&buf).unwrap();
            return Ok(
                PathBuf::from(std::ffi::OsStr::from_bytes(s.to_bytes()).to_os_string())
                    .join("distrofs"),
            );
        }
    }
    // SAFETY: getuid cannot fail.
    Ok(std::env::temp_dir().join(format!("distrofs-{}", unsafe { libc::getuid() })))
}

/// The single top-level directory (or the dir itself) an extracted
/// tarball produced.
fn one_dir(work: &Path) -> Result<PathBuf> {
    let entries: Vec<_> = std::fs::read_dir(work)?
        .filter_map(std::result::Result::ok)
        .collect();
    match entries.as_slice() {
        [only] if only.file_type()?.is_dir() => Ok(only.path()),
        _ => Ok(work.to_path_buf()),
    }
}

/// The release's client tarball for this host — the signed asset by
/// default, the unsigned CI one only with consent (same rule as
/// container-distro's pkg pick).
fn pick_client_asset(release: &serde_json::Value, tag: &str, force: bool) -> Result<Asset> {
    let triple = host_triple();
    if triple == "unknown" {
        bail!(
            "no release assets for this platform ({}-{})",
            std::env::consts::ARCH,
            std::env::consts::OS
        );
    }
    if let Some(a) = find_asset(release, &format!("distrofs-{triple}-v{tag}.tar.gz")) {
        return Ok(a);
    }
    let unsigned = format!("distrofs-{triple}-v{tag}-unsigned.tar.gz");
    match find_asset(release, &unsigned) {
        Some(a) => {
            consent(
                &format!("release {tag} has no signed tarball — only the unsigned CI build"),
                "Install the unsigned build? [y/N] ",
                force,
            )?;
            Ok(a)
        }
        None => bail!("release {tag} has no distrofs tarball for {triple}"),
    }
}

/// `TeamIdentifier` of `bin` per `codesign -dvv`; `None` for
/// adhoc/unsigned builds ("not set") or off-macOS.
#[cfg(target_os = "macos")]
fn team_id(bin: &Path) -> Option<String> {
    let out = Command::new("/usr/bin/codesign")
        .arg("-dvv")
        .arg(bin)
        .output()
        .ok()?;
    // `codesign -dvv` writes the info to stderr.
    let text = String::from_utf8_lossy(&out.stderr);
    let id = text
        .lines()
        .find_map(|l| l.strip_prefix("TeamIdentifier=").map(str::trim))?;
    (id != "not set").then(|| id.to_string())
}

/// Codesign rules for a downloaded binary on macOS: it must satisfy
/// `identifier … and anchor apple generic`, and when the installed
/// copy has a team id the download's must match.
#[cfg(target_os = "macos")]
fn check_new_binary(new: &Path, installed: &Path, force: bool) -> Result<()> {
    let status = Command::new("/usr/bin/codesign")
        .arg("-v")
        .arg(format!(
            "-R=identifier \"{IDENTIFIER}\" and anchor apple generic"
        ))
        .arg(new)
        .status();
    match status {
        Ok(s) if s.success() => {}
        _ => {
            consent(
                &format!(
                    "{} is not signed as {IDENTIFIER} under a Developer ID — \
                     an unsigned/adhoc build will only run where gatekeeper allows it",
                    new.display()
                ),
                "Use it anyway? [y/N] ",
                force,
            )?;
            return Ok(());
        }
    }
    if let (Some(have), Some(got)) = (team_id(installed), team_id(new))
        && have != got
    {
        consent(
            &format!("the download is signed by team {got}, the installed copy by {have}"),
            "Install it anyway? [y/N] ",
            force,
        )?;
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn check_new_binary(_new: &Path, _installed: &Path, _force: bool) -> Result<()> {
    Ok(())
}

/// Replace `dest` with `src` (same filesystem), mode 0755.
fn install_file(src: &Path, dest: &Path) -> Result<()> {
    let tmp = dest.with_extension("part");
    std::fs::copy(src, &tmp).with_context(|| format!("cannot stage {}", dest.display()))?;
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;
    std::fs::rename(&tmp, dest).with_context(|| format!("cannot install {}", dest.display()))?;
    Ok(())
}

/// Swap the running binary for `new` and install any server binaries
/// into the exe's own server dir.
fn install(exe: &Path, new: &Path, server_bins: &[PathBuf]) -> Result<()> {
    install_file(new, exe)?;
    if !server_bins.is_empty()
        && let Some(dir) = servers::server_dir_for(exe)
    {
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("cannot create {}", dir.display()))?;
        for s in server_bins {
            install_file(s, &dir.join(s.file_name().unwrap()))?;
            eprintln!("installed {}", dir.join(s.file_name().unwrap()).display());
        }
    }
    Ok(())
}
