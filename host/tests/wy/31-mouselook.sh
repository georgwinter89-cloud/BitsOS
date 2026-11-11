#!/usr/bin/env bash
# WY-20 on the host (headless back end, nothing on the screen): mouse look. wyclient speaks zwp_relative_pointer_v1 and
# zwp_pointer_constraints_v1 as SDL2 does; an X11 client of the host's Xwayland selects XI2 raw motion as SDL2's X11
# back end does. Proves: both globals; a lock only for the focused window under the pointer; relative motion only
# while locked (accelerated and raw, also past the screen's edge), the pointer stands still and is hidden on the fake
# hardware cursor and in the software pointer; Esc reaches the client and Alt+Tab still switches; focus loss and the
# window's end and the desktop's own dialog release, a persistent lock comes back, a oneshot does not; the cursor hint;
# a confinement and its region; under Xwayland the raw motion arrives and a pointer grab and a hidden cursor's warp reach the constraints.
# Runs in a user, network and mount namespace with a private /tmp, so the display bitscomp claims is never the host's
# (as 16-xwayland.sh). No foot, no Mesa. Timing is not judged.
set -euo pipefail

ROOT=${BITSOS_ROOT:-$(cd "$(dirname "$0")/../../.." && pwd)}
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-$ROOT/target}
RUN=${BITSOS_RUN_DIR:-$CARGO_TARGET_DIR/xtask/runs/wy31}
mkdir -p "$RUN"
RUN=$(realpath "$RUN")
case $RUN in /tmp|/tmp/*) echo "RED: the run directory $RUN lies under /tmp (hidden in the namespace)"; exit 1 ;; esac
XW=${XWAYLAND:-/usr/bin/Xwayland}
command -v "$XW" >/dev/null || { echo "RED: no $XW on this host"; exit 1; }
unshare -Urnm true 2>/dev/null || { echo "RED: no unprivileged user namespaces here (the test needs a private /tmp)"; exit 1; }

echo "== build bitscomp and wyclient"
(cd "$ROOT/user/services/bitscomp" && cargo build --release -q --offline && cargo build --release -q --offline -p wyclient)

BIN=$CARGO_TARGET_DIR/release/bitscomp
exec unshare -Urnm /bin/bash -c '
    mount -t tmpfs tmpfs /tmp || { echo "RED: no private /tmp"; exit 1; }
    exec python3 -I "$0" "$@"
' "$ROOT/host/tests/wy/lock_host.py" "$BIN" "$CARGO_TARGET_DIR/release/wyclient" "$XW" "$RUN" "$ROOT/host/tests/wy/tools/x11raw.py"
