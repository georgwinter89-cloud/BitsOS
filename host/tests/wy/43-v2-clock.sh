#!/bin/bash
# slow
# WY-17b on V2 in QEMU: the desktop's clock shows local time. QEMU's RTC holds the host's local time (xtask `-rtc
# base=localtime`), the kernel takes the RTC for Europe/Berlin (`tz=` default) and hands programs UTC, and bitscomp
# goes back through /etc/localtime: what the status bar shows must be what the RTC holds, without " UTC". Second boot:
# an RTC in UTC with CONFIG tz=UTC: the bar shows Berlin's time all the same, the kernel's UTC is UTC. Third boot: an RTC
# in UTC nobody declared, the Mini-PC's case (wy-WY-17b.md): the bar shows UTC's time, unmarked, and the kernel's UTC is
# early by the zone's offset. That boot pins the symptom until the desk's CONFIG line or `rtc=utc` is in.
set -euo pipefail
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/wy-43-clock}"
mkdir -p "$RUN_DIR"
RUN_DIR="$(realpath "$RUN_DIR")"
LO="${BITSOS_PORT_LO:-30500}"
FWD=$((LO + 43))
TOKEN="wy17b-scenario-token-c10c"
LOG="$RUN_DIR/serial.log"
QMP="$RUN_DIR/qmp.sock"
NAME=wy-clock
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-$PWD/target}

# boot "<CONFIG addition>" "<QEMU's -rtc base>" "<local|utc>"
boot() {
    cargo xtask image --name "$NAME" --config-extra "token=$TOKEN;mcp_port=8765;loglevel=warn;screen=1280x800$1" > "$RUN_DIR/image.txt" 2>&1 || { tail -20 "$RUN_DIR/image.txt"; exit 1; }
    rm -f "$LOG" "$QMP"
    cargo xtask qemu --name "$NAME" --net virtio --hostfwd "$FWD:8765" --timeout 300 --serial-log "$LOG" \
        -- -smp 4 -qmp "unix:$QMP,server=on,wait=off" -rtc "base=$2" > "$RUN_DIR/qemu.out" 2>&1 &
    QPID=$!
    python3 -I host/tests/wy/v2_clock.py "$FWD" "$TOKEN" "$QMP" "$RUN_DIR" "$3"
    if grep -aq "PANIC" "$LOG"; then echo "RED: the kernel panicked"; tail -20 "$LOG"; exit 1; fi
    stop
}
stop() {
    kill "$QPID" 2>/dev/null || true
    pf="$CARGO_TARGET_DIR/xtask/qemu-$NAME.pid"
    if [ -f "$pf" ]; then
        pid="$(head -1 "$pf")"
        if tr '\0' ' ' < "/proc/$pid/cmdline" 2>/dev/null | grep -Eq -- "-name v2-[A-Za-z0-9]+-$NAME( |$)"; then kill "$pid" 2>/dev/null || true; fi
    fi
    wait "$QPID" 2>/dev/null || true
}
QPID=
trap stop EXIT
echo "== the RTC holds local time, the kernel's default"
boot "" localtime local
echo "== the RTC holds UTC, CONFIG tz=UTC"
boot ";tz=UTC" utc utc
echo "== the RTC holds UTC, nothing says so (the Mini-PC since at least 27.10.)"
boot "" utc undeclared
echo "GREEN: the desktop's clock shows local time"
