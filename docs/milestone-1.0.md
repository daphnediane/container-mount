# 1.0 finalization checklist

What remains before tagging `v1.0.0`. The policy work is done:
`SECURITY.md`, `CODE_OF_CONDUCT.md`, and `CONTRIBUTING.md` are
de-drafted and concrete, matching container-distro's. The standard
release steps live in [design/release.md](design/release.md) and
aren't repeated here — only the 1.0-specific deltas are.

## Before the first release

- [ ] **Settle the names.** The repo is `container-mount`; the crate,
  binary, wire protocol (handshake magic, `DISTROFS_TOKEN`, version
  stamps), server binaries, release artifacts, and signing identifier
  still say `distrofs` ([history](design/history.md#rename-to-container-mount)).
  Nothing depends on them yet, so renaming is free until something
  ships — decide before `v0.1.0`.

## Before tagging 1.0

- [ ] **Close the `1.0` milestone** — the six items under
  [docs/work/](work/README.md#10-milestone) (`gh issue list
  --milestone 1.0`; #1–#6 open as of 2026-10-25), including the
  crates.io decision (#1) and the normative wire contract (#6).
- [ ] **GPG signatures, or not** — container-distro's release tarball
  carries a `gpg --detach-sign` `.asc`; container-mount's signing
  config has no GPG key, so its tarballs get the code signature only.
  Match container-distro or document the difference.
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
