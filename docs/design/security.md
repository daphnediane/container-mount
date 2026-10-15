# distrofs — threat model

What distrofs protects, what it deliberately doesn't, and the
trust-boundary rules for anyone changing code on either side.

## The shape

A `distrofs serve` process runs as **root inside the guest** and
serves the guest's whole root filesystem over a socket. A host
`distrofs` client connects, proves a shared 32-byte token (and makes
the server prove it back), then issues filesystem operations. The
embedder (e.g. container-distro) supplies the socket, launches the
server, generates the token, and passes the client its arguments.

## Deliberate trade-offs (not bugs)

- **The server is a root daemon for its guest.** Serving `/etc/shadow`
  to the host user is the point — the same reach as `exec`/`cp` as
  root, which whoever can launch the server already has.
- **The client trusts its launcher.** `DISTROFS_TOKEN`, `--socket`,
  and `--peer-requirement` come from whoever spawned it — a hostile
  launcher gains nothing it didn't already have (it launched us).
- **The guest server trusts its launcher.** Whoever can `exec` inside
  the guest as root controls it anyway.
- **TCP listeners are token-only.** `--listen tcp:`/`--tcp` exist for
  engines that publish ports, not sockets; there is no peer-identity
  check on TCP, so the token is the only gate. Bind loopback and
  prefer the unix transport wherever the engine supports it.
- **Symlink confinement is a display/export guarantee, not a jail.**
  It keeps a *mount* or `get` from silently landing on a host path; a
  user with guest access can always create links — the client just
  refuses to follow them out of the guest root.

## What the client enforces

Every check lives client-side so an embedder can't accidentally skip
it:

1. **Socket placement** — for managed paths, the directory chain is
   opened `O_DIRECTORY|O_NOFOLLOW` and must be private to the user (or
   a sticky shared dir for the temp root); the leaf must be a socket
   owned by the user or root.
2. **Peer identity (macOS, when `--peer-requirement` is given)** —
   after `connect(2)`, the peer's audit token must satisfy the
   code-signing requirement. This is what makes the check-then-connect
   race irrelevant: a swapped path or foreign listener fails *after*
   connect, on the live process, with no TOCTOU.
3. **Mutual handshake** — fresh nonces each way; `mac_c` is compared
   in constant time. A guest-local process that can reach the socket
   still can't use it without the token.
4. **Server output is untrusted** — frames are capped (16 MiB) before
   allocation; names must be single components (no `/`, NUL,
   `.`/`..`); symlink targets are rewritten to stay inside the guest
   root before anything host-side resolves them; in-flight requests
   are bounded and calls time out.

## What the server enforces

- The socket sits in a `0700` root-owned directory so guest users
  can't reach it.
- `DISTROFS_TOKEN` is read from the environment and cleared before
  serving; it never appears on a command line.
- All filesystem access is `*at()`-relative to `O_PATH|O_NOFOLLOW`
  fds — no host path is ever opened by name.
- Idle exit: no client within 30 s of start, or 30 s after the last
  client.
- virtiofs/pseudo filesystems (`proc`, `sysfs`, `devtmpfs`,
  `virtiofs`, …) present as empty read-only directories — the server
  must not serve host shares back to the host. Hidden mountpoints
  likewise refuse writes (`EROFS`) and present no xattrs.
- Optional ops (`Copy`/`CopyRange`/`Seek`/`Fallocate`/xattrs) act only
  through `O_PATH` node fds and per-connection file handles — same
  confinement as the base ops, no paths. `Copy` creates exclusively
  (`EEXIST`); a partial copy is unlinked before the error returns.
- xattrs pass all namespaces through, including `trusted.*` and
  `security.*` — they're file content, and the server is guest root
  anyway. The host treats *values* as guest-controlled, like file
  data; mapping guest names onto host-reserved `com.apple.*` attrs is
  a host-side policy decision, not a passthrough.

## Release/download integrity

- `fetch-servers`/`update` verify the GitHub-API-reported SHA-256
  digest of each asset before installing; a mismatched or undigested
  download is refused (or loudly warned, never silent).
- `update` on macOS additionally checks `identifier
  "io.github.daphnediane.distrofs" and anchor apple generic` on the
  download, and refuses a team-ID mismatch with the installed copy
  without explicit consent.
- `update` never self-elevates, refuses to run as root, and refuses
  to replace the copy installed under a container-plugins dir — that
  copy belongs to the embedding package.
- `fetch-servers` installs the bundle matching the binary's own
  version — client and server are lock-step. Fetched servers get the
  same digest + stamp verification wherever they land: the prefix
  `libexec` dir or, when it isn't writable (packaged installs,
  non-root users), the per-user servers dir — a user-writable
  location is acceptable there because only the user's own sessions
  consume it. An explicit server-path override, if added, is a
  `--dev` seam only.
- `DISTROFS_RELEASES_URL` is a **dev-only** seam honored only behind
  `--dev` on that invocation — an env var alone can never redirect
  release downloads.

## Rules for changes on this boundary

- New transports get the same defense-in-depth treatment (leaf/dir
  hygiene first, identity proof after connect, handshake last).
- Any seam that could redirect a download, disable a check, or weaken
  confinement must hang off `--dev`, never a bare environment
  variable.
- Don't oversell: doc language should say "confined," "read-only,"
  and "checked," not "sandboxed," "hardened," or "isolated" — the
  guest root is the boundary by design.
