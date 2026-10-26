/*
 * Copyright (c) 2026 Daphne Pfister
 * SPDX-License-Identifier: BSD-2-Clause
 * See LICENSE file for full license text
 */

//! `container mount` — the `container` CLI plugin
//! (docs/design/mount-backends.md).
//!
//! Installed as `<prefix>/libexec/container-plugins/mount/bin/mount`
//! (the same binary, dispatched on its file name). It reaches any
//! running container with no create-time flag: copy a server into the
//! guest, start it as root on a random vsock port below 1024 with a
//! fresh token, and dial that port through the apiserver. Each command
//! gets its own `--once` server, so the token lives only in this
//! process and that server's environment.

mod engine;
mod upstream;

use std::os::unix::net::UnixStream;
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use container_mount::client::Client;
use container_mount::connect::Stream;
use container_mount::dial::{self, APISERVER_REQUIREMENT};
use container_mount::proto;
use container_mount::servers::{self, Platform};

use crate::fscli;

/// The plugin CLI contract's version — what container-distro checks,
/// separate from the wire protocol version.
pub const INTERFACE: u32 = 1;

/// Guest vsock ports the server picks from: below 1024 so only
/// `CAP_NET_BIND_SERVICE` can bind them, clear of `vminitd`'s 1024.
const PORTS: std::ops::Range<u32> = 600..1024;

/// Guest directory holding per-command server copies — under root's
/// own `0700` home where the image has one.
const GUEST_DIR: &str = "/root/.container-mount";

/// How long to wait for the copied server to start listening.
const START_TIMEOUT: Duration = Duration::from_secs(10);

/// How long a started server waits for us before giving up.
const SERVER_IDLE_SECS: &str = "30";

const USAGE: &str = "\
usage: container mount [--dev] COMMAND ...

  fs TARGET ls|stat|get|put|cp|check ...
             file operations on a running container or machine
             (TARGET is a container id or machine name; no mount
             point needed; see `container-mount --help` per command)
  version [--format json]
             the plugin and interface version

  --dev      dev seams: CONTAINER_CLI, in-tree server builds";

/// Entry point for plugin mode. `args` excludes the program name.
pub fn run(args: &[String]) -> Result<()> {
    // `container` may pass the subcommand name as argv[1].
    let args = match args.first() {
        Some(a) if a == "mount" => &args[1..],
        _ => args,
    };
    let (dev, args) = match args.first() {
        Some(a) if a == "--dev" => (true, &args[1..]),
        _ => (false, args),
    };
    let Some(cmd) = args.first() else {
        bail!("{USAGE}");
    };
    match cmd.as_str() {
        "fs" => fs(dev, &args[1..]),
        "version" => version(&args[1..]),
        "--help" | "-h" | "help" => {
            println!("{USAGE}");
            Ok(())
        }
        _ => bail!("unknown command `{cmd}`\n{USAGE}"),
    }
}

fn version(args: &[String]) -> Result<()> {
    match args {
        [] => println!(
            "container mount {} (interface {INTERFACE}, protocol {})",
            env!("CARGO_PKG_VERSION"),
            proto::VERSION
        ),
        [f, j] if f == "--format" && j == "json" => println!(
            "{}",
            serde_json::json!({
                "interface": INTERFACE,
                "version": env!("CARGO_PKG_VERSION"),
                "protocol": proto::VERSION,
            })
        ),
        _ => bail!("usage: container mount version [--format json]"),
    }
    Ok(())
}

/// `fs TARGET OP ARGS…` — start a one-shot server in TARGET and run one
/// file command against it.
fn fs(dev: bool, args: &[String]) -> Result<()> {
    let [target, op, rest @ ..] = args else {
        bail!("usage: container mount fs TARGET ls|stat|get|put|cp|check ...");
    };
    if !matches!(op.as_str(), "ls" | "stat" | "get" | "put" | "cp" | "check") {
        bail!("unknown fs command `{op}`");
    }
    let (client, port) = connect(dev, target)?;
    let via = format!("vsock port {port} in {target} (apiserver dial, signature pinned)");
    fscli::run(op, rest, &client, &via, false)
}

/// Deliver, start, and dial a server in running container `target`.
fn connect(dev: bool, target: &str) -> Result<(Client, u32)> {
    upstream::warn_unverified_version(dev);
    let info = engine::inspect(dev, target)?;
    if info.state != "running" {
        bail!(
            "{target} is {}; `container mount fs` needs a running container",
            info.state
        );
    }
    let token = proto::random32().context("couldn't generate a session token")?;
    let random = proto::random32().context("couldn't pick a port")?;
    let port = PORTS.start + u32::from_le_bytes(random[..4].try_into()?) % PORTS.len() as u32;
    let nonce = proto::hex(&random[4..12]);
    let guest_bin = deliver(dev, &info.id, &nonce)?;
    // A per-command variable name: the target's own environment may
    // already define CONTAINER_MOUNT_TOKEN, and its entry would win.
    let token_env = format!("{}_{nonce}", proto::TOKEN_ENV);
    engine::exec_detached(
        dev,
        &info.id,
        &[(&token_env, &proto::hex(&token))],
        &[
            guest_bin,
            "serve".into(),
            "--token-env".into(),
            token_env.clone(),
            "--listen".into(),
            format!("vsock:{port}"),
            "--once".into(),
            "--remove-binary".into(),
            "--idle-secs".into(),
            SERVER_IDLE_SECS.into(),
        ],
    )?;
    Ok((dial_and_handshake(&info.id, port, &token)?, port))
}

/// Copy the server for the VM's architecture into a fresh guest
/// directory; returns the guest path of the binary. A copy lands
/// owned by the host uid, so it goes under root's home; the server
/// deletes it once running.
fn deliver(dev: bool, id: &str, nonce: &str) -> Result<String> {
    // Apple silicon VMs run an arm64 kernel even for amd64 images
    // (Rosetta), so the server matches the host.
    let platform =
        Platform::parse(std::env::consts::ARCH).context("unsupported host architecture")?;
    let server = servers::server_path(platform, dev)?;
    let stage = engine::private_temp_dir()?;
    let result = (|| {
        let bin = stage.join("container-mount");
        std::fs::copy(&server, &bin)
            .with_context(|| format!("couldn't stage {}", server.display()))?;
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755))?;
        let dest = format!("{GUEST_DIR}/{nonce}");
        engine::copy_in(dev, &stage, id, &dest)?;
        Ok(format!("{dest}/container-mount"))
    })();
    let _ = std::fs::remove_dir_all(&stage);
    result
}

/// Dial `port` until the server listens, then handshake. A handshake
/// failure is final: whatever answered isn't our server.
fn dial_and_handshake(id: &str, port: u32, token: &[u8; 32]) -> Result<Client> {
    let deadline = Instant::now() + START_TIMEOUT;
    loop {
        match dial::dial(id, port, APISERVER_REQUIREMENT) {
            Ok(fd) => {
                return Client::handshake(Stream::Unix(UnixStream::from(fd)), token).with_context(
                    || format!("vsock port {port} in {id} answered, but not as our server"),
                );
            }
            Err(e) if Instant::now() >= deadline => {
                return Err(e).context(format!("server in {id} never listened on port {port}"));
            }
            Err(_) => thread::sleep(Duration::from_millis(100)),
        }
    }
}

/// Whether `exe` was invoked as the plugin (`…/bin/mount`).
pub fn invoked_as_plugin(exe: &Path) -> bool {
    exe.file_name().is_some_and(|n| n == "mount")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_invoked_as_plugin_by_file_name() {
        assert!(invoked_as_plugin(Path::new(
            "/usr/local/libexec/container-plugins/mount/bin/mount"
        )));
        assert!(!invoked_as_plugin(Path::new(
            "/usr/local/bin/container-mount"
        )));
    }

    #[test]
    fn test_ports_are_privileged() {
        assert!(PORTS.end <= 1024 && !PORTS.is_empty());
    }
}
