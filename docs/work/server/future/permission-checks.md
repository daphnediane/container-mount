# Opt-in permission checks as the session user

- **Issue:** [#25]
- **Milestone:** none (future)

Today `USER@`/`--user` pick a home and an owner while the server acts as
guest root. An opt-in server mode would check access as that user
(`setfsuid`/`setfsgid` and groups per operation) without losing
root-only duties. [security]

[#25]: https://github.com/daphnediane/container-mount/issues/25
[security]: ../../../design/security.md
