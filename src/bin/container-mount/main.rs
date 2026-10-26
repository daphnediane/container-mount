/*
 * Copyright (c) 2026 Daphne Pfister
 * SPDX-License-Identifier: BSD-2-Clause
 * See LICENSE file for full license text
 */

//! `container-mount` — one binary, two ends.
//!
//! As the guest server (`container-mount serve`) it runs inside a Linux
//! container/VM on a socket the VM relays to the host. As the host
//! client (`ls`/`stat`/`get`/`put`/`check`) it connects through that
//! relay — or any other Unix socket, inherited fd, or TCP port —
//! validates everything the server sends, and confines symlink targets
//! to the presented root. `servers`/`server-path`/`fetch-servers`
//! manage guest server binaries; `update` self-updates installed
//! copies.

mod fscli;
#[cfg(target_os = "macos")]
mod plugin;
mod serve;
mod table;
mod update;

// Portable (and unit-tested on the host); only the Linux server uses it.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
mod mounts;
#[cfg(target_os = "linux")]
mod server;

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use container_mount::connect::{self, Target};
use container_mount::proto;

/// Scannable version marker — `container-mount servers`/`server-path` read it
/// out of a binary without executing it (`servers::STAMP_MARK`).
/// `#[used]` keeps it in the binary through release+strip builds.
#[used]
static VERSION_STAMP: &[u8] = concat!(
    "container-mount-version:",
    env!("CARGO_PKG_VERSION"),
    ":protocol:2\0"
)
.as_bytes();
const _: () = assert!(
    proto::VERSION == 2,
    "bump the stamp's protocol digits with proto::VERSION"
);

const USAGE: &str = "\
usage: container-mount [--dev] [--timeout SECS]
                [--socket PATH | --fd N | --tcp HOST:PORT]
                [--peer-requirement REQ] COMMAND ...

  serve      run the guest file server (Linux only)
  ls         list a guest directory (-l for long format)
  stat       show one guest entry's attributes (-L follows the link)
  get        copy a guest file out (DEST or `-`/omitted for stdout)
  put        copy a host file in (SRC `-` reads stdin)
  cp         copy a guest file inside the guest (reflink when possible)
  check      verify transport, peer, and the session handshake
  socket-path KEY   print a managed private host socket path for KEY
  servers    list known guest server binaries (-v shows search paths)
  server-path --arch ARCH   path of the server binary for ARCH
  fetch-servers [--version V] [--prefix DIR]   download server binaries
  update     self-update this install to the latest release
             [--version V] [--dry-run] [--force]
  plugin ... the `container mount` plugin interface, uninstalled
             (macOS; `plugin --help`)

  --socket PATH   unix socket to connect to (or CONTAINER_MOUNT_SOCKET)
  --fd N          inherited, already-connected unix socket fd
  --tcp HOST:PORT connect over TCP (engines that only publish TCP)
  --peer-requirement REQ   require the unix peer to satisfy this
                           code-signing requirement (macOS)
  --dev        enable dev seams (CONTAINER_MOUNT_RELEASES_URL, in-tree builds)
  --version  print the version";

/// How long the client waits for a server to come up.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Global options parsed ahead of the subcommand.
struct Opts {
    dev: bool,
    target: Option<Target>,
    peer_requirement: Option<String>,
    timeout: Duration,
}

fn parse_global(args: &[String]) -> Result<(Opts, &[String])> {
    let mut o = Opts {
        dev: false,
        target: None,
        peer_requirement: None,
        timeout: CONNECT_TIMEOUT,
    };
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        if !arg.starts_with('-') {
            break;
        }
        let mut value = |name: &str| -> Result<String> {
            i += 1;
            args.get(i)
                .cloned()
                .with_context(|| format!("{name} needs a value"))
        };
        match arg.as_str() {
            "--dev" => o.dev = true,
            "--socket" | "-s" => o.target = Some(Target::Socket(PathBuf::from(value(arg)?))),
            "--fd" => {
                let v = value(arg)?;
                o.target = Some(Target::Fd(
                    v.parse().with_context(|| format!("bad --fd value `{v}`"))?,
                ));
            }
            "--tcp" => o.target = Some(Target::Tcp(value(arg)?)),
            "--peer-requirement" => o.peer_requirement = Some(value(arg)?),
            "--timeout" => {
                let v = value(arg)?;
                o.timeout = Duration::from_secs(
                    v.parse()
                        .with_context(|| format!("bad --timeout value `{v}`"))?,
                );
            }
            "--version" => {
                println!("{}", version());
                std::process::exit(0);
            }
            "--help" | "-h" => {
                println!("{USAGE}");
                std::process::exit(0);
            }
            _ => bail!("unknown option `{arg}`\n{USAGE}"),
        }
        i += 1;
    }
    Ok((o, &args[i..]))
}

/// `container-mount X.Y.Z (protocol N)` — the same text `--version` prints and
/// the stamp carries.
fn version() -> String {
    format!(
        "container-mount {} (protocol {})",
        env!("CARGO_PKG_VERSION"),
        proto::VERSION
    )
}

/// The session token from `CONTAINER_MOUNT_TOKEN`.
fn token() -> Result<[u8; 32]> {
    let hex =
        std::env::var(proto::TOKEN_ENV).with_context(|| format!("missing {}", proto::TOKEN_ENV))?;
    let bytes = proto::unhex(&hex)
        .filter(|b| b.len() == 32)
        .with_context(|| format!("malformed {}", proto::TOKEN_ENV))?;
    Ok(bytes.try_into().unwrap())
}

/// The connect target: an explicit flag, else `CONTAINER_MOUNT_SOCKET`.
fn target(o: &Opts) -> Result<Target> {
    if let Some(t) = &o.target {
        return Ok(t.clone());
    }
    if let Some(p) = std::env::var_os(proto::SOCKET_ENV) {
        return Ok(Target::Socket(PathBuf::from(p)));
    }
    bail!("no server address — pass --socket PATH, --fd N, or --tcp HOST:PORT")
}

/// A short description of `t` for messages like `check`'s output.
fn describe(t: &Target) -> String {
    match t {
        Target::Socket(p) => format!("unix socket {}", p.display()),
        Target::Fd(n) => format!("fd {n}"),
        Target::Tcp(a) => format!("tcp {a}"),
    }
}

/// Connect + handshake per the global options.
fn connect_client(o: &Opts) -> Result<(container_mount::client::Client, String)> {
    let t = target(o)?;
    let tok = token()?;
    let c = connect::open(&t, &tok, o.peer_requirement.as_deref(), o.timeout)?;
    Ok((c, describe(&t)))
}

fn real_main() -> Result<()> {
    let mut argv = std::env::args_os();
    let exe = argv.next().map(PathBuf::from).unwrap_or_default();
    let args: Vec<String> = argv
        .map(|a| a.into_string())
        .collect::<std::result::Result<_, _>>()
        .map_err(|a| anyhow::anyhow!("argument is not UTF-8: {a:?}"))?;
    #[cfg(target_os = "macos")]
    if plugin::invoked_as_plugin(&exe) {
        return plugin::run(&args);
    }
    let _ = exe;
    let (o, rest) = parse_global(&args)?;
    let Some(cmd) = rest.first() else {
        bail!("{USAGE}");
    };
    match cmd.as_str() {
        "serve" => serve::run(&rest[1..]),
        "socket-path" => {
            let key = rest
                .get(1)
                .context("socket-path needs a KEY (e.g. the distro id)")?;
            println!("{}", connect::socket_path(key)?.display());
            Ok(())
        }
        "servers" => servers(&rest[1..], o.dev),
        "server-path" => server_path(&rest[1..], o.dev),
        "fetch-servers" => update::fetch_servers(&rest[1..], o.dev),
        "update" => update::run(&rest[1..], o.dev),
        // The plugin interface without installing it (`container mount …`).
        #[cfg(target_os = "macos")]
        "plugin" => {
            let dev = o.dev.then(|| "--dev".to_string());
            plugin::run(
                &dev.into_iter()
                    .chain(rest[1..].iter().cloned())
                    .collect::<Vec<_>>(),
            )
        }
        "ls" | "stat" | "get" | "put" | "cp" | "check" => {
            let (c, via) = connect_client(&o)?;
            fscli::run(cmd, &rest[1..], &c, &via, o.peer_requirement.is_some())
        }
        _ => bail!("unknown command `{cmd}`\n{USAGE}"),
    }
}

fn servers(args: &[String], dev: bool) -> Result<()> {
    let verbose = args.iter().any(|a| a == "-v" || a == "--verbose");
    let found = container_mount::servers::servers(dev);
    for arch in container_mount::servers::Arch::ALL {
        match found.iter().find(|s| s.arch == arch) {
            Some(s) => println!(
                "{:<7} {}  {}",
                arch.label(),
                match (s.version.as_deref(), s.protocol) {
                    (Some(v), Some(p)) => format!("{v} (protocol {p})"),
                    (Some(v), None) => v.to_string(),
                    _ => "(version unknown)".into(),
                },
                s.path.display()
            ),
            None => println!("{:<7} (not found)", arch.label()),
        }
    }
    if verbose {
        eprintln!("search paths:");
        for p in container_mount::servers::search_paths(dev) {
            eprintln!("  {}", p.display());
        }
    }
    Ok(())
}

fn server_path(args: &[String], dev: bool) -> Result<()> {
    let mut arch = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--arch" | "-a" => {
                let v = it.next().context("--arch needs a value")?;
                arch = container_mount::servers::Arch::parse(v);
                if arch.is_none() {
                    bail!("unknown architecture `{v}` (arm64 or amd64)");
                }
            }
            _ => bail!("unknown argument `{a}`"),
        }
    }
    let arch = arch.context("server-path needs --arch arm64|amd64")?;
    println!(
        "{}",
        container_mount::servers::server_path(arch, dev)?.display()
    );
    Ok(())
}

fn main() -> ExitCode {
    match real_main() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("container-mount: {e:#}");
            ExitCode::FAILURE
        }
    }
}
