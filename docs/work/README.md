# Work items

The working roadmap — open work only, one file per item at
`<area>/<milestone>/<feature>.md`:

- `<milestone>` mirrors the GitHub milestone on the item's issue
  (`1.0`). `future/` holds items with no milestone (post-1.0 or
  undated).
- Every item links its GitHub issue; the issue and the design docs it
  cites are the source of truth for details.
- When an item is completed its file leaves this tree: the
  user-facing change is recorded in the `X.Y.Z-dev` section of
  [CHANGELOG.md](../../CHANGELOG.md), which is the permanent record of
  what shipped.

## Index

### `1.0` milestone

The non-issue 1.0 gates (naming, GPG, the final security review) are
in [milestone-1.0.md](../milestone-1.0.md).

- [packaging/1.0/crates-io-publishing.md](packaging/1.0/crates-io-publishing.md)
  — decide crates.io publishing ([#1])
- [packaging/1.0/packaged-feature.md](packaging/1.0/packaged-feature.md)
  — `packaged` cargo feature + per-user servers dir ([#4])
- [cli/1.0/clap-man-pages.md](cli/1.0/clap-man-pages.md) — `clap`
  cargo feature + man pages ([#5])
- [protocol/1.0/wire-contract.md](protocol/1.0/wire-contract.md) —
  normative protocol contract doc ([#6])

### Before 0.1.0 (no milestone)

- plugin/ — [token-handoff](plugin/future/token-handoff.md) ([#19])

### `future` (no milestone)

- plugin/ — [prepare](plugin/future/prepare.md) ([#22]),
  [open-session](plugin/future/open-session.md) ([#23]),
  [file-verbs](plugin/future/file-verbs.md) ([#24]),
  [dev-apiserver-requirement](plugin/future/dev-apiserver-requirement.md)
  ([#26])
- fskit/ — [fskit-front-end](fskit/future/fskit-front-end.md) ([#7]),
  [inotify-invalidation](fskit/future/inotify-invalidation.md) ([#8])
- server/ — [later-capabilities](server/future/later-capabilities.md)
  ([#9]), [stdio-transport](server/future/stdio-transport.md) ([#11]),
  [serve-root](server/future/serve-root.md) ([#12]),
  [serve-ports](server/future/serve-ports.md) ([#13]),
  [permission-checks](server/future/permission-checks.md) ([#25])
- mounts/ — [distro-to-distro](mounts/future/distro-to-distro.md)
  ([#10]), [attach-detach](mounts/future/attach-detach.md) ([#20]),
  [stopped-targets](mounts/future/stopped-targets.md) ([#21])
- protocol/ — [protocol-ranges](protocol/future/protocol-ranges.md)
  ([#14])
- packaging/ —
  [embedder-pinned-pair](packaging/future/embedder-pinned-pair.md)
  ([#15]), [x86-64-universal](packaging/future/x86-64-universal.md)
  ([#16])
- infra/ — [ci-musl](infra/future/ci-musl.md) ([#17]),
  [fake-releases](infra/future/fake-releases.md) ([#18])

[#1]: https://github.com/daphnediane/container-mount/issues/1
[#4]: https://github.com/daphnediane/container-mount/issues/4
[#5]: https://github.com/daphnediane/container-mount/issues/5
[#6]: https://github.com/daphnediane/container-mount/issues/6
[#7]: https://github.com/daphnediane/container-mount/issues/7
[#8]: https://github.com/daphnediane/container-mount/issues/8
[#9]: https://github.com/daphnediane/container-mount/issues/9
[#10]: https://github.com/daphnediane/container-mount/issues/10
[#11]: https://github.com/daphnediane/container-mount/issues/11
[#12]: https://github.com/daphnediane/container-mount/issues/12
[#13]: https://github.com/daphnediane/container-mount/issues/13
[#14]: https://github.com/daphnediane/container-mount/issues/14
[#15]: https://github.com/daphnediane/container-mount/issues/15
[#16]: https://github.com/daphnediane/container-mount/issues/16
[#17]: https://github.com/daphnediane/container-mount/issues/17
[#18]: https://github.com/daphnediane/container-mount/issues/18
[#19]: https://github.com/daphnediane/container-mount/issues/19
[#20]: https://github.com/daphnediane/container-mount/issues/20
[#21]: https://github.com/daphnediane/container-mount/issues/21
[#22]: https://github.com/daphnediane/container-mount/issues/22
[#23]: https://github.com/daphnediane/container-mount/issues/23
[#24]: https://github.com/daphnediane/container-mount/issues/24
[#25]: https://github.com/daphnediane/container-mount/issues/25
[#26]: https://github.com/daphnediane/container-mount/issues/26
