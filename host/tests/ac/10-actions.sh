#!/bin/bash
# minipc: left the QEMU runs (Georg 30.10., CONTRACTS section 4); its checks run on the Mini-PC by the check list in plan/notes/ac-AC-3.md
# slow
# AC-3: brave-act (Brave's actions for Claude over the DevTools pipe) on V2 in QEMU (-smp 4, 6 GiB), headless
# bitscomp, a local test site served by BusyBox httpd inside the guest (no internet), every action once through
# MCP, the change actions refused without permission (v2_act.py). Needs the staged Brave tree
# (bash user/ports/brave-launcher/scripts/stage-v2.sh). Times are printed, never judged.
set -euo pipefail
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
ROOT=$PWD
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/ac-10-actions}"
mkdir -p "$RUN_DIR"
RUN_DIR="$(realpath "$RUN_DIR")"
LO="${BITSOS_PORT_LO:-17500}"
FWD=$((LO + 10))
TOKEN="ac3-scenario-token-91d4"
LOG="$RUN_DIR/serial.log"
NAME=ac-actions
STAGE=${BRAVE_STAGE:-/var/tmp/claude-v2/stage/brave}
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-$ROOT/target}
export BRAVE_STAGE=$STAGE
. host/tests/mcp/lib.bash
[ -x "$STAGE/brave" ] || { echo "RED: no staged Brave in $STAGE (bash user/ports/brave-launcher/scripts/stage-v2.sh)"; exit 1; }

V2_STICK_TREES=brave cargo xtask image --name "$NAME" \
    --config-extra "token=$TOKEN;mcp_port=8765;loglevel=warn,user=info;screen=1280x800"
stop_qemu "$NAME"
rm -f "$LOG"
cargo xtask qemu --name "$NAME" --mem 6G --net virtio --hostfwd "$FWD:8765" --timeout 900 --serial-log "$LOG" \
    -- -smp 4 > "$RUN_DIR/qemu.out" 2>&1 &
QPID=$!
trap 'stop_run "$QPID" "$NAME"' EXIT
python3 -I host/tests/ac/v2_act.py "$FWD" "$TOKEN" "$RUN_DIR"
if grep -aq "PANIC" "$LOG"; then echo "RED: the kernel panicked"; tail -20 "$LOG"; exit 1; fi
echo "GREEN: brave-act on V2"
