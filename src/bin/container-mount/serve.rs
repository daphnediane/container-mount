/*
 * Copyright (c) 2026 Daphne Pfister
 * SPDX-License-Identifier: BSD-2-Clause
 * See LICENSE file for full license text
 */

//! `container-mount serve` — the guest half's command line and listener setup.
//!
//! The session token always comes from `CONTAINER_MOUNT_TOKEN` (the contract
//! with whatever launched us); where to listen is a flag or
//! `CONTAINER_MOUNT_SOCKET`, never a baked-in deployment path.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use container_mount::proto;

/// Where the server listens.
#[derive(Debug, Clone, PartialEq)]
pub enum Listen {
    /// A Unix socket at this path (the normal VMM-relay deployment).
    Unix(PathBuf),
    /// A TCP port — for engines that can only publish TCP, and testing.
    Tcp(String),
    /// An `AF_VSOCK` port — what the host reaches through `container`'s
    /// dial route, with no create-time socket relay. Ports below 1024
    /// need `CAP_NET_BIND_SERVICE`, so unprivileged guest processes
    /// can't squat them.
    Vsock(u32),
}

/// Options for `serve`.
#[derive(Debug, Clone, PartialEq)]
pub struct ServeArgs {
    /// Owner given to files and directories the host creates: `--user`
    /// resolved in the guest, then `--uid`/`--gid` overriding either
    /// half; root when neither is given.
    pub user: Option<String>,
    pub uid: Option<u32>,
    pub gid: Option<u32>,
    pub listen: Listen,
    /// Exit after this long with no connected client.
    pub idle: Duration,
    /// Exit when the first authenticated session ends — one server per
    /// host command, so the token lives only as long as that command.
    pub once: bool,
    /// Delete our own binary (and its directory, once empty) after
    /// startup — for per-command copies delivered by `container copy`,
    /// in images that may have no `rm`.
    pub remove_binary: bool,
    /// The variable holding the session token — `CONTAINER_MOUNT_TOKEN`
    /// unless `--token-env` names another.
    pub token_env: String,
}

const USAGE: &str = "usage: container-mount serve [--user NAME|UID[:GID]] [--uid UID] [--gid GID] \
     [--socket PATH | --listen unix:PATH | --listen tcp:ADDR | --listen vsock:PORT] \
     [--idle-secs N] [--once] [--remove-binary] [--token-env NAME]";

pub fn parse(args: &[String]) -> Result<ServeArgs> {
    let socket_env = std::env::var_os(proto::SOCKET_ENV).map(PathBuf::from);
    let mut out = ServeArgs {
        user: None,
        uid: None,
        gid: None,
        listen: Listen::Unix(socket_env.unwrap_or_else(|| PathBuf::from(proto::DEFAULT_SOCKET))),
        idle: Duration::from_secs(30),
        once: false,
        remove_binary: false,
        token_env: proto::TOKEN_ENV.into(),
    };
    let mut explicit = false;
    let mut it = args.iter();
    while let Some(flag) = it.next() {
        let mut value = || it.next().with_context(|| format!("{flag} needs a value"));
        match flag.as_str() {
            "--user" => out.user = Some(value()?.clone()),
            "--uid" => out.uid = Some(num(flag, value()?)?),
            "--gid" => out.gid = Some(num(flag, value()?)?),
            "--idle-secs" => out.idle = Duration::from_secs(num(flag, value()?)?.into()),
            "--once" => out.once = true,
            "--remove-binary" => out.remove_binary = true,
            // `container exec -e` appends to the container's own
            // environment without replacing a same-named entry, and
            // getenv returns the first — so a launcher that can't
            // control the target's environment picks a fresh name.
            "--token-env" => out.token_env = value()?.clone(),
            "--socket" => {
                set_listen(
                    &mut out,
                    &mut explicit,
                    flag,
                    Listen::Unix(PathBuf::from(value()?)),
                )?;
            }
            "--listen" => {
                let spec = value()?;
                let l = match spec.split_once(':') {
                    Some(("unix", p)) => Listen::Unix(PathBuf::from(p)),
                    Some(("tcp", a)) => Listen::Tcp(a.to_string()),
                    Some(("vsock", p)) => Listen::Vsock(num(flag, p)?),
                    _ => bail!("--listen wants unix:PATH, tcp:ADDR, or vsock:PORT, got `{spec}`"),
                };
                set_listen(&mut out, &mut explicit, flag, l)?;
            }
            _ => bail!("unknown argument `{flag}`\n{USAGE}"),
        }
    }
    Ok(out)
}

fn num(flag: &str, v: &str) -> Result<u32> {
    v.parse::<u32>()
        .with_context(|| format!("bad {flag} value `{v}`"))
}

fn set_listen(out: &mut ServeArgs, explicit: &mut bool, flag: &str, l: Listen) -> Result<()> {
    if *explicit {
        bail!("{flag} conflicts with an earlier --socket/--listen");
    }
    *explicit = true;
    out.listen = l;
    Ok(())
}

#[cfg(target_os = "linux")]
pub fn run(args: &[String]) -> Result<()> {
    Ok(crate::server::serve(&parse(args)?)?)
}

#[cfg(not(target_os = "linux"))]
pub fn run(_args: &[String]) -> Result<()> {
    let _ = parse(_args)?;
    bail!("container-mount serve only runs inside a Linux guest")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn test_parse_serve_flags() {
        let a = parse(&v(&[
            "--uid",
            "501",
            "--gid",
            "20",
            "--socket",
            "/x.sock",
            "--idle-secs",
            "5",
        ]))
        .unwrap();
        assert_eq!(
            (a.uid, a.gid, a.idle),
            (Some(501), Some(20), Duration::from_secs(5))
        );
        assert_eq!(a.user, None);
        let w = parse(&v(&["--user", "daphne:staff"])).unwrap();
        assert_eq!((w.user.as_deref(), w.uid), (Some("daphne:staff"), None));
        assert_eq!(a.listen, Listen::Unix(PathBuf::from("/x.sock")));
        let t = parse(&v(&["--listen", "tcp:0.0.0.0:9999"])).unwrap();
        assert_eq!(t.listen, Listen::Tcp("0.0.0.0:9999".into()));
        let u = parse(&v(&["--listen", "unix:/y.sock"])).unwrap();
        assert_eq!(u.listen, Listen::Unix(PathBuf::from("/y.sock")));
        let k = parse(&v(&[
            "--listen",
            "vsock:900",
            "--once",
            "--remove-binary",
            "--token-env",
            "T_1",
        ]))
        .unwrap();
        assert_eq!(k.token_env, "T_1");
        assert_eq!(u.token_env, proto::TOKEN_ENV);
        assert_eq!(
            (k.listen, k.once, k.remove_binary),
            (Listen::Vsock(900), true, true)
        );
        assert!(!u.once && !u.remove_binary);
    }

    #[test]
    fn test_parse_rejects_bad_input() {
        for bad in [
            &["--uid"][..],
            &["--uid", "-1"],
            &["--what"],
            &["--socket", "/a", "--listen", "tcp:1:2"],
            &["--listen", "bogus:1"],
            &["--listen", "vsock:x"],
        ] {
            assert!(parse(&v(bad)).is_err(), "{bad:?}");
        }
    }
}
