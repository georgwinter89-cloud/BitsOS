#!/bin/bash
# MC-2: a normal boot (not ktest) with CONFIG token=..., MCP reached from the host through hostfwd in the MC port
# range: tools/list (owners, safety classes, no power-off), hw_info (generation and next start), net_info (the
# QEMU lease), cpus, msr_read, read_phys, ktest, output, reboot dry run; then reboot for real: the answer comes
# first, then the reset (QEMU runs with -no-reboot, so the reset ends it).
set -eu
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/mc-10-tools}"
mkdir -p "$RUN_DIR"
RUN_DIR="$(realpath "$RUN_DIR")"
HERE="$(pwd)/host/tests/mcp"
# shellcheck source=host/tests/mcp/lib.bash
. "$HERE/lib.bash"
LO="${BITSOS_PORT_LO:-25000}"
FWD=$((LO + 100))
TOKEN="mc2-scenario-token-7f3a"
LOG="$RUN_DIR/serial.log"

# Needs two kernel lines the desk applies (plan/notes/mc-MC-2.md, change request 1): without net::init the MCP
# server listens on the loopback only, without mcp::set_boot_info the generation is unknown.
if ! grep -q "mcp::set_boot_info" kernel/src/main.rs || ! grep -q "net::init" kernel/src/main.rs; then
    echo "SKIP: kernel/src/main.rs lacks mcp::set_boot_info or net::init (desk change request 1 of MC-2 / NW-3); MCP is not reachable from the host yet"
    exit 0
fi

cargo xtask image --name mc-tools --config-extra "token=$TOKEN;mcp_port=8765;loglevel=info"
# A QEMU left over from an earlier run of this script (killed wrapper) still holds the port and the socket.
stop_qemu mc-tools
rm -f "$LOG"
cargo xtask qemu --name mc-tools --net virtio --hostfwd "$FWD:8765" --timeout 240 --serial-log "$LOG" > "$RUN_DIR/qemu.out" 2>&1 &
QPID=$!
cleanup() { stop_run "$QPID" mc-tools; }
trap cleanup EXIT

python3 -I "$HERE/mcp_client.py" "$FWD" "$TOKEN" check
python3 -I "$HERE/mcp_client.py" "$FWD" "$TOKEN" reboot
for _ in $(seq 1 60); do kill -0 "$QPID" 2>/dev/null || break; sleep 0.5; done
if kill -0 "$QPID" 2>/dev/null; then echo "RED: QEMU still runs 30 s after the reboot answer"; tail -20 "$LOG"; exit 1; fi
grep -a "tool.after_answer tool=reboot\|reboot\.\(synced\|quiesced\|step\|refused\|failed\)" "$LOG" || true
grep -aq "mcp tool.after_answer tool=reboot" "$LOG" || { echo "RED: no after_answer event"; exit 1; }
# The reset is acpi's `power reboot.step` (CP-5); QEMU's exit above shows it happened, and the serial printer may
# lose the last lines to a fast reset, so the reboot path's own steps before it count too (INT-W3).
grep -aqE "power reboot.step n=|mcp reboot.(synced|quiesced)" "$LOG" || { echo "RED: no reboot step logged"; exit 1; }
if grep -aq "reboot.failed" "$LOG"; then echo "RED: every reset path failed"; exit 1; fi
if grep -aq "$TOKEN" "$LOG"; then echo "RED: the token appears in the log (R8i)"; exit 1; fi
echo "mc tools: green (tool list, hw_info, net_info, cpus, msr_read, read_phys, ktest, output, reboot answered first, then reset)"
