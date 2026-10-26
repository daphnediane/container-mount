# Plugin → server token hand-off

- **Issue:** [#19]
- **Milestone:** Decide before tagging 0.1.0

How the plugin passes the session token to the server it starts: the
per-command env name working around the upstream `container exec -e`
override bug, a token file shipped with the binary, stdin, or a
version-gated fixed name. [container-internals] has the upstream
details; `tests/upstream-exec-env.sh` reproduces the bug.

[#19]: https://github.com/daphnediane/container-mount/issues/19
[container-internals]: ../../../design/container-internals.md
