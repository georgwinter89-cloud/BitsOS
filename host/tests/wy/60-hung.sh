#!/bin/bash
# slow
# WY-4 (B4): bitscomp never waits for a client, on V2 (QEMU -smp 4, 1280x800 GOP). wyclient A animates while B stops
# itself (SIGSTOP) and C answers: B turns "antwortet nicht" (MCP tool windows hung=true, grey veil with the box on
# MCP screen), A keeps getting frame callbacks (p50/p99/max recorded in numbers.txt, not judged in QEMU; only a
# loose floor of 10 per 2 s and starved=0), "Beenden" clicked through the kernel's pointer path ends B (SIGTERM
# with SIGCONT, no SIGKILL), MCP window_kill ends C (ignores SIGTERM: SIGKILL after 2 s), "Programm beenden" from
# the circle menu ends A; then all again with 64 busy threads (the freeze suite's `stress`) running.
# Mini-PC numbers: plan/notes/wy-WY-4.md.
set -euo pipefail
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/wy-60-hung}"
mkdir -p "$RUN_DIR"
RUN_DIR="$(realpath "$RUN_DIR")"
LO="${BITSOS_PORT_LO:-30500}"
FWD=$((LO + 60))
TOKEN="wy4-scenario-token-3c81"
LOG="$RUN_DIR/serial.log"
QMP="$RUN_DIR/qmp.sock"
NAME=wy-hung
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-$PWD/target}

cargo xtask image --name "$NAME" --config-extra "token=$TOKEN;mcp_port=8765;loglevel=${WY_LOGLEVEL:-warn};screen=1280x800" > "$RUN_DIR/image.txt" 2>&1 || { tail -20 "$RUN_DIR/image.txt"; exit 1; }
rm -f "$LOG" "$QMP"
cargo xtask qemu --name "$NAME" --net virtio --hostfwd "$FWD:8765" --timeout 600 --serial-log "$LOG" \
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
python3 -I host/tests/wy/v2_hung.py "$FWD" "$TOKEN" "$QMP" "$RUN_DIR"
if grep -aq "PANIC" "$LOG"; then echo "RED: the kernel panicked"; tail -20 "$LOG"; exit 1; fi
echo "GREEN: bitscomp never waits for a hung client (numbers: $RUN_DIR/numbers.txt)"
