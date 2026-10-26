# Packaging

How releases reach users, and why. Modeled on container-distro's
[packaging](https://github.com/daphnediane/container-distro/blob/main/docs/design/packaging.md);
decided: a **macOS Installer `.pkg`** plus a **`cargo-binstall`-compatible
client tarball** and a **guest-server bundle** attached to each GitHub
Release by `.github/workflows/release.yml`, alongside
`THIRD-PARTY-NOTICES.yaml` and a CycloneDX SBOM when
`cargo-cyclonedx` is present. `cargo install` from a checkout is the
baseline path; crates.io publishing is a 1.0-milestone decision
([work item](../work/packaging/1.0/crates-io-publishing.md)).

## Assets

`scripts/build-pkg.sh` produces, under `scratch/pkg/`:

| Asset                                    | Contents                                                                                                     | Consumer                                   |
| ---------------------------------------- | ------------------------------------------------------------------------------------------------------------ | ------------------------------------------ |
| `container-mount-<ver>.pkg`              | binary, both musl servers, uninstall/update scripts, license docs under `/usr/local`                         | manual double-click install                |
| `container-mount-servers-<ver>.tar.gz`   | `aarch64-unknown-linux-musl/container-mount` + `x86_64-unknown-linux-musl/container-mount` (per-triple dirs) | `container-mount fetch-servers`            |
| `container-mount-<triple>-v<ver>.tar.gz` | `container-mount` + the per-triple server dirs + LICENSE/notices                                             | `cargo binstall`, `container-mount update` |
| `container-mount-<ver>-sbom.cdx.json`    | CycloneDX 1.5 dep inventory                                                                                  | CVE scanners                               |

Unsigned CI builds keep the same names plus `-unsigned` (pkg and
client tarball — `update` only takes the unsigned tarball with
consent, matching container-distro's pkg rule). The server bundle has
no signed variant: musl binaries are ELF and can't carry a Mac
signature — their integrity is the embedded `container-mount-version:` stamp
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

Payload — the per-triple layout of
[install-layout.md](install-layout.md); release builds carry the
`packaged` and `clap` cargo features:

- `bin/container-mount` — host client/CLI
- `bin/uninstall-container-mount.sh`, `bin/update-container-mount.sh` — the
  receipt-driven remover and the `container-mount update` wrapper, shipped so
  the layout carries its own maintenance tools like Apple's container
  pkg does
- `libexec/container-mount/<triple>/container-mount` — the guest servers, one dir
  per guest platform (`aarch64-`/`x86_64-unknown-linux-musl` today),
  where `servers::server_dir_for` discovers them
- `libexec/container-plugins/mount/{config.toml,bin/mount,libexec/container-mount/<triple>/container-mount}`
  — the `container mount` plugin: a copy of the (signed) binary and
  the servers, produced by `install-all --no-fetch --plugin-dir
  <payload>/libexec/container-plugins` like container-distro's payload
  plugin; Apple's `container` shares the `/usr/local` root, so this is
  where it looks
- `share/doc/container-mount/{LICENSE,THIRD-PARTY-NOTICES.yaml}`
- `share/man/man1/container-mount*.1` — `clap_mangen` output on clap-feature
  builds, staged via `install-man` like container-distro's

Upgrades obsolete stale files the same way: Installer removes
receipt-owned paths the new payload doesn't carry. Uninstall is
`pkgutil`-receipt-driven (never a hardcoded list) — see the header of
`packaging/uninstall-container-mount.sh`. container-mount's one piece of per-user
state is the fetched-servers dir ([install-layout]), so uninstall
offers a keep/delete-data prompt for it.

## Self-update and fetch

`container-mount update` replaces the running binary from the client tarball
(digest-verified, codesign + same-team check on macOS); it refuses to
self-elevate, so a root-owned `/usr/local` pkg install updates via the
new pkg instead — `update-container-mount.sh` stays a thin wrapper either
way. `container-mount fetch-servers` installs just the servers — the piece
`cargo install` can't provide and the download path for users who
only need the guest side (e.g. container-distro installs). It fetches
the bundle matching its own version and lands servers under
`<prefix>/libexec/container-mount/<triple>/`, or the per-user servers dir
when the prefix isn't writable; on `packaged` builds the bundled
servers count as already fetched. The full semantics and the
embedder contract are in [install-layout.md](install-layout.md).

## Signing and notarization

CI has no keychain, so `release.yml` attaches `-unsigned` artifacts;
the maintainer adds the signed ones at draft review:

```bash
scripts/build-pkg-signed.sh                 # sources packaging/signing.local.conf
scripts/upload-release-artifacts.sh vX.Y.Z  # verifies, uploads, drops -unsigned
```

`packaging/signing.local.conf` is gitignored (see
`signing.conf.example`) and supplies the identities; the private keys
stay on the maintainer's Mac — CI signing is deliberately unwired,
same as container-distro.

| What         | Identity                                                     | Tool                                          |
| ------------ | ------------------------------------------------------------ | --------------------------------------------- |
| host binary  | `Developer ID Application` (hardened runtime, `--timestamp`) | `codesign`                                    |
| `.pkg`       | `Developer ID Installer`                                     | `productbuild --sign`                         |
| tarball      | `Developer ID Application` detached signature (`.sig`)       | `codesign --detached --timestamp`             |
| tarball      | `GPG_KEY_ID` armored signature (`.asc`, optional)            | `gpg --detach-sign`                           |
| notarization | `NOTARY_PROFILE` keychain profile                            | `notarytool submit --wait` + `stapler staple` |

The tarball's `.sig` is a detached code signature of the archive
itself, so it verifies with `codesign --verify --detached=<file>.sig
<file>` against the same Apple PKI chain as the binary inside; the
`.asc` covers people who expect a PGP signature on a tarball. Both
are sidecars because the `{name}-{target}-v{version}.tar.gz` name is
fixed by the binstall metadata. That fixed name is also why unsigned
builds mangle the archive name (`-unsigned.tar.gz`) instead of
shipping the canonical name unsigned: the canonical assets only ever
appear in their final signed form, so a signed upload adds assets
instead of changing ones already on the release.
`upload-release-artifacts.sh` verifies the pkg signature, the `.sig`,
and the `.asc` when present, refuses to touch a release that is no
longer a draft, and deletes the `-unsigned` pkg and tarball CI
attached. The server bundle stays as CI built it — it has no signed
variant.

### Build provenance

`build-pkg.sh` is an orchestrator over `scripts/pkg/`: `name.sh`
computes artifact naming, `worktree.sh` manages build worktrees,
`build.sh` compiles, `collect.sh` gathers the binaries and every
source-tree input into the output dir, and `bundle.sh` signs and
assembles from the output dir alone (`sign.sh` is its signing
primitive). Because bundling is source-free, a build worktree only
has to span compile + collect.

- A clean checkout compiles inside a `git worktree` under `scratch/`
  — of the `v<version>` tag when it exists, else of HEAD — so
  uncommitted changes and a branch switch mid-build can't reach the
  artifacts. The worktree build prefers the tag's own `scripts/pkg/`
  helpers.
- Unanchored builds mangle their names ahead of `-unsigned`:
  `-<hash>` when HEAD isn't the tag, `-dirty` on a dirty tree. A
  canonical name always means tag content.
- Auto-detected signing only happens at the release tag. Elsewhere
  the build stays unsigned unless `--sign` is explicit, and `--sign`
  on an unanchored checkout refuses unless `--allow-dirty` is given
  (a test build).
- `--no-worktree` builds in place with the tag + clean requirement
  still enforced — CI uses it, since the runner's checkout is already
  the verified tag.
- Every run re-checks HEAD, version, and tree status before bundling,
  so a checkout that moved mid-build fails before anything is signed.

[install-layout]: install-layout.md
