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
}

/// Options for `serve`.
#[derive(Debug, Clone, PartialEq)]
pub struct ServeArgs {
    /// Owner given to files and directories the host creates.
    pub uid: u32,
    pub gid: u32,
    pub listen: Listen,
    /// Exit after this long with no connected client.
    pub idle: Duration,
}

const USAGE: &str = "usage: container-mount serve [--uid UID] [--gid GID] \
     [--socket PATH | --listen unix:PATH | --listen tcp:ADDR] [--idle-secs N]";

pub fn parse(args: &[String]) -> Result<ServeArgs> {
    let socket_env = std::env::var_os(proto::SOCKET_ENV).map(PathBuf::from);
    let mut out = ServeArgs {
        uid: 0,
        gid: 0,
        listen: Listen::Unix(socket_env.unwrap_or_else(|| PathBuf::from(proto::DEFAULT_SOCKET))),
        idle: Duration::from_secs(30),
    };
    let mut explicit = false;
    let mut it = args.iter();
    while let Some(flag) = it.next() {
        let mut value = || it.next().with_context(|| format!("{flag} needs a value"));
        match flag.as_str() {
            "--uid" => out.uid = num(flag, value()?)?,
            "--gid" => out.gid = num(flag, value()?)?,
            "--idle-secs" => out.idle = Duration::from_secs(num(flag, value()?)?.into()),
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
                    _ => bail!("--listen wants unix:PATH or tcp:ADDR, got `{spec}`"),
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
        assert_eq!((a.uid, a.gid, a.idle), (501, 20, Duration::from_secs(5)));
        assert_eq!(a.listen, Listen::Unix(PathBuf::from("/x.sock")));
        let t = parse(&v(&["--listen", "tcp:0.0.0.0:9999"])).unwrap();
        assert_eq!(t.listen, Listen::Tcp("0.0.0.0:9999".into()));
        let u = parse(&v(&["--listen", "unix:/y.sock"])).unwrap();
        assert_eq!(u.listen, Listen::Unix(PathBuf::from("/y.sock")));
    }

    #[test]
    fn test_parse_rejects_bad_input() {
        for bad in [
            &["--uid"][..],
            &["--uid", "-1"],
            &["--what"],
            &["--socket", "/a", "--listen", "tcp:1:2"],
            &["--listen", "bogus:1"],
        ] {
            assert!(parse(&v(bad)).is_err(), "{bad:?}");
        }
    }
}
