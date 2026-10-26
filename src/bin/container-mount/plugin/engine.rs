/*
 * Copyright (c) 2026 Daphne Pfister
 * SPDX-License-Identifier: BSD-2-Clause
 * See LICENSE file for full license text
 */

//! The `container` CLI calls the plugin makes: resolving the binary,
//! `inspect`, `copy`, `exec`, and `system status`.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

/// Where Apple's installer puts `container`.
const DEFAULT_CONTAINER: &str = "/usr/local/bin/container";
/// `--dev` only: run this `container` binary instead.
const CONTAINER_CLI_ENV: &str = "CONTAINER_CLI";

/// The `container` binary: `CONTAINER_CLI` under `--dev`, then the
/// installer's path, then `PATH`. Selecting a user-privilege binary is
/// PATH-equivalent, but it stays behind `--dev` like container-distro's.
/// `None` when none resolves.
pub fn resolve(dev: bool) -> Option<PathBuf> {
    if dev && let Some(p) = std::env::var_os(CONTAINER_CLI_ENV) {
        return Some(p.into());
    }
    if Path::new(DEFAULT_CONTAINER).is_file() {
        return Some(DEFAULT_CONTAINER.into());
    }
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|d| d.join("container"))
        .find(|p| p.is_file())
}

pub fn command(dev: bool) -> Command {
    Command::new(resolve(dev).map_or_else(|| OsString::from("container"), PathBuf::into_os_string))
}

/// Run `container ARGS`, failing with its stderr on a non-zero exit.
fn run(dev: bool, args: &[&std::ffi::OsStr]) -> Result<Vec<u8>> {
    let out = command(dev)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .context("couldn't run `container`")?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let err = err.trim().trim_start_matches("Error: ");
        bail!("container {}: {err}", args[0].to_string_lossy());
    }
    Ok(out.stdout)
}

/// The facts about a target container the plugin needs.
#[derive(Debug, Clone, PartialEq)]
pub struct Info {
    pub id: String,
    pub state: String,
    /// Who relative paths and new files belong to by default: a
    /// machine's provisioned account (`uid:gid`), else the container's
    /// init-process user (`NAME|UID[:GID]`); `None` means root.
    pub user: Option<String>,
}

/// `container inspect ID`; failing that, a `container machine` named
/// `ID`, through its backing container (`containerId`). A container id
/// wins over a same-named machine.
pub fn inspect(dev: bool, id: &str) -> Result<Info> {
    match run(dev, &["inspect".as_ref(), id.as_ref()]) {
        Ok(out) => parse_inspect(&out),
        Err(e) => {
            let Ok(out) = run(dev, &["machine".as_ref(), "inspect".as_ref(), id.as_ref()]) else {
                return Err(e);
            };
            let (backing, user) = parse_machine(&out)?;
            let out = run(dev, &["inspect".as_ref(), backing.as_ref()])?;
            Ok(Info {
                user: user.or(parse_inspect(&out)?.user),
                ..parse_inspect(&out)?
            })
        }
    }
}

/// `containerId` and the provisioned `userSetup` account (as
/// `uid:gid`) from `container machine inspect` output.
fn parse_machine(json: &[u8]) -> Result<(String, Option<String>)> {
    let v: serde_json::Value =
        serde_json::from_slice(json).context("unreadable `container machine inspect` output")?;
    let id = v
        .pointer("/0/containerId")
        .and_then(|v| v.as_str())
        .map(str::to_owned)
        .context("`container machine inspect` has no containerId")?;
    let user = v.pointer("/0/userSetup").and_then(|u| {
        Some(format!(
            "{}:{}",
            u.get("uid")?.as_u64()?,
            u.get("gid")?.as_u64()?
        ))
    });
    Ok((id, user))
}

/// The init process's user from `container inspect`: either
/// `{"raw": {"userString": "…"}}` or `{"id": {"uid": …, "gid": …}}`.
fn init_user(c: &serde_json::Value) -> Option<String> {
    let u = c.pointer("/configuration/initProcess/user")?;
    if let Some(s) = u.pointer("/raw/userString").and_then(|v| v.as_str()) {
        return (!s.is_empty()).then(|| s.to_owned());
    }
    let id = u.get("id")?;
    Some(format!(
        "{}:{}",
        id.get("uid")?.as_u64()?,
        id.get("gid")?.as_u64()?
    ))
}

fn parse_inspect(json: &[u8]) -> Result<Info> {
    let v: serde_json::Value =
        serde_json::from_slice(json).context("unreadable `container inspect` output")?;
    let c = v.get(0).context("`container inspect` returned nothing")?;
    let field = |ptr: &str| c.pointer(ptr).and_then(|v| v.as_str()).map(str::to_owned);
    Ok(Info {
        id: field("/configuration/id").context("`container inspect` has no id")?,
        state: field("/status/state").unwrap_or_else(|| "unknown".into()),
        user: init_user(c),
    })
}

/// `container copy SRC ID:DEST` — copies a host directory in, keeping
/// file modes; missing guest parents are created root-owned.
pub fn copy_in(dev: bool, src: &Path, id: &str, dest: &str) -> Result<()> {
    let dest = format!("{id}:{dest}");
    run(dev, &["copy".as_ref(), src.as_os_str(), dest.as_ref()]).map(drop)
}

/// `container exec -d -u 0 -e NAME… ID ARGV…` — starts a detached root
/// process. Each `env` value is set only in the `container` child's
/// environment and its name passed bare (`-e NAME` inherits), so the
/// value never lands in an argv.
pub fn exec_detached(dev: bool, id: &str, env: &[(&str, &str)], argv: &[String]) -> Result<()> {
    let mut cmd = command(dev);
    cmd.args(["exec", "-d", "-u", "0"]);
    for (name, value) in env {
        cmd.arg("-e").arg(name).env(name, value);
    }
    cmd.arg(id).args(argv).stdin(Stdio::null());
    let out = cmd.output().context("couldn't run `container`")?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        bail!(
            "container exec: {}",
            err.trim().trim_start_matches("Error: ")
        );
    }
    Ok(())
}

/// The `container` daemon version (`server.version` in `system status`),
/// or `None` when it isn't answering.
pub fn server_version(dev: bool) -> Option<String> {
    let out = run(
        dev,
        &[
            "system".as_ref(),
            "status".as_ref(),
            "--format".as_ref(),
            "json".as_ref(),
        ],
    )
    .ok()?;
    let v: serde_json::Value = serde_json::from_slice(&out).ok()?;
    v.pointer("/server/version")?.as_str().map(str::to_owned)
}

/// A fresh `0700` directory under the per-user temp dir.
pub fn private_temp_dir() -> Result<PathBuf> {
    let template = std::env::temp_dir().join("container-mount.XXXXXX");
    let mut raw =
        std::ffi::CString::new(template.as_os_str().as_encoded_bytes())?.into_bytes_with_nul();
    // SAFETY: `raw` is a writable NUL-terminated template.
    let p = unsafe { libc::mkdtemp(raw.as_mut_ptr().cast()) };
    if p.is_null() {
        return Err(std::io::Error::last_os_error()).context("couldn't create a staging directory");
    }
    raw.pop();
    // SAFETY: mkdtemp filled in the template in place.
    Ok(PathBuf::from(unsafe {
        OsString::from_encoded_bytes_unchecked(raw)
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_inspect_reads_id_and_state() {
        let json = br#"[{"configuration":{"id":"web","platform":{"architecture":"arm64"}},
                        "status":{"state":"running"},"id":"web"}]"#;
        assert_eq!(
            parse_inspect(json).unwrap(),
            Info {
                id: "web".into(),
                state: "running".into(),
                user: None,
            }
        );
        let json = br#"[{"configuration":{"id":"u","initProcess":{"user":{"raw":{"userString":"1000:1000"}}}},
                        "status":{"state":"running"}}]"#;
        assert_eq!(
            parse_inspect(json).unwrap().user.as_deref(),
            Some("1000:1000")
        );
        let json =
            br#"[{"configuration":{"id":"u","initProcess":{"user":{"id":{"uid":7,"gid":8}}}},
                        "status":{"state":"running"}}]"#;
        assert_eq!(parse_inspect(json).unwrap().user.as_deref(), Some("7:8"));
        let json =
            br#"[{"configuration":{"id":"u","initProcess":{"user":{"raw":{"userString":""}}}},
                        "status":{"state":"running"}}]"#;
        assert_eq!(parse_inspect(json).unwrap().user, None);
        assert!(parse_inspect(b"[]").is_err());
        assert!(parse_inspect(b"nope").is_err());
    }

    #[test]
    fn test_parse_machine_reads_backing_id_and_user() {
        let json = br#"[{"containerId":"dev-c2bb52","id":"dev","homeMount":"rw",
                        "userSetup":{"username":"me","uid":501,"gid":20,"home":"/Users/me"}}]"#;
        assert_eq!(
            parse_machine(json).unwrap(),
            ("dev-c2bb52".into(), Some("501:20".into()))
        );
        let bare = br#"[{"containerId":"dev-c2bb52","id":"dev"}]"#;
        assert_eq!(parse_machine(bare).unwrap(), ("dev-c2bb52".into(), None));
        assert!(parse_machine(br#"[{"id":"dev"}]"#).is_err());
    }

    #[test]
    fn test_private_temp_dir_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let d = private_temp_dir().unwrap();
        let mode = std::fs::metadata(&d).unwrap().permissions().mode() & 0o777;
        std::fs::remove_dir(&d).unwrap();
        assert_eq!(mode, 0o700);
    }
}
