#!/bin/bash
# slow
# WY-1b: real Wayland programs in bitscomp on V2 (QEMU -smp 4, 1280x800 GOP): foot from CachyOS (its libraries,
# Noto Sans Mono, fontconfig, terminfo, C.UTF-8, the compose table: user/packaging/manifest.txt) started by
# Strg+Alt+Enter and by the circle menu's "Neues Terminal"; German text with umlauts and AltGr typed into foot's
# BusyBox shell and read back with MCP read_file; clipboard from one foot to the other; foot resizes with its tile
# (stty size); stack/unstack, float and close by `bitscomp ctl` and by pointer clicks through the kernel's evdev
# path; `exit` closes a window; `ctl quit` ends the remaining foot and gives the console back. foot's startup time
# and memory are printed, not judged (Mini-PC list in plan/notes/wy-WY-1b.md). Also: a tab dragged by the
# pointer, a foot killed while it holds the clipboard, ENOSYS of bitscomp/foot/its shell beyond the known ones
# (the syscall subsystem logs at info), and memfd_lazy.c (foot's buffer pools; CachyOS is the oracle; V2 must
# match since FS-14). WY_LOGLEVEL=info records all info events.
set -euo pipefail
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/wy-50-foot}"
mkdir -p "$RUN_DIR"
RUN_DIR="$(realpath "$RUN_DIR")"
LO="${BITSOS_PORT_LO:-30500}"
FWD=$((LO + 50))
TOKEN="wy1b-scenario-token-7e2f"
LOG="$RUN_DIR/serial.log"
QMP="$RUN_DIR/qmp.sock"
NAME=wy-foot
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-$PWD/target}

PROBE="$RUN_DIR/memfd-lazy"
V2_SYSROOT="${V2_SYSROOT:-$PWD/user/sysroot/out/sysroot}" user/sysroot/bin/v2-cc -O2 -Wall -Werror -std=gnu11 \
    -o "$PROBE" host/tests/wy/memfd_lazy.c
"$PROBE" | grep -qx "result ok" || { echo "RED: memfd_lazy on CachyOS (the oracle) did not say result ok"; exit 1; }
export WY_MEMFD_PROBE="$PROBE"
cargo xtask image --name "$NAME" --config-extra "token=$TOKEN;mcp_port=8765;loglevel=${WY_LOGLEVEL:-warn,syscall=info};screen=1280x800"
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
python3 -I host/tests/wy/v2_foot.py "$FWD" "$TOKEN" "$QMP" "$RUN_DIR"
if grep -aq "PANIC" "$LOG"; then echo "RED: the kernel panicked"; tail -20 "$LOG"; exit 1; fi
echo "GREEN: foot in bitscomp on V2"
