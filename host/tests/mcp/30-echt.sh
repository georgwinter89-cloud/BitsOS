#!/bin/bash
# slow
# MC-5a + MC hardening: the A1 echt suite (host/tests/mcp/echt.mjs) against a normal boot with 16 CPUs, MCP
# through hostfwd in the MC port range: protocol, tools/list shapes (B5, R8f), every tool that only looks, and
# run (hello, hello-rs, separate stderr, fault message, stdin_b64, a program that never ends: deadline answer
# then stop, a sleep loop: partial output, run's 30 s default deadline). Later the desk points echt.mjs at the
# Mini-PC (plan/notes/mc-MC-5a.md).
set -eu
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/mc-30-echt}"
mkdir -p "$RUN_DIR"
RUN_DIR="$(realpath "$RUN_DIR")"
HERE="$(pwd)/host/tests/mcp"
# shellcheck source=host/tests/mcp/lib.bash
. "$HERE/lib.bash"
LO="${BITSOS_PORT_LO:-25000}"
FWD=$((LO + 300))
TOKEN="mc5a-echt-scenario-4b1e"
LOG="$RUN_DIR/serial.log"
NAME=mc-echt-scn

cargo xtask image --name "$NAME" --config-extra "token=$TOKEN;mcp_port=8765;loglevel=info"
stop_qemu "$NAME"
rm -f "$LOG"
cargo xtask qemu --name "$NAME" --net virtio --smp 16 --hostfwd "$FWD:8765" --timeout 400 --serial-log "$LOG" > "$RUN_DIR/qemu.out" 2>&1 &
QPID=$!
cleanup() { stop_run "$QPID" "$NAME"; }
trap cleanup EXIT

for _ in $(seq 1 90); do
    if BITSOS_TOKEN="$TOKEN" node "$HERE/echt.mjs" --host "127.0.0.1:$FWD" --only protocol.initialize > /dev/null 2>&1; then break; fi
    kill -0 "$QPID" 2>/dev/null || { echo "RED: QEMU ended before MCP answered"; tail -20 "$LOG"; exit 1; }
    sleep 1
done
BITSOS_TOKEN="$TOKEN" node "$HERE/echt.mjs" --host "127.0.0.1:$FWD" | tee "$RUN_DIR/echt.out"
status=${PIPESTATUS[0]}
if [ "$status" -ne 0 ]; then echo "RED: echt suite (exit $status)"; tail -40 "$LOG"; exit 1; fi
if grep -aq "$TOKEN" "$LOG"; then echo "RED: the token appears in the log (R8i)"; exit 1; fi
grep -aq "proc run.still_running" "$LOG" || { echo "RED: no run.still_running event (B6)"; exit 1; }
grep -aq "proc mcp.kill " "$LOG" || { echo "RED: no mcp.kill event (B6, MC-5b)"; exit 1; }
echo "mc echt: green"
