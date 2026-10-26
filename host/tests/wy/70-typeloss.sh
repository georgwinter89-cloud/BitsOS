#!/bin/bash
# slow
# desk fix typeloss: typed input through the whole path, MCP send_keys -> kernel evdev -> bitscomp (grabs the
# devices, reads evdev) -> wl_keyboard -> foot -> pty -> BusyBox ash, at injection speed. A line of 54 characters
# per send_keys call as on the Mini-PC (`echo abc...789 > /tmp/tl<n>`), TL_LINES times (default 50) into one foot,
# then the same right after a second foot opened, then TL_BURST lines in back-to-back calls without waiting
# (2000+ characters in all). Zero loss is green; the kernel's evdev overflow counters, bitscomp's own counts
# (SYN_DROPPED read, key presses read and forwarded) and MCP's dropped report are printed and checked too.
set -euo pipefail
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/wy-70-typeloss}"
mkdir -p "$RUN_DIR"
RUN_DIR="$(realpath "$RUN_DIR")"
LO="${BITSOS_PORT_LO:-30500}"
FWD=$((LO + 70))
TOKEN="typeloss-scenario-token-91c3"
LOG="$RUN_DIR/serial.log"
QMP="$RUN_DIR/qmp.sock"
NAME=wy-typeloss
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-$PWD/target}

cargo xtask image --name "$NAME" --config-extra "token=$TOKEN;mcp_port=8765;loglevel=${WY_LOGLEVEL:-warn};screen=1280x800"
rm -f "$LOG" "$QMP"
cargo xtask qemu --name "$NAME" --net virtio --hostfwd "$FWD:8765" --timeout 900 --serial-log "$LOG" \
    -- -smp 4 -qmp "unix:$QMP,server=on,wait=off" > "$RUN_DIR/qemu.out" 2>&1 &
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
python3 -I host/tests/wy/v2_typeloss.py "$FWD" "$TOKEN" "$QMP" "$RUN_DIR"
if grep -aq "PANIC" "$LOG"; then echo "RED: the kernel panicked"; tail -20 "$LOG"; exit 1; fi
echo "GREEN: no typed input lost through evdev, bitscomp, foot and the pty"
