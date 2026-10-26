/*
 * Copyright (c) 2026 Daphne Pfister
 * SPDX-License-Identifier: BSD-2-Clause
 * See LICENSE file for full license text
 */

//! `container-mount update` and `container-mount fetch-servers`.
//!
//! `update` self-updates an installed `container-mount` to a GitHub release:
//! fetch the release, compare versions (consent for a downgrade or
//! reinstall), download the `container-mount-<triple>-v<ver>.tar.gz` asset,
//! verify the API-reported SHA-256 and — on macOS — that the binary
//! satisfies `anchor apple generic` under a Developer ID whose team
//! matches the installed copy's, then swap it into place (plus any
//! server binaries it carries). It never self-elevates and refuses to
//! run as root. A pkg install — a receipt whose root holds this very
//! binary — updates through the signed `.pkg` instead (see [`pkg`]):
//! the receipt owns those root-owned files, and a tarball swap could
//! neither write them nor update the receipt.
//!
//! `fetch-servers` is the package-installer path: download just the
//! `container-mount-servers-<ver>.tar.gz` asset into the libexec dir so a
//! `cargo install`/`cargo binstall` user gets guest servers without
//! building them. It always fetches this binary's own release — client
//! and servers are lock-step (docs/design/install-layout.md).
//!
//! Both archives carry servers as `<triple>/container-mount`, the
//! layout of `libexec/container-mount/` itself.
//!
//! Both go through [`container_mount::release`]; `CONTAINER_MOUNT_RELEASES_URL`
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
use container_mount::release::{
    Asset, cmp_version, consent, download, fetch_release, find_asset, is_root, untar,
};
use container_mount::servers;

/// Our code-signing identifier — what releases sign the binary as.
#[cfg(target_os = "macos")]
const IDENTIFIER: &str = "io.github.daphnediane.container-mount";

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

/// `container-mount update [--version V] [--dry-run] [--force]`.
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
        bail!("run without sudo — container-mount never self-elevates");
    }
    let exe = current_exe()?;
    // A plugin copy (ours under container-plugins/mount, or one an
    // embedder ships) is refreshed by its installer, not swapped here.
    if exe
        .components()
        .any(|c| c.as_os_str() == "container-plugins")
    {
        bail!(
            "{} lives in the container plugins directory — update the main \
             `container-mount`, then refresh this copy with \
             `container-mount install-all` (or its embedder's installer)",
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
            println!("container-mount {tag} is already installed (--force to reinstall)");
            return Ok(());
        }
        Ordering::Equal => eprintln!("reinstalling container-mount {tag}"),
        Ordering::Greater => consent(
            &format!("{tag} is older than the installed {current}"),
            &format!("Downgrade container-mount to {tag}? [y/N] "),
            force,
        )?,
        Ordering::Less => eprintln!("updating container-mount {current} -> {tag}"),
    }
    #[cfg(target_os = "macos")]
    if pkg::owns(&exe) {
        return pkg::update(&release, &tag, &exe, force, dry_run);
    }

    let asset = pick_client_asset(&release, &tag, force)?;
    let dir = download_dir()?;
    let tarball = download(&asset, &dir)?;

    // Extract into a private temp dir next to the cache.
    let work = dir.join(format!(".extract-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&work);
    untar(&tarball, &work)?;
    let unpacked = one_dir(&work)?;
    let new_bin = unpacked.join("container-mount");
    if !new_bin.is_file() {
        let _ = std::fs::remove_dir_all(&work);
        bail!("{} didn't contain a container-mount binary", asset.name);
    }

    // The stamp proves it's a container-mount build; on macOS codesign proves
    // provenance, and the team must match the installed copy's.
    let (ver, _) = servers::stamped(&new_bin)?;
    check_new_binary(&new_bin, &exe, force)?;
    let mut servers_ok = Vec::new();
    for platform in servers::Platform::ALL {
        let s = servers::installed_path(&unpacked, platform);
        if s.is_file() {
            servers::stamped(&s)
                .with_context(|| format!("{} failed its version-stamp check", s.display()))?;
            servers_ok.push((platform, s));
        }
    }

    if dry_run {
        println!(
            "dry run — would replace {} with {} (container-mount {ver}){}",
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
    println!("updated {} to container-mount {tag}", exe.display());
    // `update` replaces only this binary; the plugin keeps its own copy.
    #[cfg(target_os = "macos")]
    if let Some(dir) = crate::plugin::install::stale_plugin(false) {
        println!(
            "the `container mount` plugin at {} is a separate copy — run \
             `container-mount install-all` to update it",
            dir.display()
        );
    }
    Ok(())
}

/// `container-mount fetch-servers [--prefix DIR]` — install this
/// release's server binaries into `<prefix>/libexec/container-mount`
/// (default: the running binary's own install dir). There is no
/// `--version`: a server from another release can't be relied on to
/// handshake with this client.
pub fn fetch_servers(args: &[String], dev: bool) -> Result<()> {
    let mut prefix = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--prefix" => {
                prefix = Some(PathBuf::from(it.next().context("--prefix needs a value")?))
            }
            _ => bail!("unknown argument `{a}` (fetch-servers wants [--prefix DIR])"),
        }
    }
    let own = env!("CARGO_PKG_VERSION");
    let (tag, rel) = fetch_release(Some(own), dev).with_context(|| {
        format!("no published release {own} to fetch servers from — `container-mount update` installs a released build")
    })?;
    let name = format!("container-mount-servers-{tag}.tar.gz");
    let asset =
        find_asset(&rel, &name).with_context(|| format!("release {tag} has no {name} asset"))?;
    let dir = download_dir()?;
    let tarball = download(&asset, &dir)?;

    let work = dir.join(format!(".servers-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&work);
    untar(&tarball, &work)?;
    let unpacked = one_dir(&work)?;

    let dest = match prefix {
        Some(p) => p.join("libexec").join("container-mount"),
        None => servers::server_dir_for(&current_exe()?).with_context(|| {
            "this container-mount has no install prefix — pass --prefix DIR \
             (servers land in DIR/libexec/container-mount)"
                .to_string()
        })?,
    };
    std::fs::create_dir_all(&dest).with_context(|| format!("cannot create {}", dest.display()))?;

    let mut installed = 0;
    for platform in servers::Platform::ALL {
        let s = servers::installed_path(&unpacked, platform);
        if !s.is_file() {
            continue;
        }
        let (v, p) = servers::stamped(&s)
            .with_context(|| format!("{} failed its version-stamp check", s.display()))?;
        let to = servers::installed_path(&dest, platform);
        install_dir_for(&to)?;
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
        bail!("{name} contained no <triple>/container-mount servers");
    }
    Ok(())
}

/// Pkg installs update through the signed `.pkg` and Installer.app,
/// like container-distro's update: download (digest-verified), check
/// the Developer ID Installer signature and its team against the
/// installed binary, then `exec` into `open -a Installer.app` — our
/// process is replaced, so nothing of ours runs while the privileged
/// install proceeds, and Installer.app collects the admin password.
#[cfg(target_os = "macos")]
mod pkg {
    use std::os::unix::process::CommandExt;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    use anyhow::{Context, Result, bail};
    use container_mount::release::{Asset, consent, download, find_asset};
    use serde_json::Value;

    /// Our pkg identifier — what `pkgutil` answers for.
    const PKG_ID: &str = "io.github.daphnediane.container-mount";
    /// Explicit `-a` target, so a remapped `.pkg` binding can't send the
    /// installer elsewhere.
    const INSTALLER_APP: &str = "/System/Library/CoreServices/Installer.app";

    /// The receipt's install root (`location:` joined under `/`; a
    /// `/` location means /usr/local, as in the uninstaller), or `None`
    /// without a receipt.
    fn receipt_root() -> Option<PathBuf> {
        let out = Command::new("/usr/sbin/pkgutil")
            .args(["--pkg-info", PKG_ID])
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        parse_root(&String::from_utf8_lossy(&out.stdout))
    }

    fn parse_root(info: &str) -> Option<PathBuf> {
        let loc = info
            .lines()
            .find_map(|l| l.strip_prefix("location:").map(str::trim))?;
        let root = PathBuf::from(format!("/{}", loc.trim_matches('/')));
        Some(if root == Path::new("/") {
            PathBuf::from("/usr/local")
        } else {
            root
        })
    }

    /// Whether `exe` is the pkg's own `bin/container-mount` — a cargo
    /// copy elsewhere keeps the tarball path even with a pkg installed.
    pub fn owns(exe: &Path) -> bool {
        receipt_root()
            .and_then(|r| std::fs::canonicalize(r.join("bin/container-mount")).ok())
            .is_some_and(|p| p == exe)
    }

    /// The signed pkg when the release has one; the unsigned CI build
    /// only with an explicit yes.
    fn pick(release: &Value, tag: &str, force: bool) -> Result<(Asset, bool)> {
        if let Some(a) = find_asset(release, &format!("container-mount-{tag}.pkg")) {
            return Ok((a, true));
        }
        let unsigned = format!("container-mount-{tag}-unsigned.pkg");
        match find_asset(release, &unsigned) {
            Some(a) => {
                consent(
                    &format!("release {tag} has no signed package — only the unsigned CI build"),
                    "Install the unsigned package? [y/N] ",
                    force,
                )?;
                Ok((a, false))
            }
            None => bail!("release {tag} has no container-mount .pkg asset"),
        }
    }

    /// `pkgutil --check-signature`: must be signed; returns the
    /// Developer ID Installer team when the output names one.
    fn check_signature(pkg: &Path) -> Result<Option<String>> {
        let out = Command::new("/usr/sbin/pkgutil")
            .arg("--check-signature")
            .arg(pkg)
            .output()
            .context("failed to run `pkgutil --check-signature`")?;
        let text = String::from_utf8_lossy(&out.stdout);
        if !out.status.success() || !text.contains("Status: signed") {
            bail!("{} failed its signature check:\n{text}", pkg.display());
        }
        Ok(installer_team(&text))
    }

    fn installer_team(text: &str) -> Option<String> {
        let line = text
            .lines()
            .find(|l| l.contains("Developer ID Installer:"))?;
        eprintln!("  signed by {}", line.trim());
        line.rsplit('(')
            .next()
            .and_then(|s| s.trim().strip_suffix(')'))
            .map(str::to_string)
    }

    pub fn update(
        release: &Value,
        tag: &str,
        exe: &Path,
        force: bool,
        dry_run: bool,
    ) -> Result<()> {
        let (asset, signed) = pick(release, tag, force)?;
        let pkg = download(&asset, &super::download_dir()?)?;
        let team = if signed { check_signature(&pkg)? } else { None };
        if let (Some(have), Some(got)) = (super::team_id(exe), team.as_deref())
            && have != got
        {
            consent(
                &format!("the {tag} package is signed by team {got}, the installed copy by {have}"),
                "Install it anyway? [y/N] ",
                force,
            )?;
        }
        if dry_run {
            println!(
                "dry run — would install {tag} from {} (pkg install)",
                pkg.display()
            );
            return Ok(());
        }
        eprintln!(
            "installing container-mount {tag} — {} opens in Installer.app; then run \
             `container-mount install-all` if you registered the plugin yourself",
            pkg.display()
        );
        let err = Command::new("/usr/bin/open")
            .arg("-a")
            .arg(INSTALLER_APP)
            .arg(&pkg)
            .exec();
        Err(err).with_context(|| {
            format!(
                "could not launch Installer.app; install the verified package manually:\n  \
                 sudo installer -pkg {} -target /",
                pkg.display()
            )
        })
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn release(names: &[&str]) -> Value {
            serde_json::json!({
                "tag_name": "v0.1.0",
                "assets": names.iter().map(|n| serde_json::json!({
                    "name": n,
                    "browser_download_url": format!("https://example/{n}"),
                    "digest": "sha256:aa",
                })).collect::<Vec<_>>(),
            })
        }

        #[test]
        fn test_pick_prefers_signed_pkg() {
            let r = release(&[
                "container-mount-0.1.0-unsigned.pkg",
                "container-mount-0.1.0.pkg",
                "container-mount-aarch64-apple-darwin-v0.1.0.tar.gz",
            ]);
            let (a, signed) = pick(&r, "0.1.0", false).unwrap();
            assert!(signed && a.name == "container-mount-0.1.0.pkg");
        }

        #[test]
        fn test_unsigned_pkg_needs_force_off_tty() {
            let r = release(&["container-mount-0.1.0-unsigned.pkg"]);
            assert!(!pick(&r, "0.1.0", true).unwrap().1);
            assert!(pick(&r, "0.1.0", false).is_err());
            assert!(pick(&release(&["x.tar.gz"]), "0.1.0", true).is_err());
        }

        #[test]
        fn test_parse_receipt_root() {
            let info = "package-id: io.github.daphnediane.container-mount\nlocation: usr/local\n";
            assert_eq!(parse_root(info), Some(PathBuf::from("/usr/local")));
            assert_eq!(
                parse_root("location: /\n"),
                Some(PathBuf::from("/usr/local"))
            );
            assert_eq!(parse_root("version: 1\n"), None);
        }

        #[test]
        fn test_installer_team_from_pkgutil_output() {
            let text = "Status: signed by a developer certificate issued by Apple for distribution\n\
                 Certificate Chain:\n 1. Developer ID Installer: Daphne Pfister (YN9EG6U9PY)\n";
            assert_eq!(installer_team(text).as_deref(), Some("YN9EG6U9PY"));
        }
    }
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
                    .join("container-mount"),
            );
        }
    }
    // SAFETY: getuid cannot fail.
    Ok(std::env::temp_dir().join(format!("container-mount-{}", unsafe { libc::getuid() })))
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
    if let Some(a) = find_asset(release, &format!("container-mount-{triple}-v{tag}.tar.gz")) {
        return Ok(a);
    }
    let unsigned = format!("container-mount-{triple}-v{tag}-unsigned.tar.gz");
    match find_asset(release, &unsigned) {
        Some(a) => {
            consent(
                &format!("release {tag} has no signed tarball — only the unsigned CI build"),
                "Install the unsigned build? [y/N] ",
                force,
            )?;
            Ok(a)
        }
        None => bail!("release {tag} has no container-mount tarball for {triple}"),
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

/// Create the `<triple>/` directory a server installs into.
fn install_dir_for(server: &Path) -> Result<()> {
    let dir = server.parent().context("server path has no parent")?;
    std::fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))
}

/// Swap the running binary for `new` and install any server binaries
/// into the exe's own server dir.
fn install(exe: &Path, new: &Path, server_bins: &[(servers::Platform, PathBuf)]) -> Result<()> {
    install_file(new, exe)?;
    if let Some(dir) = servers::server_dir_for(exe) {
        for (platform, s) in server_bins {
            let to = servers::installed_path(&dir, *platform);
            install_dir_for(&to)?;
            install_file(s, &to)?;
            eprintln!("installed {}", to.display());
        }
    }
    Ok(())
}
