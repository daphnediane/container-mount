# Contributing

container-mount (the host/guest file bridge and, in design, the
`container mount` plugin) is a personal project with a single
maintainer. Issues and pull requests are welcome.

## Code of conduct

Participation is covered by the [Code of Conduct](CODE_OF_CONDUCT.md)
(adapted from Contributor Covenant 3.0). Short version: be kind,
assume good faith, don't be a jerk.

## Ways to help

- **Bug reports** — open an [issue] with the `container-mount` version
  (`container-mount --version`), the transport involved (unix socket / fd /
  TCP) and which engine relayed it, plus the command you ran and what
  happened vs. what you expected.
- **Feature ideas** — check the [open issues][issue] first; each
  planned item has an issue linking its doc under docs/work/, so a
  fair amount of the roadmap is already tracked. For anything bigger
  than a small fix, open an issue to discuss before writing code.
- **Pull requests** — small, focused PRs are far easier to review than
  sweeping ones. PRs should reference the issue they implement; only
  trivial fixes (typos, obvious bugs) should come in untracked.

## Development setup

Building, testing, and the e2e harness are covered in
[DEV.md](DEV.md). Short version: stable Rust plus the two musl
targets, then `cargo build --workspace`; `tests/e2e.sh` needs an OCI
engine and skips without one.

## Before opening a PR

CI (`.github/workflows/ci.yml`) runs these checks on push and PR —
run them locally first:

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo clippy --target aarch64-unknown-linux-musl --all-targets -- -D warnings
cargo clippy --target x86_64-unknown-linux-musl --all-targets -- -D warnings
cargo test --workspace
```

If you add or bump a dependency it must be pure Rust (the musl server
links without a C toolchain) and pass the supply-chain gate
(`cargo deny check` — permissive licenses only; the policy lives in
deny.toml).

## Commit and PR conventions

- Commit subjects use conventional-commit tags (`feat`, `fix`, `docs`,
  `refactor`, `test`, `chore`, ...), imperative mood, ~50 characters.
- Describe the why, not just the what.
- AI-assisted contributions are welcome — most of this codebase was
  AI-generated and human-reviewed. If an AI agent meaningfully
  authored the change, credit it with a `Co-Authored-By:` trailer in
  the commit message. Either way, only submit code you have actually
  read and understood.

## Security issues

Read [docs/design/security.md](docs/design/security.md) before
reporting what looks like a hole; several apparent ones are
documented, deliberate trade-offs.

For genuine vulnerabilities, report privately per the
[security policy](SECURITY.md) rather than opening a public issue.

## License

Contributions are licensed under the project's
[BSD-2-Clause License](LICENSE). By submitting a pull request you
agree your contribution is provided under those terms.

[issue]: https://github.com/daphnediane/container-mount/issues
