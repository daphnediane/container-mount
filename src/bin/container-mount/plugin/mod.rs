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

mod cli;
mod engine;
pub mod install;
mod upstream;

use std::collections::HashMap;
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
use cli::{Base, Ref, Remote, Word};

/// The plugin CLI contract's version — what container-distro checks,
/// separate from the wire protocol version. 0 through the 0.x releases
/// (match exactly); 1 is reserved for the 1.0.0 contract.
pub const INTERFACE: u32 = 0;

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
usage: container mount [--dev] COMMAND [OPTIONS] [--] ARGS...
       container mount [OPTIONS] [--] TARGET [DIR]   (attach; not yet)

  ls [-l] TARGET[:PATH]          list a guest directory
  stat [-L] TARGET:PATH          one entry's attributes
  cat TARGET:PATH...             guest files to stdout
  copy SRC DEST  (alias: cp)     host<->guest, within a guest
                                 (server-side), or guest->guest
  list [--format json]           attached mounts
  help                           this text

  TARGET is a running container id or machine name, optionally as
  USER@TARGET. PATH is absolute, or relative to that user's home; `~`
  and `~USER` work too. Host paths with a colon need a `/` first
  (`./a:b`). The user picks the home and owns new files; the server
  still reads and writes as guest root (no permission checks).

  -u, --user NAME|UID[:GID]      the guest user for references without
                                 USER@ (default: the machine's account,
                                 else the container's init-process user,
                                 else root)
  --                             end options; the next word is a TARGET
                                 or path even if it spells a verb
  --version [--format json]      plugin, interface, and protocol versions
  --dev                          dev seams: CONTAINER_CLI, in-tree servers

  Reserved for later: attach detach inspect prune mv rm mkdir rmdir ln
  touch chmod chown du df find sync export import edit open events
  prepare.";

/// Entry point for plugin mode. `args` excludes the program name.
pub fn run(args: &[String]) -> Result<()> {
    // `container` may pass the subcommand name as argv[1].
    let mut args = match args.first() {
        Some(a) if a == "mount" => &args[1..],
        _ => args,
    };
    let mut dev = false;
    while let Some(a) = args.first() {
        match a.as_str() {
            "--dev" => dev = true,
            "--version" => return version(&args[1..]),
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(());
            }
            _ => break,
        }
        args = &args[1..];
    }
    let Some(first) = args.first() else {
        return list(&[]);
    };
    if first == "--" {
        return attach(&args[1..]);
    }
    match cli::classify(first) {
        Word::Verb => verb(dev, first, &args[1..]),
        Word::Reserved => bail!("`container mount {first}` is not implemented yet"),
        Word::Target if first.starts_with('-') => attach(args),
        Word::Target => match cli::near_verbs(first).as_slice() {
            [] => attach(args),
            near => bail!(
                "unknown command `{first}` — did you mean {}? \
                 (`container mount -- {first}` attaches a container by that name)",
                near.iter()
                    .map(|v| format!("`{v}`"))
                    .collect::<Vec<_>>()
                    .join(" or ")
            ),
        },
    }
}

fn verb(dev: bool, name: &str, args: &[String]) -> Result<()> {
    match name {
        "ls" => ls(dev, args),
        "stat" => stat(dev, args),
        "cat" => cat(dev, args),
        "copy" | "cp" => copy(dev, args),
        "list" => list(args),
        "help" => {
            println!("{USAGE}");
            Ok(())
        }
        _ => unreachable!("cli::VERBS lists only these"),
    }
}

/// The implied (or, later, explicit) `attach`.
fn attach(_args: &[String]) -> Result<()> {
    bail!(
        "attaching a mount point needs the FSKit front end, which isn't here yet — \
         `container mount ls|cat|copy TARGET:PATH` work today"
    )
}

fn version(args: &[String]) -> Result<()> {
    match args {
        [] => println!(
            "container mount {} (interface {INTERFACE}, protocol {}, wire {})",
            env!("CARGO_PKG_VERSION"),
            proto::VERSION,
            proto::WIRE_HEX
        ),
        [f, j] if f == "--format" && j == "json" => println!(
            "{}",
            serde_json::json!({
                "interface": INTERFACE,
                "version": env!("CARGO_PKG_VERSION"),
                "protocol": proto::VERSION,
                "wire": proto::WIRE_HEX,
            })
        ),
        _ => bail!("usage: container mount --version [--format json]"),
    }
    Ok(())
}

/// `list [--format json]` — attached mounts. None exist until `attach`
/// lands with FSKit, so the contract's shape is all there is today.
fn list(args: &[String]) -> Result<()> {
    match args {
        [] => eprintln!("no mounts attached (attach arrives with the FSKit front end)"),
        [f, j] if f == "--format" && j == "json" => println!("[]"),
        _ => bail!("usage: container mount list [--format json]"),
    }
    Ok(())
}

/// A verb's options: `-u/--user`, the verb's own boolean `flags`, and
/// positionals (everything after `--` included).
struct Opts {
    user: Option<String>,
    set: Vec<&'static str>,
    args: Vec<String>,
}

fn options(args: &[String], flags: &[&'static str]) -> Result<Opts> {
    let mut o = Opts {
        user: None,
        set: Vec::new(),
        args: Vec::new(),
    };
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--" => {
                o.args.extend(it.cloned());
                break;
            }
            "-u" | "--user" => o.user = Some(it.next().context("--user needs a value")?.clone()),
            f if flags.contains(&f) => o.set.extend(flags.iter().find(|x| **x == f)),
            f if f.starts_with('-') && f.len() > 1 => bail!("unknown option `{f}`"),
            _ => o.args.push(a.clone()),
        }
    }
    Ok(o)
}

/// The one remote operand of `stat` and friends.
fn remote(arg: &str) -> Result<Remote> {
    match cli::parse_ref(arg) {
        Ref::Remote(r) => Ok(r),
        Ref::Local(_) => bail!("`{arg}` is a host path — name the guest side as TARGET:PATH"),
    }
}

/// Opened sessions, one per (target, user) — `cat a:x a:y` reuses one.
struct Sessions<'a> {
    dev: bool,
    /// `--user`, for references without their own `USER@`.
    user: Option<&'a str>,
    open: HashMap<(String, Option<String>), Session>,
}

impl<'a> Sessions<'a> {
    fn new(dev: bool, user: Option<&'a str>) -> Self {
        Sessions {
            dev,
            user,
            open: HashMap::new(),
        }
    }

    /// The session for `r`, and `r`'s path as an absolute guest path.
    fn get(&mut self, r: &Remote) -> Result<(&Session, String)> {
        let user = r.user.as_deref().or(self.user);
        let key = (r.target.clone(), user.map(str::to_owned));
        if !self.open.contains_key(&key) {
            let s = Session::open(self.dev, &r.target, user)?;
            self.open.insert(key.clone(), s);
        }
        let s = &self.open[&key];
        let path = s.path(&r.path)?;
        Ok((s, path))
    }
}

fn ls(dev: bool, args: &[String]) -> Result<()> {
    let o = options(args, &["-l", "--long"])?;
    let [arg] = o.args.as_slice() else {
        bail!(
            "usage: container mount ls [-l] [USER@]TARGET[:PATH] \
             (`container mount list` shows attached mounts)"
        );
    };
    // A bare TARGET lists its user's home — `ls` is always remote.
    let r = match cli::parse_ref(&format!(
        "{arg}{}",
        if arg.contains(':') { "" } else { ":" }
    )) {
        Ref::Remote(r) => r,
        Ref::Local(_) => remote(arg)?,
    };
    let mut sessions = Sessions::new(dev, o.user.as_deref());
    let (s, path) = sessions.get(&r)?;
    fscli::ls(&s.client, &path, !o.set.is_empty())
}

fn stat(dev: bool, args: &[String]) -> Result<()> {
    let o = options(args, &["-L", "--dereference"])?;
    let [arg] = o.args.as_slice() else {
        bail!("usage: container mount stat [-L] [USER@]TARGET:PATH");
    };
    let mut sessions = Sessions::new(dev, o.user.as_deref());
    let (s, path) = sessions.get(&remote(arg)?)?;
    fscli::stat(&s.client, &path, !o.set.is_empty())
}

fn cat(dev: bool, args: &[String]) -> Result<()> {
    let o = options(args, &[])?;
    if o.args.is_empty() {
        bail!("usage: container mount cat [USER@]TARGET:PATH...");
    }
    let mut sessions = Sessions::new(dev, o.user.as_deref());
    for arg in &o.args {
        let (s, path) = sessions.get(&remote(arg)?)?;
        fscli::get(&s.client, &path, None)?;
    }
    Ok(())
}

/// `copy SRC DEST` — `container copy` semantics in every direction:
/// into an existing directory keeps the source name, otherwise DEST is
/// the new name. Directories are not copied yet.
fn copy(dev: bool, args: &[String]) -> Result<()> {
    // -r is accepted for scp/cp muscle memory.
    let o = options(args, &["-r", "-R", "--recursive"])?;
    let [src, dst] = o.args.as_slice() else {
        bail!("usage: container mount copy SRC DEST (one or both [USER@]TARGET:PATH)");
    };
    let mut sessions = Sessions::new(dev, o.user.as_deref());
    match (cli::parse_ref(src), cli::parse_ref(dst)) {
        (Ref::Local(_), Ref::Local(_)) => {
            bail!("one of SRC and DEST must be TARGET:PATH")
        }
        (Ref::Local(from), Ref::Remote(to)) => {
            let (s, path) = sessions.get(&to)?;
            fscli::put(&s.client, Path::new(&from), &path)
        }
        (Ref::Remote(from), Ref::Local(to)) => {
            let (s, path) = sessions.get(&from)?;
            fscli::get(&s.client, &path, Some(Path::new(&to)))
        }
        // One guest as one user: a server-side copy, reflinked where
        // possible.
        (Ref::Remote(a), Ref::Remote(b))
            if a.target == b.target
                && a.user.as_deref().or(sessions.user) == b.user.as_deref().or(sessions.user) =>
        {
            let (s, from) = sessions.get(&a)?;
            let to = s.path(&b.path)?;
            fscli::cp(&s.client, &from, &to)
        }
        // Two guests (or two users): through a private host staging
        // file, which keeps sparse files sparse on both legs.
        (Ref::Remote(a), Ref::Remote(b)) => {
            let from = sessions.get(&a)?.1;
            let name = from
                .rsplit('/')
                .next()
                .filter(|n| !n.is_empty())
                .unwrap_or("file")
                .to_owned();
            let stage = engine::private_temp_dir()?;
            let staged = stage.join(name);
            let result = (|| {
                fscli::get(&sessions.get(&a)?.0.client, &from, Some(&staged))?;
                let (s, to) = sessions.get(&b)?;
                fscli::put(&s.client, &staged, &to)
            })();
            let _ = std::fs::remove_dir_all(&stage);
            result
        }
    }
}

/// A connected, authenticated server in one target, acting as one
/// guest user.
struct Session {
    client: Client,
    /// The user's home, for relative and `~` paths.
    home: Vec<u8>,
}

impl Session {
    /// Start a server in running `target` as `user` (default: the
    /// target's own default user, else root) and learn that user's home.
    fn open(dev: bool, target: &str, user: Option<&str>) -> Result<Session> {
        upstream::warn_unverified_version(dev);
        let info = engine::inspect(dev, target)?;
        if info.state != "running" {
            bail!(
                "{target} is {}; `container mount` needs a running container",
                info.state
            );
        }
        let spec = user
            .map(str::to_owned)
            .or(info.user.clone())
            .unwrap_or_else(|| "0".into());
        let client = connect(dev, &info.id, &spec)?;
        let home = client
            .lookup_user(spec.as_bytes())
            .with_context(|| format!("can't resolve user `{spec}` in {target}"))?
            .home;
        Ok(Session { client, home })
    }

    /// A remote PATH as an absolute guest path.
    fn path(&self, path: &str) -> Result<String> {
        Ok(match cli::base(path) {
            Base::Root(p) => p.to_owned(),
            Base::Home(rest) => cli::join(&self.home, rest),
            Base::UserHome { user, rest } => {
                let u = self
                    .client
                    .lookup_user(user.as_bytes())
                    .with_context(|| format!("no guest user `{user}` for `~{user}`"))?;
                cli::join(&u.home, rest)
            }
        })
    }
}

/// Deliver, start, and dial a server in running container `id` that
/// owns new files as `user`.
fn connect(dev: bool, id: &str, user: &str) -> Result<Client> {
    let token = proto::random32().context("couldn't generate a session token")?;
    let random = proto::random32().context("couldn't pick a port")?;
    let port = PORTS.start + u32::from_le_bytes(random[..4].try_into()?) % PORTS.len() as u32;
    let nonce = proto::hex(&random[4..12]);
    let guest_bin = deliver(dev, id, &nonce)?;
    // A per-command variable name: the target's own environment may
    // already define CONTAINER_MOUNT_TOKEN, and its entry would win.
    let token_env = format!("{}_{nonce}", proto::TOKEN_ENV);
    engine::exec_detached(
        dev,
        id,
        &[(&token_env, &proto::hex(&token))],
        &[
            guest_bin,
            "serve".into(),
            "--token-env".into(),
            token_env.clone(),
            "--user".into(),
            user.into(),
            "--listen".into(),
            format!("vsock:{port}"),
            "--once".into(),
            "--remove-binary".into(),
            "--idle-secs".into(),
            SERVER_IDLE_SECS.into(),
        ],
    )?;
    dial_and_handshake(id, port, &token)
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
