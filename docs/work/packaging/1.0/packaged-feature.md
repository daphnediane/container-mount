# `packaged` cargo feature + per-user servers dir

- **Issue:** [#4]
- **Milestone:** `1.0`

Release builds compile `--features packaged`: bundled servers report
already-fetched, non-bundled triples and unwritable prefixes fetch to
`~/Library/Application Support/container-mount/servers/` (macOS) /
`$XDG_DATA_HOME/container-mount/servers`. `fetch-servers` drops `--version`
(lock-step own-version bundles). Uninstall gains a keep/delete prompt
— container-mount's first per-user state. [install-layout]

[#4]: https://github.com/daphnediane/container-mount/issues/4
[install-layout]: ../../../design/install-layout.md
