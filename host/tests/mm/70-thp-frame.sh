#!/bin/bash
# MM-13: FD-1's frame test ($HOME/BitsOS/V2/target/a0/www/fd/frame: a fresh 13.8 MB buffer per frame, copied
# into, unmapped) runs on V2 in QEMU (-smp 4), fetched over MCP `fetch` from a local HTTP server (QEMU's host address
# 10.0.2.2; the program is larger than one MCP request) and started by `run`, for correctness only: it ends normally and
# prints its numbers, mm.thp.fault_alloc rises while it runs, no counter mismatch, underflow or failed split is
# logged. QEMU timing is never judged (the numbers go to the run dir for the desk). FD1_FRAME overrides the path.
set -euo pipefail
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/mm-70-thp-frame}"
mkdir -p "$RUN_DIR"
RUN_DIR="$(realpath "$RUN_DIR")"
FRAME="${FD1_FRAME:-$HOME/BitsOS/V2/target/a0/www/fd/frame}"
if [ ! -f "$FRAME" ]; then
    echo "skip: no FD-1 frame program at $FRAME"
    exit 0
fi
LO="${BITSOS_PORT_LO:-21500}"
FWD=$((LO + 70))
WWW_PORT=$((LO + 71))
TOKEN="mm-thp-frame-token-5d21"
LOG="$RUN_DIR/serial.log"
NAME=mm-thp
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-$PWD/target}
cargo xtask image --name "$NAME" --config-extra "token=$TOKEN;mcp_port=8765;loglevel=info"
rm -f "$LOG"
mkdir -p "$RUN_DIR/www"
cp "$FRAME" "$RUN_DIR/www/frame"
python3 -I -m http.server --bind 127.0.0.1 --directory "$RUN_DIR/www" "$WWW_PORT" > "$RUN_DIR/www.log" 2>&1 &
WPID=$!
cargo xtask qemu --name "$NAME" --net virtio --hostfwd "$FWD:8765" --timeout 600 --serial-log "$LOG" -- -smp 4 > "$RUN_DIR/qemu.out" 2>&1 &
QPID=$!
stop() {
    kill "$WPID" 2>/dev/null || true
    kill "$QPID" 2>/dev/null || true
    pf="target/xtask/qemu-$NAME.pid"
    if [ -f "$pf" ]; then
        pid="$(head -1 "$pf")"
        if tr '\0' ' ' < "/proc/$pid/cmdline" 2>/dev/null | grep -Eq -- "-name v2-[A-Za-z0-9]+-$NAME( |$)"; then kill "$pid" 2>/dev/null || true; fi
    fi
    wait "$QPID" 2>/dev/null || true
}
trap stop EXIT
python3 -I host/tests/mm/v2_thp_frame.py "$FWD" "$TOKEN" "http://10.0.2.2:$WWW_PORT/frame" "$RUN_DIR"
if grep -aq "PANIC" "$LOG"; then echo "RED: the kernel panicked"; tail -20 "$LOG"; exit 1; fi
if grep -aEq "as.counter_mismatch|as.counter_underflow|thp.split_no_table|thp.split_failed|as.unmap_failed|as.protect_failed" "$LOG"; then
    echo "RED: MM logged a counter or split error"; grep -aE "counter_mismatch|counter_underflow|thp.split|unmap_failed|protect_failed" "$LOG" | head; exit 1
fi
echo "GREEN: FD-1's frame test runs on huge pages"
