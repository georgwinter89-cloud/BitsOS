#!/bin/bash
# slow
# DP-2: a normal boot (not ktest) with MCP reached from the host at 1024x768 (a full-frame PNG stays far below
# the 4 MB answer): /bin/abi-fbdev draw started through MCP `run`; MCP `screen` PNG == the program's own pattern
# == QEMU screendump; after the kernel kills it (kill_after_ms) the console's picture is back, again
# screen == screendump. Needs MCP's screen to read display::with_screen (change request DP-2/1); without it the
# script says SKIP.
set -eu
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/dp-30-fbdev-mcp}"
mkdir -p "$RUN_DIR"
RUN_DIR="$(realpath "$RUN_DIR")"
LO="${BITSOS_PORT_LO:-26500}"
FWD=$((LO + 30))
TOKEN="dp2-scenario-token-7a2f"
LOG="$RUN_DIR/serial.log"
QMP="$RUN_DIR/qmp.sock"
NAME=dp-fbdev-mcp

if ! grep -q "display::with_screen" crates/mcp/src/tools/screen.rs; then
    echo "SKIP: MCP screen does not read display::with_screen yet (change request DP-2/1, plan/notes/dp-DP-2-mc-screen.patch)"
    exit 0
fi

cargo xtask image --name "$NAME" --config-extra "token=$TOKEN;mcp_port=8765;loglevel=warn;screen=1024x768;init=off"
rm -f "$LOG" "$QMP"
cargo xtask qemu --name "$NAME" --net virtio --hostfwd "$FWD:8765" --timeout 300 --serial-log "$LOG" \
    -- -qmp "unix:$QMP,server=on,wait=off" > "$RUN_DIR/qemu.out" 2>&1 &
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
python3 -I host/tests/dp/fbdev_mcp_check.py "$FWD" "$TOKEN" "$QMP" "$RUN_DIR"
if grep -aq "PANIC" "$LOG"; then echo "RED: the kernel panicked"; tail -20 "$LOG"; exit 1; fi
grep -a "fbdev screen\." "$LOG" | tail -3 || true
