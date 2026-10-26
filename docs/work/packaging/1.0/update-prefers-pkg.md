# `update` prefers the signed pkg on pkg installs

- **Issue:** [#2]
- **Milestone:** `1.0`

Today `update` swaps the binary in place from the tarball, which fails
(correctly) on a root-owned `/usr/local` install. Mirror
container-distro's update: detect a pkg receipt, download the signed
pkg, hand off to Installer.app.

[#2]: https://github.com/daphnediane/container-mount/issues/2
