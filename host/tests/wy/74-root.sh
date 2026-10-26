#!/bin/bash
# slow
# WY-7: bitscomp started from the console shell (init's, root), as Georg starts it, runs as uid 1000 once its
# devices are open: its ids and groups, sockets in /run/user/1000 owned by 1000 and reachable by `bitscomp ctl` as
# MCP's uid 1000, the compositor grant kept across the drop, foot and its shell as 1000, a keyboard plugged in after
# the drop (QMP device_add on a second qemu-xhci) opened and grabbed, `ctl quit` as 1000, the console back.
set -euo pipefail
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/wy-74-root}"
mkdir -p "$RUN_DIR"
RUN_DIR="$(realpath "$RUN_DIR")"
LO="${BITSOS_PORT_LO:-30500}"
FWD=$((LO + 74))
TOKEN="root-scenario-token-7a40"
LOG="$RUN_DIR/serial.log"
QMP="$RUN_DIR/qmp.sock"
NAME=wy-root
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-$PWD/target}

cargo xtask image --name "$NAME" --config-extra "token=$TOKEN;mcp_port=8765;loglevel=info;screen=1280x800"
rm -f "$LOG" "$QMP"
cargo xtask qemu --name "$NAME" --net virtio --hostfwd "$FWD:8765" --timeout 900 --serial-log "$LOG" \
    -- -smp 4 -device qemu-xhci,id=xb -qmp "unix:$QMP,server=on,wait=off" > "$RUN_DIR/qemu.out" 2>&1 &
QPID=$!
stop() {
    kill "$QPID" 2>/dev/null || true
    pf="target/xtask/qemu-$NAME.pid"
    if [ -f "$pf" ]; then
        pid="$(head -1 "$pf")"
        if tr '\0' ' ' < "/proc/$pid/cmdline" 2>/dev/null | grep -Eq -- "-name v2-[A-Za-z0-9]+-$NAME( |$)"; then kill "$pid" 2>/dev/null || true; fi
    fi
    wait "$QPID" 2>/dev/null || true
}
trap stop EXIT
python3 -I host/tests/wy/v2_root.py "$FWD" "$TOKEN" "$QMP" "$RUN_DIR"
if grep -aq "PANIC" "$LOG"; then echo "RED: the kernel panicked"; tail -20 "$LOG"; exit 1; fi
if grep -aq "compositor.refused" "$LOG"; then echo "RED: the kernel refused a compositor ioctl"; grep -a "compositor\." "$LOG" | tail -5; exit 1; fi
grep -aq "compositor.granted" "$LOG" || { echo "RED: no compositor.granted event"; exit 1; }
if grep -aq "$TOKEN" "$LOG"; then echo "RED: the token appears in the log (R8i)"; exit 1; fi
echo "GREEN: bitscomp from the root console runs as uid 1000 with its devices, grant and sockets"
