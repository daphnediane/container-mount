/*
 * Copyright (c) 2026 Daphne Pfister
 * SPDX-License-Identifier: BSD-2-Clause
 * See LICENSE file for full license text
 */

//! GitHub Release plumbing shared by `fetch-servers` and `update`:
//! resolving a release, picking assets with their API-reported
//! SHA-256 digests, and verified downloads.
//!
//! Downloads go through the platform `curl`/`tar` instead of an HTTP
//! crate — the crate stays pure-Rust so the musl server build needs no
//! cross C toolchain. `DISTROFS_RELEASES_URL` overrides the API base
//! for testing (point it at a local static tree mirroring
//! `<base>/releases/latest`, `<base>/releases/tags/vX.Y.Z`, and the
//! asset `browser_download_url`s); it is honored only when the caller
//! passes `dev: true` from the explicit `--dev` flag — an env var
//! alone must never redirect release downloads (a shell-profile line
//! would be a silent backdoor).

use std::cmp::Ordering;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde_json::Value;
use sha2::{Digest, Sha256};

/// The GitHub repo releases come from — matches Cargo.toml's
/// `repository`, restated here so a typo can't redirect the download.
pub const REPO: &str = "daphnediane/distrofs";
/// The dev-only override for the releases API base.
pub const RELEASES_URL_ENV: &str = "DISTROFS_RELEASES_URL";

/// The releases API base — api.github.com normally, the env override
/// only under `--dev`.
pub fn releases_base(dev: bool) -> String {
    let base = format!("https://api.github.com/repos/{REPO}");
    let Ok(u) = std::env::var(RELEASES_URL_ENV) else {
        return base;
    };
    if u.is_empty() {
        return base;
    }
    if !dev {
        eprintln!("ignoring {RELEASES_URL_ENV} (pass --dev to honor it)");
        return base;
    }
    eprintln!("--dev: release lookups redirect to {u}");
    u.trim_end_matches('/').to_string()
}

/// `tag` minus the `v` prefix — release tags carry one, versions don't.
pub fn strip_v(tag: &str) -> &str {
    tag.strip_prefix('v').unwrap_or(tag)
}

/// `curl` the URL into memory (release JSON is small).
fn curl_get(url: &str) -> Result<Vec<u8>> {
    let out = Command::new("curl")
        .args(["-fsSL", "--"])
        .arg(url)
        .output()
        .context("cannot run curl — is it installed?")?;
    if !out.status.success() {
        bail!(
            "curl {} failed: {}",
            url,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(out.stdout)
}

/// `curl` the URL to `dest` (a temp file next to the final path).
fn curl_to(url: &str, dest: &Path) -> Result<()> {
    let status = Command::new("curl")
        .args(["-fSL", "-o"])
        .arg(dest)
        .arg("--")
        .arg(url)
        .status()
        .context("cannot run curl — is it installed?")?;
    if !status.success() {
        bail!("download of {url} failed");
    }
    Ok(())
}

/// `(version, release JSON)` for `latest` or an explicit tag.
pub fn fetch_release(version: Option<&str>, dev: bool) -> Result<(String, Value)> {
    let base = releases_base(dev);
    let (url, label) = match version {
        None => (
            format!("{base}/releases/latest"),
            "the latest release".to_string(),
        ),
        Some(v) => {
            let tag = format!("v{}", strip_v(v));
            (
                format!("{base}/releases/tags/{tag}"),
                format!("release {tag}"),
            )
        }
    };
    let body = curl_get(&url).with_context(|| format!("cannot fetch {label}"))?;
    let json: Value = serde_json::from_slice(&body).context("release response is not JSON")?;
    let tag = json
        .get("tag_name")
        .and_then(Value::as_str)
        .map(strip_v)
        .with_context(|| format!("{label} has no tag_name"))?
        .to_string();
    Ok((tag, json))
}

/// `X.Y.Z[-suffix]`: the numeric triple first, then a prerelease sorts
/// before its release (`0.6.0-dev` < `0.6.0`). Two prerelease suffixes
/// compare lexically — `dev` < `rc.N` — close enough for our tags.
pub fn cmp_version(a: &str, b: &str) -> Ordering {
    fn split(v: &str) -> ([u64; 3], Option<&str>) {
        let (core, pre) = match strip_v(v).split_once('-') {
            Some((c, p)) => (c, Some(p)),
            None => (strip_v(v), None),
        };
        let mut nums = [0u64; 3];
        for (n, part) in core.split('.').take(3).enumerate() {
            nums[n] = part.parse().unwrap_or(0);
        }
        (nums, pre)
    }
    let (an, ap) = split(a);
    let (bn, bp) = split(b);
    an.cmp(&bn).then_with(|| match (ap, bp) {
        (None, Some(_)) => Ordering::Greater,
        (Some(_), None) => Ordering::Less,
        (x, y) => x.cmp(&y),
    })
}

/// One release asset: name, download URL, and the `digest` field's
/// SHA-256 (`None` when the API didn't report one — the download then
/// can't be integrity-checked and the caller should say so).
pub struct Asset {
    pub name: String,
    pub url: String,
    pub sha256: Option<String>,
}

/// The release asset called `name`, or `None`.
pub fn find_asset(release: &Value, name: &str) -> Option<Asset> {
    release.get("assets")?.as_array()?.iter().find_map(|a| {
        if a.get("name")?.as_str()? != name {
            return None;
        }
        Some(Asset {
            name: name.to_string(),
            url: a.get("browser_download_url")?.as_str()?.to_string(),
            sha256: a
                .get("digest")
                .and_then(Value::as_str)
                .and_then(|d| d.strip_prefix("sha256:"))
                .map(str::to_string),
        })
    })
}

/// SHA-256 hex of a file.
pub fn sha256_file(path: &Path) -> Result<String> {
    let mut f = File::open(path).with_context(|| format!("cannot open {}", path.display()))?;
    let mut h = Sha256::new();
    let mut buf = [0u8; 64 << 10];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(crate::proto::hex(&h.finalize()))
}

/// Download `asset` into `dir` as `<dir>/<name>`, verifying against
/// the API-reported digest when one exists. A cached copy is re-verified
/// before reuse — the hash is the trust root.
pub fn download(asset: &Asset, dir: &Path) -> Result<PathBuf> {
    std::fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    let path = dir.join(&asset.name);
    if path.is_file()
        && let Some(d) = &asset.sha256
        && sha256_file(&path).ok().as_deref() == Some(d.as_str())
    {
        eprintln!("{}: already downloaded", asset.url);
        return Ok(path);
    }
    let tmp = dir.join(format!(".{}.part", asset.name));
    curl_to(&asset.url, &tmp)?;
    if let Some(d) = &asset.sha256 {
        let got = sha256_file(&tmp)?;
        if got != *d {
            let _ = std::fs::remove_file(&tmp);
            bail!(
                "SHA-256 mismatch for {}: expected {d}, got {got}",
                asset.url
            );
        }
    } else {
        eprintln!(
            "warning: the release API reported no digest for {} — downloaded unverified",
            asset.name
        );
    }
    std::fs::rename(&tmp, &path)
        .with_context(|| format!("cannot move the download to {}", path.display()))?;
    Ok(path)
}

/// `tar -xzf` a downloaded tarball into `dir`.
pub fn untar(tarball: &Path, dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    let status = Command::new("tar")
        .arg("-xzf")
        .arg(tarball)
        .arg("-C")
        .arg(dir)
        .status()
        .context("cannot run tar")?;
    if !status.success() {
        bail!("tar failed on {}", tarball.display());
    }
    Ok(())
}

/// A y/N question on stderr, answered on stdin (port of
/// container-distro's `install::prompt_yes`).
pub fn prompt_yes(question: &str) -> Result<bool> {
    use std::io::{IsTerminal, Write};
    eprint!("{question}");
    std::io::stderr().flush().ok();
    if !std::io::stdin().is_terminal() {
        bail!("that needs a yes — rerun on a terminal");
    }
    let mut line = String::new();
    std::io::stdin()
        .read_line(&mut line)
        .context("cannot read the reply")?;
    let a = line.trim();
    Ok(a.eq_ignore_ascii_case("y") || a.eq_ignore_ascii_case("yes"))
}

/// An interactive yes, or `force`. The context prints either way —
/// `--force` shouldn't hide *why* this needed confirming.
/// Non-interactive sessions get an error naming --force rather than a
/// prompt that reads EOF as "no".
pub fn consent(context: &str, question: &str, force: bool) -> Result<()> {
    use std::io::IsTerminal;
    eprintln!("{context}");
    if force {
        return Ok(());
    }
    if !std::io::stdin().is_terminal() {
        bail!("that needs a yes — rerun on a terminal or pass --force");
    }
    if !prompt_yes(question)? {
        bail!("declined");
    }
    Ok(())
}

/// Whether this process runs with euid 0.
pub fn is_root() -> bool {
    // SAFETY: geteuid is always safe to call.
    unsafe { libc::geteuid() == 0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_ordering() {
        assert_eq!(cmp_version("0.5.0", "0.5.0"), Ordering::Equal);
        assert_eq!(cmp_version("0.5.0", "v0.5.0"), Ordering::Equal);
        assert_eq!(cmp_version("0.5.0", "0.6.0"), Ordering::Less);
        assert_eq!(cmp_version("0.6.0-dev", "0.5.0"), Ordering::Greater);
        assert_eq!(cmp_version("0.6.0-dev", "0.6.0"), Ordering::Less);
        assert_eq!(cmp_version("0.6.0-rc.1", "0.6.0"), Ordering::Less);
        assert_eq!(cmp_version("0.6.0-dev", "0.6.0-rc.1"), Ordering::Less);
    }

    #[test]
    fn find_asset_reads_the_digest() {
        let r = serde_json::json!({
            "tag_name": "v0.1.0",
            "assets": [{
                "name": "distrofs-servers-0.1.0.tar.gz",
                "browser_download_url": "https://example/x",
                "digest": "sha256:aaaa",
            }],
        });
        let a = find_asset(&r, "distrofs-servers-0.1.0.tar.gz").unwrap();
        assert_eq!(a.sha256.as_deref(), Some("aaaa"));
        assert!(find_asset(&r, "nope").is_none());
    }
}
