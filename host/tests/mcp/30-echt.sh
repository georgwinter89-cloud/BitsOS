#!/bin/bash
# slow
# MC-5a + MC hardening: the A1 echt suite (host/tests/mcp/echt.mjs) against a normal boot with 16 CPUs, MCP
# through hostfwd in the MC port range: protocol, tools/list shapes (B5, R8f), every tool that only looks, and
# run (hello, hello-rs, separate stderr, fault message, stdin_b64, a program that never ends: deadline answer
# then stop, a sleep loop: partial output, run's 30 s default deadline), fetch and install_kernel's dry run with this
# image's own files, served from this script. Run: `cargo xtask fullrun`. On the Mini-PC echt.mjs needs the same three
# arguments, else install.dry_run is FAIL.
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
HTTP=$((LO + 301))
TOKEN="mc5a-echt-scenario-4b1e"
LOG="$RUN_DIR/serial.log"
NAME=mc-echt-scn

cargo xtask image --name "$NAME" --config-extra "token=$TOKEN;mcp_port=8765;loglevel=info"
# install.dry_run wants what a real stick has: a confirmed generation to fall back on and room for the new one (as mcp/70).
X="${CARGO_TARGET_DIR:-target}/xtask"
BUNDLE="$X/esp-$NAME/BITSOS2/SYS/1"
touch "$BUNDLE/OK"
IMG="$X/$NAME.img"
CONTENT=$(du -sb "$X/esp-$NAME" | cut -f1)
rm -f "$IMG"
mkfs.fat -F 32 -n BITSOS -C "$IMG" $(( (CONTENT * 4 + (64 << 20)) / 1024 / 1024 * 1024 )) > /dev/null
mcopy -s -i "$IMG" "$X/esp-$NAME/EFI" "$X/esp-$NAME/BITSOS2" ::/
stop_qemu "$NAME"
rm -f "$LOG"
cargo xtask qemu --name "$NAME" --mem 4096 --net virtio --smp 16 --hostfwd "$FWD:8765" --timeout 400 --serial-log "$LOG" > "$RUN_DIR/qemu.out" 2>&1 &
QPID=$!
cleanup() { stop_run "$QPID" "$NAME"; }
trap cleanup EXIT

for _ in $(seq 1 90); do
    if BITSOS_TOKEN="$TOKEN" node "$HERE/echt.mjs" --host "127.0.0.1:$FWD" --only protocol.initialize > /dev/null 2>&1; then break; fi
    kill -0 "$QPID" 2>/dev/null || { echo "RED: QEMU ended before MCP answered"; tail -20 "$LOG"; exit 1; }
    sleep 1
done
BITSOS_TOKEN="$TOKEN" node "$HERE/echt.mjs" --host "127.0.0.1:$FWD" --http "10.0.2.2:$HTTP" --http-bind 127.0.0.1 --install-bundle "$BUNDLE" | tee "$RUN_DIR/echt.out"
status=${PIPESTATUS[0]}
if [ "$status" -ne 0 ]; then echo "RED: echt suite (exit $status)"; tail -40 "$LOG"; exit 1; fi
if grep -aq "$TOKEN" "$LOG"; then echo "RED: the token appears in the log (R8i)"; exit 1; fi
grep -aq "proc run.still_running" "$LOG" || { echo "RED: no run.still_running event (B6)"; exit 1; }
grep -aq "proc mcp.kill " "$LOG" || { echo "RED: no mcp.kill event (B6, MC-5b)"; exit 1; }
echo "mc echt: green"
