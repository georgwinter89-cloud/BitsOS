#!/bin/bash
# slow
# MC-4a: the reload loop through host/mcp/reload.mjs (the tool the desk's Mini-PC rounds use): boot an image with
# an igb card (the I226's driver family) and 4 CPUs, then reload.mjs builds a second image itself
# (cargo xtask image --name mc-reload-new), serves its KERNEL + INITRD on 127.0.0.1 (the guest reaches it at
# 10.0.2.2) and reloads three times in a row; each round must come back with the Reload counter + 1, a new
# `version` start line and the served initramfs (size + FNV-1a 64). Then the refusals reload.mjs reports
# (a public address is refused by the kernel before anything is fetched).
set -eu
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/mc-50-reload}"
mkdir -p "$RUN_DIR"
RUN_DIR="$(realpath "$RUN_DIR")"
HERE="$(pwd)/host/tests/mcp"
TOOLS="$(pwd)/host/mcp"
# shellcheck source=host/tests/mcp/lib.bash
. "$HERE/lib.bash"
LO="${BITSOS_PORT_LO:-25000}"
FWD=$((LO + 401))
HTTP=$((LO + 410))
TOKEN="mc4a-reload-scenario-3e8a"
LOG="$RUN_DIR/serial.log"
NAME=mc-reload-scn
HOST="127.0.0.1:$FWD"
COUNT="${MC_RELOADS:-3}"
export BITSOS_TOKEN="$TOKEN"

if ! grep -q "kexec::set_boot_info" kernel/src/main.rs; then
    echo "SKIP: kernel/src/main.rs lacks kexec::set_boot_info (RB-5a's desk lines)"
    exit 0
fi

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

node "$TOOLS/reload.mjs" "$HOST" --name mc-reload-new --bind 127.0.0.1 --port "$HTTP" --count "$COUNT" --back-s 90 | tee "$RUN_DIR/reload.out"
[ "${PIPESTATUS[0]}" -eq 0 ] || red "reload.mjs"
grep -q "^reload: $COUNT of $COUNT rounds came back with the new kernel" "$RUN_DIR/reload.out" || red "summary line"
[ "$(grep -c '^round [0-9]*: back after' "$RUN_DIR/reload.out")" -eq "$COUNT" ] || red "one timing line per round"
node "$TOOLS/mcp-call.mjs" "$HOST" version | tee "$RUN_DIR/version.out"
grep -q "^start: reload $COUNT," "$RUN_DIR/version.out" || red "version names reload $COUNT"
[ "$(grep -ac 'kexec reload.jump' "$LOG")" -eq "$COUNT" ] || red "$COUNT reload.jump events in the log"

# A public address: the kernel refuses before fetching; reload.mjs exits 1 and says why.
set +e
node "$TOOLS/reload.mjs" "$HOST" --no-build --name mc-reload-new --bind 127.0.0.1 --port "$HTTP" --me 8.8.8.8 > "$RUN_DIR/refused.out" 2>&1; e=$?
set -e
[ "$e" -eq 1 ] && grep -q "reload refused: Refused: .*not on a private network" "$RUN_DIR/refused.out" || { cat "$RUN_DIR/refused.out"; red "public address not refused (exit $e)"; }
grep -aq "$TOKEN" "$LOG" && red "the token appears in the log (R8i)"
echo "mc reload: green"
