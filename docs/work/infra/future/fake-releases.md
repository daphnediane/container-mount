# `make-fake-releases` equivalent

- **Issue:** [#18]
- **Milestone:** none (future)

The e2e fixture for `fetch-servers`/`update` is hand-rolled today
(`DISTROFS_RELEASES_URL` behind `--dev`); a script like
container-distro's would make the verified-download path part of the
regular test loop.

[#18]: https://github.com/daphnediane/distrofs/issues/18
