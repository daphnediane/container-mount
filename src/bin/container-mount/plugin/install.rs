/*
 * Copyright (c) 2026 Daphne Pfister
 * SPDX-License-Identifier: BSD-2-Clause
 * See LICENSE file for full license text
 */

//! Registering `container mount`: `install-plugin`/`uninstall-plugin`
//! and the `install-all`/`uninstall-all` wrappers around them.
//!
//! `container` scans `<prefix>/libexec/container-plugins/<name>/` for a
//! `config.toml` plus `bin/<name>`, and `container <name> …` execs that
//! binary. Our directory is self-contained:
//!
//! ```text
//! container-plugins/mount/config.toml
//! container-plugins/mount/bin/mount                              this binary
//! container-plugins/mount/libexec/container-mount/<triple>/container-mount
//! ```
//!
//! so the plugin finds its servers through the ordinary prefix lookup,
//! and everything it runs as root in a guest sits in the same
//! root-owned tree. Files are **copied**, never linked: the build
//! output is user-writable, and a link would let that user replace
//! what every other user's `container mount` runs. Modeled on
//! container-distro's `install-plugin`.

use std::ffi::{CString, OsStr};
use std::fs;
use std::io::ErrorKind;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use container_mount::servers::{self, Platform};
use sha2::{Digest, Sha256};

use super::engine;

/// The plugin (and subcommand) name — upstream takes it from the
/// directory and binary names.
pub const PLUGIN_NAME: &str = "mount";
/// The `config.toml` abstract; uninstall recognizes our install by it.
const ABSTRACT: &str = "Browse and copy files in running containers and machines";
const MARKER: &str = "(container-mount)";

/// The plugin's `config.toml` (no `servicesConfig`: a CLI plugin).
#[must_use]
pub fn config_toml() -> String {
    format!(
        "abstract = \"{ABSTRACT} {MARKER}\"\nauthor = \"container-mount\"\nversion = \"{}\"\n",
        env!("CARGO_PKG_VERSION")
    )
}

/// What one install writes.
#[derive(Debug, Clone, PartialEq)]
struct Plan {
    /// `<root>/mount`.
    dir: PathBuf,
    /// The binary copied to `bin/mount`.
    exe: PathBuf,
    /// Servers copied under `libexec/container-mount/`.
    servers: Vec<(Platform, PathBuf)>,
}

impl Plan {
    fn bin(&self) -> PathBuf {
        self.dir.join("bin").join(PLUGIN_NAME)
    }

    fn servers_dir(&self) -> PathBuf {
        self.dir.join("libexec").join("container-mount")
    }

    /// Whether the installed copy already matches — identical binary
    /// (with its exec bit), config, and servers — so a re-run can
    /// answer "up to date" without elevating. Unreadable counts as
    /// not current.
    fn current(&self) -> bool {
        fs::read_to_string(self.dir.join("config.toml")).is_ok_and(|c| c == config_toml())
            && same_bytes(&self.bin(), &self.exe)
            && fs::metadata(self.bin()).is_ok_and(|m| m.permissions().mode() & 0o111 != 0)
            && self
                .servers
                .iter()
                .all(|(p, s)| same_bytes(&servers::installed_path(&self.servers_dir(), *p), s))
    }
}

/// Whether `a` and `b` are regular files with identical bytes.
fn same_bytes(a: &Path, b: &Path) -> bool {
    let (Ok(ma), Ok(mb)) = (fs::metadata(a), fs::metadata(b)) else {
        return false;
    };
    if !ma.is_file() || !mb.is_file() || ma.len() != mb.len() {
        return false;
    }
    if ma.dev() == mb.dev() && ma.ino() == mb.ino() {
        return true;
    }
    let digest = |p: &Path| fs::read(p).map(Sha256::digest);
    matches!((digest(a), digest(b)), (Ok(x), Ok(y)) if x == y)
}

/// `<prefix>/libexec/container-plugins` for `container` at
/// `<prefix>/bin/container`.
fn plugin_root_for(container_bin: &Path) -> Option<PathBuf> {
    Some(
        container_bin
            .parent()?
            .parent()?
            .join("libexec/container-plugins"),
    )
}

/// The plugin root: `--plugin-dir`, else derived from the resolved
/// `container` binary (resolved as the invoking user, before any sudo).
fn plugin_root(explicit: Option<PathBuf>, dev: bool) -> Result<PathBuf> {
    if let Some(r) = explicit {
        return Ok(r);
    }
    let bin = engine::resolve(dev).context(
        "`container` not found — install it first (https://github.com/apple/container/releases), \
         or pass --plugin-dir",
    )?;
    let bin =
        fs::canonicalize(&bin).with_context(|| format!("cannot resolve {}", bin.display()))?;
    plugin_root_for(&bin).context("cannot derive the plugin directory from the `container` path")
}

/// This binary's servers for every platform it can find, each checked
/// to be a stamped build of our own version — client and servers are
/// lock-step.
fn our_servers(dev: bool) -> Result<Vec<(Platform, PathBuf)>> {
    let mut out = Vec::new();
    for platform in Platform::ALL {
        let Ok(path) = servers::server_path(platform, dev) else {
            continue;
        };
        out.push((platform, checked_server(&path)?));
    }
    if out.is_empty() {
        bail!(
            "no container-mount servers found to bundle with the plugin — \
             run `container-mount fetch-servers` (or build them) first"
        );
    }
    Ok(out)
}

/// `path`, canonicalized, if it carries a stamp of our own version.
fn checked_server(path: &Path) -> Result<PathBuf> {
    let path =
        fs::canonicalize(path).with_context(|| format!("cannot resolve {}", path.display()))?;
    let stamp = servers::stamped(&path)?;
    let version = &stamp.version;
    if !stamp.compatible() {
        bail!(
            "{} speaks a different protocol (protocol {:?}, wire {}) — client and servers must match",
            path.display(),
            stamp.protocol,
            stamp.wire.as_deref().unwrap_or("none")
        );
    }
    if version != env!("CARGO_PKG_VERSION") {
        bail!(
            "{} is container-mount {version}, not {} — client and servers must match",
            path.display(),
            env!("CARGO_PKG_VERSION")
        );
    }
    Ok(path)
}

/// Options shared by the install/uninstall commands.
#[derive(Debug, Default)]
struct Opts {
    plugin_dir: Option<PathBuf>,
    from: Option<PathBuf>,
    /// `TRIPLE:PATH` — set by the sudo re-exec so the elevated child
    /// copies exactly what the user's process resolved.
    servers: Vec<(Platform, PathBuf)>,
    no_plugin: bool,
    no_fetch: bool,
}

fn parse(args: &[String], allowed: &[&str]) -> Result<Opts> {
    let mut o = Opts::default();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if !allowed.contains(&a.as_str()) {
            bail!("unknown argument `{a}` (accepted: {})", allowed.join(" "));
        }
        let mut value = || it.next().with_context(|| format!("{a} needs a value"));
        match a.as_str() {
            "--plugin-dir" => o.plugin_dir = Some(PathBuf::from(value()?)),
            "--from" => o.from = Some(PathBuf::from(value()?)),
            "--server" => {
                let v = value()?;
                let (t, p) = v
                    .split_once(':')
                    .with_context(|| format!("--server wants TRIPLE:PATH, got `{v}`"))?;
                let platform =
                    Platform::parse(t).with_context(|| format!("unknown platform `{t}`"))?;
                o.servers.push((platform, PathBuf::from(p)));
            }
            "--no-plugin" => o.no_plugin = true,
            "--no-fetch" => o.no_fetch = true,
            _ => unreachable!("listed in `allowed`"),
        }
    }
    Ok(o)
}

/// Our plugin directory under the default plugin root, when one is
/// installed there and doesn't match this binary — for `update`'s
/// "refresh the plugin" hint. Reads only; never elevates.
#[must_use]
pub fn stale_plugin(dev: bool) -> Option<PathBuf> {
    let dir = plugin_root(None, dev).ok()?.join(PLUGIN_NAME);
    let config = fs::read_to_string(dir.join("config.toml")).ok()?;
    if !config.contains(MARKER) {
        return None;
    }
    let exe = fs::canonicalize(std::env::current_exe().ok()?).ok()?;
    (!same_bytes(&dir.join("bin").join(PLUGIN_NAME), &exe)).then_some(dir)
}

/// `install-plugin [--plugin-dir DIR]`.
pub fn install_plugin(args: &[String], dev: bool) -> Result<()> {
    let o = parse(args, &["--plugin-dir", "--from", "--server"])?;
    install(o, dev)
}

fn install(o: Opts, dev: bool) -> Result<()> {
    let root = plugin_root(o.plugin_dir, dev)?;
    let exe = match o.from {
        Some(f) => {
            fs::canonicalize(&f).with_context(|| format!("cannot resolve {}", f.display()))?
        }
        None => fs::canonicalize(std::env::current_exe()?)?,
    };
    servers::stamped(&exe).context("--from must be a container-mount build")?;
    let servers = if o.servers.is_empty() {
        our_servers(dev)?
    } else {
        o.servers
            .iter()
            .map(|(p, s)| Ok((*p, checked_server(s)?)))
            .collect::<Result<_>>()?
    };
    let plan = Plan {
        dir: root.join(PLUGIN_NAME),
        exe,
        servers,
    };
    if plan.current() {
        println!("`container {PLUGIN_NAME}` is already installed and up to date");
        return Ok(());
    }
    let mut extra = vec![
        "--from".to_string(),
        plan.exe.to_string_lossy().into_owned(),
    ];
    for (p, s) in &plan.servers {
        extra.push("--server".into());
        extra.push(format!("{}:{}", p.triple(), s.display()));
    }
    with_privilege("install-plugin", &root, &extra, || install_into(&plan))
}

fn install_into(plan: &Plan) -> Result<()> {
    let bin = plan.bin();
    copy_executable(&plan.exe, &bin)?;
    for (platform, src) in &plan.servers {
        copy_executable(
            src,
            &servers::installed_path(&plan.servers_dir(), *platform),
        )?;
    }
    // config.toml last: `container` only lists the plugin once the
    // binary it would exec is in place.
    let config = plan.dir.join("config.toml");
    fs::write(&config, config_toml()).map_err(|e| permission_hint(e, &config))?;
    println!("Installed `container {PLUGIN_NAME}` ({})", bin.display());
    println!("  copied from {}", plan.exe.display());
    for (platform, _) in &plan.servers {
        println!("  with the {} server", platform.label());
    }
    Ok(())
}

/// Copy `src` to `dest` as a fresh `0755` file, replacing whatever was
/// there (a symlink included) rather than writing through it.
fn copy_executable(src: &Path, dest: &Path) -> Result<()> {
    let dir = dest.parent().context("destination has no parent")?;
    fs::create_dir_all(dir).map_err(|e| permission_hint(e, dir))?;
    if dest.symlink_metadata().is_ok() {
        fs::remove_file(dest).map_err(|e| permission_hint(e, dest))?;
    }
    fs::copy(src, dest).map_err(|e| permission_hint(e, dest))?;
    fs::set_permissions(dest, fs::Permissions::from_mode(0o755))
        .map_err(|e| permission_hint(e, dest))?;
    Ok(())
}

/// `uninstall-plugin [--plugin-dir DIR]`.
pub fn uninstall_plugin(args: &[String], dev: bool) -> Result<()> {
    let o = parse(args, &["--plugin-dir"])?;
    uninstall(o, dev)
}

fn uninstall(o: Opts, dev: bool) -> Result<()> {
    let root = plugin_root(o.plugin_dir, dev)?;
    let dir = root.join(PLUGIN_NAME);
    // Nothing there? Answer without elevating.
    if let Err(e) = fs::metadata(dir.join("config.toml"))
        && e.kind() == ErrorKind::NotFound
    {
        println!("Not installed ({})", dir.display());
        return Ok(());
    }
    with_privilege("uninstall-plugin", &root, &[], || uninstall_from(&dir))
}

fn uninstall_from(dir: &Path) -> Result<()> {
    let config = dir.join("config.toml");
    match fs::read_to_string(&config) {
        Ok(c) if c.contains(MARKER) => {}
        Ok(_) => bail!("{} was not installed by container-mount", dir.display()),
        Err(e) if e.kind() == ErrorKind::NotFound => {
            println!("Not installed ({})", dir.display());
            return Ok(());
        }
        Err(e) => return Err(e.into()),
    }
    // Only what install writes — files by name, then directories iff
    // empty — so a foreign file (or a directory swapped in) survives.
    let servers_dir = dir.join("libexec").join("container-mount");
    for platform in Platform::ALL {
        let s = servers::installed_path(&servers_dir, platform);
        remove_file(&s);
        if let Some(d) = s.parent() {
            remove_dir_if_empty(d);
        }
    }
    remove_dir_if_empty(&servers_dir);
    remove_dir_if_empty(&dir.join("libexec"));
    let bin_dir = dir.join("bin");
    remove_file(&bin_dir.join(PLUGIN_NAME));
    remove_dir_if_empty(&bin_dir);
    remove_file(&config);
    if remove_dir_if_empty(dir) {
        println!("Removed {}", dir.display());
    }
    Ok(())
}

fn remove_file(path: &Path) {
    match fs::remove_file(path) {
        Ok(()) => println!("Removed {}", path.display()),
        Err(e) if e.kind() == ErrorKind::NotFound => {}
        Err(e) => eprintln!("container-mount: cannot remove {}: {e}", path.display()),
    }
}

/// Remove `dir` only if empty; false (with a warning) when files remain.
fn remove_dir_if_empty(dir: &Path) -> bool {
    match fs::remove_dir(dir) {
        Ok(()) => true,
        Err(e) if e.kind() == ErrorKind::NotFound => true,
        Err(e) => {
            eprintln!("container-mount: {} left behind: {e}", dir.display());
            false
        }
    }
}

fn permission_hint(e: std::io::Error, what: &Path) -> anyhow::Error {
    let denied = e.kind() == ErrorKind::PermissionDenied;
    let err = anyhow::Error::from(e);
    if denied {
        err.context(format!(
            "cannot write {} (re-run with sudo, or pass --plugin-dir)",
            what.display()
        ))
    } else {
        err.context(format!("failed to write {}", what.display()))
    }
}

/// Whether this process runs with euid 0.
fn is_root() -> bool {
    // SAFETY: geteuid is always safe to call.
    unsafe { libc::geteuid() == 0 }
}

/// Whether `dir` — or its nearest existing ancestor — is writable by
/// this user.
fn user_writable(dir: &Path) -> bool {
    let mut probe = Some(dir);
    while let Some(d) = probe {
        if d.exists() {
            let Ok(c) = CString::new(d.as_os_str().as_bytes()) else {
                return false;
            };
            // SAFETY: `c` is a NUL-terminated path.
            return unsafe { libc::access(c.as_ptr(), libc::W_OK | libc::X_OK) } == 0;
        }
        probe = d.parent();
    }
    false
}

/// Run the file ops directly when the plugin dir is writable, else
/// re-exec `sudo <self> SUBCOMMAND --plugin-dir ROOT EXTRA…` — only the
/// file ops run as root, with every input (root, binary, servers)
/// passed explicitly so sudo's env_reset can't change what's
/// installed. The same trust as running `sudo` by hand.
fn with_privilege(
    subcommand: &str,
    root: &Path,
    extra: &[String],
    ops: impl FnOnce() -> Result<()>,
) -> Result<()> {
    if is_root() || user_writable(&root.join(PLUGIN_NAME)) {
        return ops();
    }
    let exe = std::env::current_exe().context("cannot locate the running binary")?;
    eprintln!("{} requires administrator rights:", root.display());
    let status = Command::new("/usr/bin/sudo")
        .arg(exe)
        .arg(subcommand)
        .arg("--plugin-dir")
        .arg(root)
        .args(extra)
        .status()
        .context("failed to run sudo")?;
    if !status.success() {
        bail!("sudo did not complete — re-run `sudo container-mount {subcommand}` yourself");
    }
    Ok(())
}

/// `install-all [--plugin-dir DIR] [--no-plugin] [--no-fetch]` —
/// everything around an installed `container-mount`: this release's
/// servers (fetched when missing) and the `container mount` plugin.
/// Man pages join with the clap feature (#5).
pub fn install_all(args: &[String], dev: bool) -> Result<()> {
    refuse_plugin_copy()?;
    let o = parse(args, &["--plugin-dir", "--no-plugin", "--no-fetch"])?;
    let mut failed = Vec::new();
    if !o.no_fetch
        && Platform::ALL
            .iter()
            .any(|&p| servers::server_path(p, dev).is_err())
        && let Err(e) = crate::update::fetch_servers(&[], dev)
    {
        eprintln!("container-mount: servers: {e:#}");
        failed.push("servers");
    }
    if !o.no_plugin {
        let plugin = Opts {
            plugin_dir: o.plugin_dir,
            ..Opts::default()
        };
        if let Err(e) = install(plugin, dev) {
            eprintln!("container-mount: plugin: {e:#}");
            failed.push("plugin");
        }
    }
    if failed.is_empty() {
        Ok(())
    } else {
        bail!("incomplete — failed: {}", failed.join(", "))
    }
}

/// `uninstall-all [--plugin-dir DIR]` — undoes `install-all`'s
/// registrations. The binary and its prefix servers belong to cargo or
/// the pkg receipt, which remove them.
pub fn uninstall_all(args: &[String], dev: bool) -> Result<()> {
    refuse_plugin_copy()?;
    let o = parse(args, &["--plugin-dir"])?;
    uninstall(o, dev)
}

/// install-all from the plugin copy would register the plugin from
/// itself and fetch into the plugin's own tree — run it from the
/// installed `container-mount` instead.
fn refuse_plugin_copy() -> Result<()> {
    let exe = std::env::current_exe()?;
    if exe.file_name() == Some(OsStr::new(PLUGIN_NAME)) {
        bail!("this is the `container mount` plugin copy — run install-all from `container-mount`");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stamped stand-in for a binary or server, under a tempdir.
    fn fake_build(dir: &Path, name: &str) -> PathBuf {
        let p = dir.join(name);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(
            &p,
            format!(
                "\x7fELF container-mount-version:{}:protocol:{}:wire:{}\0{name}",
                env!("CARGO_PKG_VERSION"),
                container_mount::proto::VERSION,
                container_mount::proto::WIRE_HEX
            ),
        )
        .unwrap();
        p
    }

    fn plan(root: &Path, src: &Path) -> Plan {
        Plan {
            dir: root.join(PLUGIN_NAME),
            exe: fake_build(src, "container-mount"),
            servers: vec![(
                Platform::LinuxArm64,
                fake_build(src, "aarch64-unknown-linux-musl/container-mount"),
            )],
        }
    }

    #[test]
    fn test_config_is_cli_plugin_with_marker() {
        let c = config_toml();
        assert!(c.contains("abstract = ") && c.contains(MARKER));
        assert!(!c.contains("servicesConfig"));
    }

    #[test]
    fn test_plugin_root_from_container_bin() {
        assert_eq!(
            plugin_root_for(Path::new("/usr/local/bin/container")).unwrap(),
            Path::new("/usr/local/libexec/container-plugins")
        );
    }

    #[test]
    fn test_install_is_self_contained_and_idempotent() {
        let (root, src) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let p = plan(root.path(), src.path());
        assert!(!p.current());
        install_into(&p).unwrap();
        assert!(p.current());
        let server = root
            .path()
            .join("mount/libexec/container-mount/aarch64-unknown-linux-musl/container-mount");
        assert_eq!(
            fs::metadata(&server).unwrap().permissions().mode() & 0o777,
            0o755
        );
        // The plugin copy finds its bundled servers by the prefix rule.
        assert_eq!(
            servers::server_dir_for(&p.bin()),
            Some(root.path().join("mount/libexec/container-mount"))
        );
        // A changed source makes it stale again.
        fs::write(&p.servers[0].1, b"other").unwrap();
        assert!(!p.current());
    }

    #[test]
    fn test_install_replaces_a_symlink_instead_of_writing_through() {
        let (root, src) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let p = plan(root.path(), src.path());
        let decoy = src.path().join("decoy");
        fs::write(&decoy, b"decoy").unwrap();
        fs::create_dir_all(p.bin().parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&decoy, p.bin()).unwrap();
        install_into(&p).unwrap();
        assert_eq!(fs::read(&decoy).unwrap(), b"decoy");
        assert!(p.bin().symlink_metadata().unwrap().is_file());
    }

    #[test]
    fn test_uninstall_removes_ours_and_keeps_foreign_files() {
        let (root, src) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let p = plan(root.path(), src.path());
        install_into(&p).unwrap();
        let keep = p.dir.join("bin/keep.txt");
        fs::write(&keep, b"not ours").unwrap();
        uninstall_from(&p.dir).unwrap();
        assert!(!p.dir.join("config.toml").exists() && !p.bin().exists());
        assert!(!p.dir.join("libexec").exists());
        assert!(keep.exists());
        fs::remove_file(&keep).unwrap();
        install_into(&p).unwrap();
        uninstall_from(&p.dir).unwrap();
        assert!(!p.dir.exists());
    }

    #[test]
    fn test_uninstall_refuses_a_foreign_plugin() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join(PLUGIN_NAME);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("config.toml"), "abstract = \"someone else\"\n").unwrap();
        assert!(uninstall_from(&dir).is_err());
        assert!(dir.join("config.toml").exists());
    }

    #[test]
    fn test_parse_server_and_rejects_unlisted_flags() {
        let o = parse(
            &[
                "--server".into(),
                "aarch64-unknown-linux-musl:/a:b/c".into(),
            ],
            &["--server"],
        )
        .unwrap();
        assert_eq!(
            o.servers,
            vec![(Platform::LinuxArm64, PathBuf::from("/a:b/c"))]
        );
        assert!(parse(&["--from".into(), "/x".into()], &["--plugin-dir"]).is_err());
        assert!(parse(&["--server".into(), "nope".into()], &["--server"]).is_err());
    }

    #[test]
    fn test_checked_server_requires_our_version() {
        let src = tempfile::tempdir().unwrap();
        let ok = fake_build(src.path(), "s");
        assert!(checked_server(&ok).is_ok());
        let old = src.path().join("old");
        fs::write(&old, b"container-mount-version:0.0.1:protocol:1\0").unwrap();
        assert!(checked_server(&old).is_err());
        // Same version, other protocol sources (a dev build): refused.
        let drift = src.path().join("drift");
        fs::write(
            &drift,
            format!(
                "container-mount-version:{}:protocol:0:wire:ffffffffffffffff\0",
                env!("CARGO_PKG_VERSION")
            ),
        )
        .unwrap();
        assert!(checked_server(&drift).is_err());
    }
}
