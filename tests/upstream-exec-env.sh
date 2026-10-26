#!/bin/sh
# Upstream check (docs/design/container-internals.md, re-verify step 6).
# Reproduce: `container exec -e NAME=VALUE` does not override a variable
# the container already defines. The exec process's environment holds
# both entries, container value first, so getenv() returns the
# container's value. A shell hides it (it keeps the last duplicate).
#
#   tests/upstream-exec-env.sh   # exits 1 while the bug is present, 0 once fixed
#
# Same symptom on `run` was apple/container#1212, fixed by #1218 inside
# Parser.allEnv; `exec` appends to the container's environment outside
# that de-duplication. The plugin's `serve --token-env` works around it.
set -u

NAME=exec-env-repro-$$
cleanup() { container delete -f "$NAME" >/dev/null 2>&1; }
trap cleanup EXIT

container run -d --name "$NAME" -e FOO=from-run alpine:3.22 sleep 300 >/dev/null || exit 2

echo "== raw environ of the exec'd process (no shell involved):"
container exec -e FOO=from-exec "$NAME" cat /proc/self/environ | tr '\0' '\n' | grep '^FOO='

echo "== what a non-shell program sees via getenv() (busybox printenv):"
seen=$(container exec -e FOO=from-exec "$NAME" printenv FOO)
echo "$seen"

echo "== the same through a shell (masks the bug):"
container exec -e FOO=from-exec "$NAME" sh -c 'echo "$FOO"'

echo "== bare -e NAME (inherit from the host) behaves the same:"
FOO=from-host container exec -e FOO "$NAME" printenv FOO

if [ "$seen" = from-exec ]; then
    echo "PASS: exec -e overrides the container's value"
    exit 0
fi
echo "FAIL: exec -e did not override (got '$seen', want 'from-exec')"
exit 1
