#!/bin/bash
# slow
# MC-L: the local tools socket /run/bitsos/tools.sock in a normally started system (the ktests mc.local.* use
# sockets of their own: a ktest boot never starts MCP). The socket is there, another program than the boot's
# /bin/assist (here /bin/mcp-fakesvc as uid 1000, started through the network side's `run`) gets one refusal
# line and the refusal is in the log; the network side goes on answering; after a Reload (host/mcp/reload.mjs,
# as mcp/50-reload.sh) the new kernel has made the socket again and refuses the same way.
set -eu
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/mc-55-local}"
mkdir -p "$RUN_DIR"
RUN_DIR="$(realpath "$RUN_DIR")"
HERE="$(pwd)/host/tests/mcp"
TOOLS="$(pwd)/host/mcp"
# shellcheck source=host/tests/mcp/lib.bash
. "$HERE/lib.bash"
LO="${BITSOS_PORT_LO:-25000}"
FWD=$((LO + 425))
HTTP=$((LO + 426))
TOKEN="mcl-local-scenario-7c1d"
LOG="$RUN_DIR/serial.log"
NAME=mc-local-scn
HOST="127.0.0.1:$FWD"
SOCK=/run/bitsos/tools.sock
export BITSOS_TOKEN="$TOKEN"

cargo xtask image --name "$NAME" --config-extra "token=$TOKEN;mcp_port=8765;loglevel=info"
stop_qemu "$NAME"
rm -f "$LOG"
cargo xtask qemu --name "$NAME" --net igb --smp 4 --hostfwd "$FWD:8765" --timeout 500 --serial-log "$LOG" > "$RUN_DIR/qemu.out" 2>&1 &
QPID=$!
cleanup() { stop_run "$QPID" "$NAME"; }
trap cleanup EXIT

for _ in $(seq 1 90); do
    if node "$TOOLS/mcp-call.mjs" "$HOST" version > /dev/null 2>&1; then break; fi
    kill -0 "$QPID" 2>/dev/null || { echo "RED: QEMU ended before MCP answered"; tail -20 "$LOG"; exit 1; }
    sleep 1
done
red() { echo "RED: $*"; tail -40 "$LOG"; exit 1; }

LIST='{"jsonrpc":"2.0","id":1,"method":"tools/list"}'
CALL='{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"version","arguments":{}}}'

# One round: the socket is there, a stranger is refused, the refusal is logged, the network side answers.
round() {
    local tag="$1" refused_before
    node "$TOOLS/mcp-call.mjs" "$HOST" stat "{\"path\":\"$SOCK\"}" > "$RUN_DIR/stat-$tag.out" || red "$tag: no $SOCK"
    grep -q '"type": *"socket"' "$RUN_DIR/stat-$tag.out" || { cat "$RUN_DIR/stat-$tag.out"; red "$tag: $SOCK is not a socket"; }
    refused_before="$(grep -ac 'mcp local.refused' "$LOG" || true)"
    node "$TOOLS/mcp-call.mjs" "$HOST" run "$(printf '{"path":"/bin/mcp-fakesvc","args":["ask","%s",%s,%s]}' "$SOCK" "$(printf '%s' "$LIST" | python3 -c 'import json,sys; print(json.dumps(sys.stdin.read()))')" "$(printf '%s' "$CALL" | python3 -c 'import json,sys; print(json.dumps(sys.stdin.read()))')")" > "$RUN_DIR/stranger-$tag.out" 2>&1 || true
    grep -q 'Refused: ' "$RUN_DIR/stranger-$tag.out" || { cat "$RUN_DIR/stranger-$tag.out"; red "$tag: a stranger was not refused"; }
    grep -q '^closed' "$RUN_DIR/stranger-$tag.out" || { cat "$RUN_DIR/stranger-$tag.out"; red "$tag: a refused program's connection stayed open"; }
    if grep -q '"tools"' "$RUN_DIR/stranger-$tag.out" || grep -q 'BitsOS V2 kernel' "$RUN_DIR/stranger-$tag.out"; then
        cat "$RUN_DIR/stranger-$tag.out"; red "$tag: a stranger got an answer"
    fi
    for _ in $(seq 1 20); do
        [ "$(grep -ac 'mcp local.refused' "$LOG" || true)" -gt "$refused_before" ] && break
        sleep 0.25
    done
    [ "$(grep -ac 'mcp local.refused' "$LOG" || true)" -gt "$refused_before" ] || red "$tag: the refusal is not in the log"
    node "$TOOLS/mcp-call.mjs" "$HOST" version > "$RUN_DIR/version-$tag.out" || red "$tag: the network side does not answer"
    grep -q '^BitsOS V2 kernel' "$RUN_DIR/version-$tag.out" || red "$tag: version"
}

grep -aq 'mcp local.listen' "$LOG" || red "no local.listen event at the start"
round boot

node "$TOOLS/reload.mjs" "$HOST" --name mc-local-new --bind 127.0.0.1 --port "$HTTP" --count 1 --back-s 90 | tee "$RUN_DIR/reload.out"
[ "${PIPESTATUS[0]}" -eq 0 ] || red "reload.mjs"
grep -q "^reload: 1 of 1 rounds came back with the new kernel" "$RUN_DIR/reload.out" || red "the Reload did not come back"
[ "$(grep -ac 'mcp local.listen' "$LOG")" -eq 2 ] || red "the new kernel did not make the local tools socket again"
round reload

echo "GREEN: mcp/55-local"
