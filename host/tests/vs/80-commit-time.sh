#!/bin/bash
# slow
# VS-7: commit times of the /data pool on a throttled stick (QEMU block_set_io_throttle on the stick drive: writes
# 2 MiB/s like Georg's stick, reads 16 MiB/s). 1. 4 MiB written into /data with fsync, nothing else on the stick.
# 2. The same while a Background reader (ionice -c3, as the FS-13 preload) streams a 48 MiB file from the stick.
# The write-back of a commit that carries much runs before it takes the pool lock (held time small).
# Every commit is logged (loglevel bitsvol=debug) with bytes, flushes, lock wait and I/O time; no commit may be
# over its budget (timing.rs: 1 s + carried bytes at 1 MiB/s). Prints the commit lines of both phases.
set -eu
export LC_ALL=C
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
for t in python3 mcopy mkfs.fat; do command -v $t >/dev/null || { echo "SKIP: $t missing"; exit 0; }; done
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/vs-80-commit-time}"
mkdir -p "$RUN_DIR"
RUN_DIR="$(realpath "$RUN_DIR")"
LO="${BITSOS_PORT_LO:-18000}"
FWD=$((LO + 80))
TOKEN="vs7-commit-token-3b90"
LOG="$RUN_DIR/serial.log"
MON="$RUN_DIR/mon.sock"
NAME=vs-commit
IMG="target/xtask/$NAME.img"
BIG_MB=48

cargo xtask image --name "$NAME" --config-extra "token=$TOKEN;mcp_port=8765;loglevel=info,bitsvol=debug;wlan=off;nvme_write=off;data_img_mb=64"
kib=$(( $(stat -c %s "$IMG") / 1024 + 700 * 1024 ))
rm -f "$IMG"
mkfs.fat -F 32 -n BITSOS -C "$IMG" "$kib" > /dev/null
mcopy -s -i "$IMG" "target/xtask/esp-$NAME/EFI" "target/xtask/esp-$NAME/BITSOS2" ::/
head -c $((BIG_MB << 20)) /dev/urandom > "$RUN_DIR/big.bin"
mcopy -i "$IMG" "$RUN_DIR/big.bin" ::BITSOS2/vs7-big.bin
rm -f "$RUN_DIR/big.bin" "$LOG" "$MON"
cargo xtask qemu --name "$NAME" --net virtio --hostfwd "$FWD:8765" --timeout 900 --serial-log "$LOG" \
    -- -monitor "unix:$MON,server,nowait" > "$RUN_DIR/qemu.out" 2>&1 &
QPID=$!
stop() {
    kill "$QPID" 2>/dev/null || true
    pf="target/xtask/qemu-$NAME.pid"
    if [ -f "$pf" ]; then
        pid="$(head -1 "$pf")"
        if tr '\0' ' ' < "/proc/$pid/cmdline" 2>/dev/null | grep -Eq -- "-name v2-[A-Za-z0-9]+-$NAME( |$)"; then kill "$pid" 2>/dev/null || true; fi
    fi
    wait "$QPID" 2>/dev/null || true
    rm -f "$IMG"
}
trap stop EXIT
python3 -I host/tests/vs/commit_time_check.py "$FWD" "$TOKEN" "$MON" "$LOG"
if grep -aq "PANIC" "$LOG"; then echo "RED: the kernel panicked"; tail -20 "$LOG"; exit 1; fi
echo "GREEN: commit times on a throttled stick reported and within budget"
