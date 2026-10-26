# Per-triple server layout

- **Issue:** [#3]
- **Milestone:** `1.0`

`servers::Arch` becomes a `Platform` table (triple ↔ label ↔
aliases); installed servers move to `libexec/container-mount/<triple>/container-mount`
in the pkg, the servers bundle, and the client tarball;
`server-path`/`fetch-servers` take `--triple` and `server-path` gains
`--fetch` (ensure-then-print). No legacy flat-name fallback — nothing
has shipped. [install-layout]

[#3]: https://github.com/daphnediane/container-mount/issues/3
[install-layout]: ../../../design/install-layout.md
