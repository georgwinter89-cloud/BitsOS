#!/bin/bash
# slow
# MM-rmap: foot's scroll pools stay bounded without bitscomp's 64 MiB cap (tweak.max-shm-pool-size-mb): foot
# scrolls by moving its buffer forward in a large memfd and punches the pages behind it (FALLOC_FL_PUNCH_HOLE)
# while bitscomp still maps the pool; since MM-rmap the punch takes those pages out of both mappings and frees
# them. One foot in bitscomp (QEMU -smp 4, 1280x800) prints 200000 lines; /proc/meminfo Shmem is sampled while
# it scrolls. Red when Shmem grows by more than 64 MiB (before MM-rmap it grew toward foot's 512 MiB pool).
set -euo pipefail
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/mm-50-foot-scroll}"
mkdir -p "$RUN_DIR"
RUN_DIR="$(realpath "$RUN_DIR")"
LO="${BITSOS_PORT_LO:-21500}"
FWD=$((LO + 60))
TOKEN="mm-rmap-scroll-token-31c4"
LOG="$RUN_DIR/serial.log"
QMP="$RUN_DIR/qmp.sock"
NAME=mm-scroll
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-$PWD/target}
cargo xtask image --name "$NAME" --config-extra "token=$TOKEN;mcp_port=8765;loglevel=warn;screen=1280x800"
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
python3 -I host/tests/mm/v2_foot_scroll.py "$FWD" "$TOKEN" "$QMP" "$RUN_DIR"
if grep -aq "PANIC" "$LOG"; then echo "RED: the kernel panicked"; tail -20 "$LOG"; exit 1; fi
echo "GREEN: foot's scroll memory stays bounded"
