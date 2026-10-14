---
name: release
description: Cut a release — gate, changelog adoption, tag, publish, close issues
---

# Release

Checklist for a minor release from `main`.

1. Confirm `Cargo.toml` is at `X.Y.Z-dev` and the tree is on `main`
   (for a patch release, `release/X.Y`).
2. Run `scripts/release-gate.sh` — fmt, host + both-musl clippy,
   musl builds, tests, shell syntax, `cargo audit`, `cargo deny`, and
   `tests/e2e.sh` when an OCI engine is present.
3. In `CHANGELOG.md` rename `## X.Y.Z-dev` to `## X.Y.Z -- YYYY-MM-DD`,
   then audit `git log --oneline v<prev>..HEAD` — add entries for
   anything that landed without one.
4. Commit per `.devin/rules/comment-file.md` (cargo release needs a
   clean tree), then:

   ```bash
   cargo release release                  # dry run
   cargo release release -x --no-confirm  # strips -dev, commits, tags vX.Y.Z
   git push origin main vX.Y.Z
   ```

5. The tag push runs `.github/workflows/release.yml`, which drafts a
   GitHub Release carrying `distrofs-servers-<ver>.tar.gz` and
   `distrofs-aarch64-apple-darwin-v<ver>-unsigned.tar.gz` — the asset
   names `fetch-servers`/`update` look for. Review and publish the
   draft; when Developer ID signing is configured, sign the binaries
   locally and re-upload under the same names minus `-unsigned`.
6. crates.io publish stays manual until the crate is meant to be
   published there.
7. Open the next cycle: `cargo release version X.Y.0-dev -x
   --no-confirm`, start a fresh empty `## X.Y.0-dev` section atop
   `CHANGELOG.md`, commit, push.
8. Sanity-check: `git checkout vX.Y.Z && cargo test`, `distrofs
   --version` prints `X.Y.Z`, back to `main`.
