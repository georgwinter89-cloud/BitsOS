#!/usr/bin/env bash
# End-to-end test of the framebuffer backend without the network (YT-1).
# Run on the host; it uses the lane container cobalt-yt-v2 (podman) and the
# built loader_app in the Cobalt copy. A local page (e2e/keys.html, served on
# 127.0.0.1:29080) records keys, mouse and wheel; keys go in through the fake
# evdev FIFO, the picture is read from the fake framebuffer file.
# Checks: picture (page colour, scaled 4/3 to 2560x1440), the six TV keys,
# QWERTZ letters, gamepad -> remote keys, pointer position mapping (screen ->
# window), click, wheel, pointer drawn, Ctrl+Alt+Backspace ends Cobalt.
set -uo pipefail
if [ "$(podman container inspect -f '{{.State.Running}}' cobalt-yt-v2 2>/dev/null)" != "true" ]; then
    echo "SKIP: the lane container cobalt-yt-v2 is not running (podman start cobalt-yt-v2)"
    exit 0
fi
podman exec cobalt-yt-v2 bash /lane/host/tests/yt/fb-e2e-inner.sh
