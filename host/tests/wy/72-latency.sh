#!/bin/bash
# slow
# WY-7: input-to-screen latency while terminals start (Georg 21.10.: avg 16 ms, max 526 ms during a burst of foot
# starts). bitscomp on fbdev, one foot, the pointer moved every 30 ms over MCP (each move one frame on the kernel's
# `program` latency path), then LAT_FOOTS (default 10) Strg+Alt+Enter back to back, then LAT_KEYS (default 5) by keys
# alone (the pointer still: the launch press must be answered by the "starting" mark). The `latency` figures of the
# quiet stretch and of the burst are printed; LAT_BUDGET_MS=50 makes the worst case a check (QEMU timing is only
# indicative; the Mini-PC check decides). Always checked, timing-free: starts go through the trampoline and a
# windowless start carries the mark until it ends (`ctl stats` launching=/spawn=). LAT_SCREEN=2560x1440 draws at the Mini-PC's size (default 1280x800).
# LAT_FB=--fb-pan starts bitscomp with the two pan windows (WY-8), default the one-window copy path.
set -euo pipefail
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/wy-72-latency}"
mkdir -p "$RUN_DIR"
RUN_DIR="$(realpath "$RUN_DIR")"
LO="${BITSOS_PORT_LO:-30500}"
FWD=$((LO + 72))
TOKEN="latency-scenario-token-5d17"
LOG="$RUN_DIR/serial.log"
QMP="$RUN_DIR/qmp.sock"
NAME=wy-latency
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-$PWD/target}
# WY-8: a QEMU left over on the forward port would answer instead of this run's (it happened: three runs measured
# a 10 h old guest while the new QEMU failed to forward).
if ss -Htln "sport = :$FWD" 2>/dev/null | grep -q .; then
    echo "RED: port $FWD is in use already (a leftover QEMU?); stop it by its pid first"
    exit 1
fi

cargo xtask image --name "$NAME" --config-extra "token=$TOKEN;mcp_port=8765;loglevel=${WY_LOGLEVEL:-warn};screen=${LAT_SCREEN:-1280x800}"
rm -f "$LOG" "$QMP"
cargo xtask qemu --name "$NAME" --net virtio --hostfwd "$FWD:8765" --timeout 900 --mem "${LAT_MEM:-4G}" --serial-log "$LOG" \
    -- -smp "${LAT_SMP:-4}" -qmp "unix:$QMP,server=on,wait=off" > "$RUN_DIR/qemu.out" 2>&1 &
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
python3 -I host/tests/wy/v2_latency.py "$FWD" "$TOKEN" "$QMP" "$RUN_DIR"
if grep -aq "PANIC" "$LOG"; then echo "RED: the kernel panicked"; tail -20 "$LOG"; exit 1; fi
echo "GREEN: terminals started while the pointer moved; latency figures above"
