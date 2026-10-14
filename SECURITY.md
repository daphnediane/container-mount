# Security Policy

> **Draft.** `distrofs` is pre-1.0 with a single maintainer; this
> policy is adopted ahead of need. Anything marked `[TODO]` is a
> placeholder for response expectations and disclosure details.

## Supported Versions

The project is pre-1.0: only the latest release receives fixes, and
there are no backports to older versions.

| Version        | Supported |
| -------------- | --------- |
| latest release | yes       |
| older releases | no        |

## Reporting a Vulnerability

**Please do not report suspected vulnerabilities through public GitHub
issues or pull requests.**

Report privately via GitHub's private vulnerability reporting: open
the repository's **Security** tab and choose **Report a
vulnerability**.

If that isn't available to you, or you get no response within
[TODO: N] days, open a public issue that says only "I have a security
report to share privately" — no details — and the maintainer will
reach out.

Including as much of the following as you can helps triage:

- `distrofs` version (`distrofs --version`)
- the transport (unix socket / fd / TCP) and what launched the server
- steps to reproduce, or a proof of concept
- the impact — what an attacker gains that they shouldn't have

Response expectations: **[TODO: acknowledgement / fix timelines —
currently best-effort]**.

## Scope

The server deliberately runs as guest root and serves the whole guest
rootfs — that is the feature (a `\\wsl$` equivalent), not a hole:
anyone who can launch it already has equivalent access by other means.
What *is* in scope: anything that lets a party reach a session without
the token, that lets a guest-user process use a running server, that
escapes symlink confinement onto host paths, or that weakens the
peer/leaf/socket-directory checks. Read
[docs/design/security.md](docs/design/security.md) before reporting —
several trade-offs (TCP listeners are token-only, a host client
inherently trusts whoever spawned it) are documented there.

## Disclosure

[TODO: coordinated-disclosure policy — e.g. fix released before any
public advisory, expected embargo window, whether reporters are
credited]

## Attribution

Adapted from GitHub's default `SECURITY.md` template, with scope and
disclosure sections informed by the OpenSSF
[oss-vulnerability-guide](https://github.com/ossf/oss-vulnerability-guide)
(CC BY 3.0).
