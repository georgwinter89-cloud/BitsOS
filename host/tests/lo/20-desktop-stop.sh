#!/bin/bash
# TS-1: what host/tests/minipc/bare.sh does to the desktop, in QEMU where nothing can go wrong: a normal boot with
# desktop=on and MCP (plain HTTP), `bitscomp ctl quit` gives the screen back (dp.display.handover goes from Busy to ok),
# `bitscomp` typed into the console shell takes it again. mp.py is the scripts' own way to MCP.
set -euo pipefail
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
RUN="${BITSOS_RUN_DIR:-target/xtask/runs/lo-20-desktop-stop}"
mkdir -p "$RUN"
RUN="$(realpath "$RUN")"
# shellcheck source=host/tests/mcp/lib.bash
. "$(pwd)/host/tests/mcp/lib.bash"
LO="${BITSOS_PORT_LO:-14500}"
FWD=$((LO + 20))
NAME="lo-desktop-stop"
export BITSOS_TOKEN="lo20-desktop-scenario-token"
LOG="$RUN/serial.log"
cargo xtask image --name "$NAME" --config-extra "token=$BITSOS_TOKEN;mcp_port=8765;loglevel=info;desktop=on"
stop_qemu "$NAME"
rm -f "$LOG"
cargo xtask qemu --name "$NAME" --mem 3G --net virtio --hostfwd "$FWD:8765" --timeout 240 --serial-log "$LOG" -- -smp 4 > "$RUN/qemu.out" 2>&1 &
QPID=$!
trap 'stop_run "$QPID" "$NAME"' EXIT
python3 -I host/tests/lo/desktop_stop.py "127.0.0.1:$FWD"
echo "GREEN: the desktop stops and starts again as bare.sh needs it"
