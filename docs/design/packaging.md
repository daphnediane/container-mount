# Packaging

How releases reach users, and why. Modeled on container-distro's
[packaging](https://github.com/daphnediane/container-distro/blob/main/docs/design/packaging.md);
decided: a **macOS Installer `.pkg`** plus a **`cargo-binstall`-compatible
client tarball** and a **guest-server bundle** attached to each GitHub
Release by `.github/workflows/release.yml`, alongside
`THIRD-PARTY-NOTICES.yaml` and a CycloneDX SBOM when
`cargo-cyclonedx` is present. `cargo install` from a checkout is the
baseline path; crates.io publishing is a 1.0-milestone decision
(TODO.md).

## Assets

`scripts/build-pkg.sh` produces, under `scratch/pkg/`:

| Asset | Contents | Consumer |
| ----- | -------- | -------- |
| `distrofs-<ver>.pkg` | binary, both musl servers, uninstall/update scripts, license docs under `/usr/local` | manual double-click install |
| `distrofs-servers-<ver>.tar.gz` | `distrofs-server-aarch64` + `distrofs-server-x86_64` | `distrofs fetch-servers` |
| `distrofs-<triple>-v<ver>.tar.gz` | `distrofs` + the server binaries + LICENSE/notices | `cargo binstall`, `distrofs update` |
| `distrofs-<ver>-sbom.cdx.json` | CycloneDX 1.5 dep inventory | CVE scanners |

Unsigned CI builds keep the same names plus `-unsigned` (pkg and
client tarball — `update` only takes the unsigned tarball with
consent, matching container-distro's pkg rule). The server bundle has
no signed variant: musl binaries are ELF and can't carry a Mac
signature — their integrity is the embedded `distrofs-version:` stamp
plus the API-reported SHA-256 digest checked at download
(`src/release.rs`).

## The `.pkg`

Same shape as container-distro's: `pkgbuild` component package wrapped
by `productbuild --distribution` (`packaging/Distribution.xml`),
`hostArchitectures="arm64"`, `rootVolumeOnly`, no install scripts —
the payload is files only, so the installer adds no code-execution
surface. Unlike container-distro there's no `container` prerequisite,
so no minimum-OS gate; that changes if the FSKit module joins the
payload (it will want macOS ≥ 26).

Payload:

- `bin/distrofs` — host client/CLI
- `bin/uninstall-distrofs.sh`, `bin/update-distrofs.sh` — the
  receipt-driven remover and the `distrofs update` wrapper, shipped so
  the layout carries its own maintenance tools like Apple's container
  pkg does
- `libexec/distrofs/distrofs-server-{aarch64,x86_64}` — the guest
  servers, where `servers::server_dir_for` discovers them
- `share/doc/distrofs/{LICENSE,THIRD-PARTY-NOTICES.yaml}`

Upgrades obsolete stale files the same way: Installer removes
receipt-owned paths the new payload doesn't carry. Uninstall is
`pkgutil`-receipt-driven (never a hardcoded list) — see the header of
`packaging/uninstall-distrofs.sh`; distrofs keeps no per-user state,
so there is no keep/delete-data prompt.

## Self-update and fetch

`distrofs update` replaces the running binary from the client tarball
(digest-verified, codesign + same-team check on macOS); it refuses to
self-elevate, so a root-owned `/usr/local` pkg install updates via the
new pkg instead — `update-distrofs.sh` stays a thin wrapper either
way. `distrofs fetch-servers` installs just the servers under
`<prefix>/libexec/distrofs/` — the piece `cargo install` can't
provide and the download path for users who only need the guest side
(e.g. container-distro installs).

## Signing and notarization

CI has no keychain, so `release.yml` attaches `-unsigned` artifacts;
the maintainer replaces them at draft review:

```bash
scripts/build-pkg-signed.sh   # sources packaging/signing.local.conf
```

`packaging/signing.local.conf` is gitignored (see
`signing.conf.example`) and supplies `CODESIGN_IDENTITY` (Developer ID
Application → `codesign --options runtime --timestamp` on the host
binary), `PKG_SIGN_IDENTITY` (Developer ID Installer → `productbuild
--sign`), and `NOTARY_PROFILE` (`notarytool submit --wait` +
`stapler staple`). Signed artifacts keep the clean names; the private
key stays on the maintainer's Mac — CI signing is deliberately
unwired, same as container-distro.
