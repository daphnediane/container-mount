# Release process

Releases are git tags; the tag-push workflow attaches the built
artifacts — the macOS installer .pkg, the `cargo-binstall` client
tarball, the guest-server bundle, the CycloneDX SBOM, and
`THIRD-PARTY-NOTICES.yaml` ([packaging]). Same conventions as
container-distro's [release
process](https://github.com/daphnediane/container-distro/blob/main/docs/design/release.md).

## Versioning

- One version for the package, in the root `Cargo.toml`
  (`[package].version`); the `distrofs` binary and library share it.
- [SemVer]: `MAJOR.MINOR.PATCH`. Pre-1.0, minor bumps (`0.x.0`) may
  break CLI and wire-protocol behavior; patch bumps (`0.x.y`) are
  fixes only. The wire protocol has its own version (`proto::VERSION`,
  in the handshake and the embedded stamp) — bumping it is a protocol
  change, not just a release.
- Tags are `v<version>` (`v0.1.0`), annotated. Maintenance branches
  are `release/<major>.<minor>` (`release/0.1`).
- **Only tagged commits carry a clean `X.Y.Z` version.** Every other
  commit carries a prerelease suffix: `main` sits at
  `<next-minor>.0-dev` and `release/X.Y` at `<next-patch>-dev`, so a
  dev build can't be mistaken for a release.

## Cutting a release (from `main`)

1. Run the gate: `scripts/release-gate.sh` — fmt, clippy on host *and*
   both musl targets, release musl builds, `cargo test`, shell syntax,
   `cargo audit`, `cargo deny`, `THIRD-PARTY-NOTICES.yaml` freshness
   (regenerate with `cargo bundle-licenses -f yaml -o
   THIRD-PARTY-NOTICES.yaml` if stale), and `tests/e2e.sh` when an OCI
   engine is up. Confirm [TODO] lists only still-open work and the
   docs reflect what shipped.
2. Update `CHANGELOG.md`: rename the running `## X.Y.Z-dev` section to
   `## X.Y.Z -- YYYY-MM-DD` and audit it against `git log --oneline
   v<prev>..HEAD`. Commit before releasing — `cargo release` needs a
   clean tree.
3. Preview then cut:

   ```bash
   cargo release release                  # dry run: 0.1.0-dev -> 0.1.0
   cargo release release -x --no-confirm  # commits "chore: release X.Y.Z",
                                          # tags vX.Y.Z
   ```

   (`[package.metadata.release]` config: `publish = false`, `push =
   false`, `allow-branch = ["main", "release/*"]` — publish and push
   stay manual.)
4. Create the maintenance branch on the tag: `git branch release/X.Y
   vX.Y.Z`.
5. Push: `git push origin main release/X.Y vX.Y.Z`.
6. The tag push runs `.github/workflows/release.yml`: verify tag ==
   package version, re-run tests, `scripts/build-pkg.sh`, draft GitHub
   Release with the changelog section as notes and the `-unsigned`
   artifacts attached. CI has no signing keychain — rebuild signed and
   replace before publishing:

   ```bash
   scripts/build-pkg-signed.sh   # signs + notarizes/staples
   gh release upload vX.Y.Z scratch/pkg/distrofs-X.Y.Z.pkg \
       scratch/pkg/distrofs-aarch64-apple-darwin-vX.Y.Z.tar.gz \
       --clobber
   ```

   Review the draft and publish (`gh release edit vX.Y.Z
   --draft=false`).
7. crates.io publish is deferred — see [TODO]; until decided, install
   paths are the pkg, binstall tarball, `cargo install --path`, and
   `distrofs update`/`fetch-servers`.
8. Close the issues the release resolves, naming `vX.Y.Z` in each
   comment.
9. Re-open development on `main`:

   ```bash
   cargo release version X.Y.0-dev -x --no-confirm
   git commit -am "chore: start X.Y development"
   git push
   ```

   The same commit starts a fresh `## X.Y.0-dev` CHANGELOG section.

## Patch releases (on `release/X.Y`)

Same as container-distro: if the branch head is still the last tag,
open the patch dev cycle first (`cargo release version X.Y.Z-dev -x
--no-confirm`, commit `chore: start X.Y.Z development`); cherry-pick
fixes with `-x`; draft the changelog section from the branch commits;
`cargo release release -x --no-confirm`; push branch + tag; publish
the draft; re-open the dev cycle.

## Changelog

`CHANGELOG.md` records user-facing changes per release, newest first —
`## X.Y.Z -- YYYY-MM-DD` sections grouped Added / Changed / Fixed. The
top section is the running draft (`## X.Y.Z-dev`) fed as work lands;
at release time it's renamed and audited, so the changelog stays the
permanent record and TODO stays a list of work that remains.

[packaging]: packaging.md
[SemVer]: https://semver.org
[TODO]: ../TODO.md
