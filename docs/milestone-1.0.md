# 1.0 finalization checklist

What remains before tagging `v1.0.0`. The policy work is done:
`SECURITY.md`, `CODE_OF_CONDUCT.md`, and `CONTRIBUTING.md` are
de-drafted and concrete, matching container-distro's. The standard
release steps live in [design/release.md](design/release.md) and
aren't repeated here — only the 1.0-specific deltas are.

## Before the first release

- [x] **Settle the names.** Everything — repo, crate, binary, wire
  protocol, server binaries, artifacts, identifiers — is
  `container-mount` ([history](design/history.md#rename-to-container-mount)).

## Before tagging 1.0

- [ ] **Close the `1.0` milestone** — the items under
  [docs/work/](work/README.md#10-milestone) (`gh issue list
  --milestone 1.0`; #2 and #3 done 2026-10-26), including the
  crates.io decision (#1) and the normative wire contract (#6).
- [x] **GPG signatures, or not** — matched container-distro: signed
  release tarballs carry a `gpg --detach-sign` `.asc` next to the
  code-signature `.sig` (`GPG_KEY_ID` in `signing.local.conf`).
- [ ] **Final review of [design/security.md](design/security.md)** —
  one pass over the trade-offs and the client/server enforcement
  lists against what actually shipped, including anything the
  `container mount` plugin ([design/mount-backends.md](design/mount-backends.md))
  has added by then.
- [ ] **Set the 1.0 version** — `cargo release release` tags whatever
  the dev version points at. When the next release is decided to be
  1.0, run `cargo release version 1.0.0-dev -x --no-confirm` first so
  the tag lands as `v1.0.0`.
- [ ] **Release per [design/release.md](design/release.md)** — the
  CHANGELOG rename to `## 1.0.0 -- YYYY-MM-DD`, tag + `release/1.0`
  branch, signed artifacts, publish, close shipped issues.
