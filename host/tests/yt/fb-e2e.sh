#!/usr/bin/env bash
# slow
# Run: `cargo xtask fullrun`, on the build host (a program in a container, no QEMU, no Mini-PC; silent); by hand
# `cargo xtask scenario yt/fb-e2e`. Needs lane YT's container cobalt-yt-v2 and the Cobalt build it mounts: without them RED.
# End-to-end test of the framebuffer backend without the network (YT-1).
# Run on the host; it uses the lane container cobalt-yt-v2 (podman) and the
# built loader_app in the Cobalt copy. A local page (e2e/keys.html, served on
# 127.0.0.1:29080) records keys, mouse and wheel; keys go in through the fake
# evdev FIFO, the picture is read from the fake framebuffer file.
# Checks: picture (page colour, scaled 4/3 to 2560x1440), the six TV keys,
# QWERTZ letters, gamepad -> remote keys, pointer position mapping (screen ->
# window), click, wheel, pointer drawn, Ctrl+Alt+Backspace ends Cobalt.
set -uo pipefail
here=$(cd "$(dirname "$0")" && pwd)
C=cobalt-yt-v2
state=$(podman container inspect -f '{{.State.Running}}' "$C" 2>/dev/null) \
  || { echo "RED: no container $C (lane YT: user/ports/cobalt-launcher/README.md)"; exit 1; }
started=""
if [ "$state" != true ]; then
    podman start "$C" >/dev/null || { echo "RED: podman start $C failed"; exit 1; }
    started=1
fi
# Only what this script started goes down again.
trap '[ -z "$started" ] || podman stop -t 5 "$C" >/dev/null 2>&1' EXIT
OUT=/work/cobalt/src/out/evergreen-x64-bitsos_qa
podman exec "$C" test -x "$OUT/loader_app" || { echo "RED: no Cobalt build in the container ($OUT/loader_app)"; exit 1; }
# This worktree's inner script; bash -c, because Cobalt's helpers would read a script on stdin.
podman exec "$C" bash -c "$(cat "$here/fb-e2e-inner.sh")"
