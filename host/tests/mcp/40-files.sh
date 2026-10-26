#!/bin/bash
# slow
# MC-4a: the file tools and the host side against a normal boot (virtio-net, 4 CPUs, MCP through hostfwd in the
# MC port range): the echt suite's file checks (write/read/list/stat/delete, refusals, upload + run, fetch from
# a host HTTP server the guest reaches at 10.0.2.2), host/mcp/echt.mjs uploading and running the image's own
# static hello, host/mcp/mcp-call.mjs (--save of a read_file answer equals the local file; exit codes 0/1/2).
set -eu
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/mc-40-files}"
mkdir -p "$RUN_DIR"
RUN_DIR="$(realpath "$RUN_DIR")"
HERE="$(pwd)/host/tests/mcp"
TOOLS="$(pwd)/host/mcp"
# shellcheck source=host/tests/mcp/lib.bash
. "$HERE/lib.bash"
LO="${BITSOS_PORT_LO:-25000}"
FWD=$((LO + 400))
HTTP=$((LO + 420))
TOKEN="mc4a-files-scenario-7d21"
LOG="$RUN_DIR/serial.log"
NAME=mc-files-scn
HOST="127.0.0.1:$FWD"
export BITSOS_TOKEN="$TOKEN"

cargo xtask image --name "$NAME" --config-extra "token=$TOKEN;mcp_port=8765;loglevel=info"
stop_qemu "$NAME"
rm -f "$LOG"
cargo xtask qemu --name "$NAME" --net virtio --smp 4 --hostfwd "$FWD:8765" --timeout 300 --serial-log "$LOG" > "$RUN_DIR/qemu.out" 2>&1 &
QPID=$!
cleanup() { stop_run "$QPID" "$NAME"; }
trap cleanup EXIT

for _ in $(seq 1 90); do
    if node "$TOOLS/mcp-call.mjs" "$HOST" version > /dev/null 2>&1; then break; fi
    kill -0 "$QPID" 2>/dev/null || { echo "RED: QEMU ended before MCP answered"; tail -20 "$LOG"; exit 1; }
    sleep 1
done

red() { echo "RED: $*"; tail -30 "$LOG"; exit 1; }

# 1. The echt suite's file checks (incl. fetch from this machine).
node "$HERE/echt.mjs" --host "$HOST" --only files,tools.list --http "10.0.2.2:$HTTP" --http-bind 127.0.0.1 | tee "$RUN_DIR/echt.out"
[ "${PIPESTATUS[0]}" -eq 0 ] || red "echt file checks"
grep -q "^ok files.fetch" "$RUN_DIR/echt.out" || red "the fetch check did not run"

# 2. Upload and run with host/mcp/echt.mjs: the image's own static hello, then the wrong expectation fails.
node "$TOOLS/echt.mjs" "$HOST" target/xtask/cbin/hello | tee "$RUN_DIR/upload.out"
[ "${PIPESTATUS[0]}" -eq 0 ] || red "echt.mjs upload and run of hello"
grep -q "hello from musl C on BitsOS V2" "$RUN_DIR/upload.out" || red "no hello output"
if node "$TOOLS/echt.mjs" "$HOST" target/xtask/cbin/hello --expect-exit 3 > "$RUN_DIR/upload-wrong.out"; then red "a wrong --expect-exit passed"; fi
grep -q "^FAIL hello: exit 0, expected 3" "$RUN_DIR/upload-wrong.out" || red "wrong expectation not named"
node "$TOOLS/mcp-call.mjs" "$HOST" stat '{"path":"/tmp/echt/hello"}' > /dev/null 2>&1 && red "echt.mjs left its upload behind"

# 3. mcp-call.mjs: --save of read_file equals the local file; an error answer exits 1; a wrong token exits 2.
node "$TOOLS/mcp-call.mjs" "$HOST" read_file '{"path":"/bin/hello"}' --save "$RUN_DIR/hello.back"
cmp "$RUN_DIR/hello.back" target/xtask/cbin/hello || red "read_file --save differs from the image's hello"
set +e
node "$TOOLS/mcp-call.mjs" "$HOST" delete '{"path":"/tmp"}' > "$RUN_DIR/refused.out"; e1=$?
BITSOS_TOKEN=wrong-token-123456 node "$TOOLS/mcp-call.mjs" "$HOST" version > /dev/null 2>&1; e2=$?
set -e
[ "$e1" -eq 1 ] && grep -q "^Refused: " "$RUN_DIR/refused.out" || red "a refusal must exit 1 (got $e1)"
[ "$e2" -eq 2 ] || red "a wrong token must exit 2 (got $e2)"

# 4. B6 and R8i in the log.
for ev in "mcp file.write" "mcp file.deleted" "mcp file.refused" "mcp fetch.done"; do
    grep -aq "$ev" "$LOG" || red "no '$ev' event"
done
grep -aq "$TOKEN" "$LOG" && red "the token appears in the log (R8i)"
echo "mc files: green"
