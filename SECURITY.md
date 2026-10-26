# Security Policy

container-mount (`distrofs`) is a single-maintainer project; reports
are handled on a best-effort basis.

## Supported Versions

Fixes land on `main` and may be backported to the latest release line
when the next release isn't out yet; older releases are not
maintained.

| Version        | Supported |
| -------------- | --------- |
| `main`         | yes       |
| latest release | yes       |
| older releases | no        |

## Reporting a Vulnerability

**Please do not report suspected vulnerabilities through public GitHub
issues or pull requests.**

Report privately via GitHub's private vulnerability reporting: open
the repository's **Security** tab and choose **Report a
vulnerability**.

If that isn't available to you, or you get no response within
7 days, open a public issue that says only "I have a security
report to share privately" — no details — and the maintainer will
reach out.

Including as much of the following as you can helps triage:

- `distrofs` version (`distrofs --version`)
- the transport (unix socket / fd / TCP) and what launched the server
- steps to reproduce, or a proof of concept
- the impact — what an attacker gains that they shouldn't have

Response expectations: this is a hobby project with no guaranteed
timelines — reports are triaged as maintainer time allows.

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

Suspected vulnerabilities in upstream projects — Apple `container`
and `containerization`, other OCI engines, Rust crates — belong to
those projects; report them per the upstream project's own policy. If
it isn't clear where the fault lies, the private channel above is
fine.

## Disclosure

The intent is to ship a fix before any public advisory — a quiet fix
release usually resolves the report. Reporters are credited in the
advisory and release notes unless they ask not to be; there is no
bounty program.

## Attribution

Adapted from GitHub's default `SECURITY.md` template, with scope and
disclosure sections informed by the OpenSSF
[oss-vulnerability-guide](https://github.com/ossf/oss-vulnerability-guide)
(CC BY 3.0).
