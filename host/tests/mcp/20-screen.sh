#!/bin/bash
# MC-3: a normal boot (not ktest) with MCP reached from the host; `screen` compared pixel for pixel with QEMU's own
# picture (QMP screendump of the VGA display: full frame, a region, a region scaled by 2), the console as text,
# size and time of a full 2560x1440 shot printed, send_keys/send_mouse answered as injected input.
set -eu
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/mc-20-screen}"
mkdir -p "$RUN_DIR"
RUN_DIR="$(realpath "$RUN_DIR")"
HERE="$(pwd)/host/tests/mcp"
# shellcheck source=host/tests/mcp/lib.bash
. "$HERE/lib.bash"
LO="${BITSOS_PORT_LO:-25000}"
FWD=$((LO + 101))
TOKEN="mc3-scenario-token-4c1e"
LOG="$RUN_DIR/serial.log"
QMP="$RUN_DIR/qmp.sock"

if ! grep -q "net::init" kernel/src/main.rs; then
    echo "SKIP: kernel/src/main.rs lacks net::init; MCP is not reachable from the host"
    exit 0
fi

# loglevel=warn: the request lines (Info) stay off the screen, so the picture holds still between two dumps.
cargo xtask image --name mc-screen --config-extra "token=$TOKEN;mcp_port=8765;loglevel=warn"
# A QEMU left over from an earlier run of this script (killed wrapper) still holds the port and the socket.
stop_qemu mc-screen
rm -f "$LOG" "$QMP"
cargo xtask qemu --name mc-screen --net virtio --hostfwd "$FWD:8765" --timeout 300 --serial-log "$LOG" \
    -- -qmp "unix:$QMP,server=on,wait=off" > "$RUN_DIR/qemu.out" 2>&1 &
QPID=$!
cleanup() { stop_run "$QPID" mc-screen; }
trap cleanup EXIT

python3 -I "$HERE/screen_check.py" "$FWD" "$TOKEN" "$QMP" "$RUN_DIR"
if grep -aq "$TOKEN" "$LOG"; then echo "RED: the token appears in the log (R8i)"; exit 1; fi
if grep -aq "KERNEL PANIC\|PANIC" "$LOG"; then echo "RED: the kernel panicked"; tail -20 "$LOG"; exit 1; fi
echo "mc screen: green (PNG = QEMU screendump for frame, region and scaled region; text grid; injected keys and mouse)"
