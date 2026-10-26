#!/bin/bash
# slow
# RB-5a/RB-5b: Reload over MCP under QEMU (KVM) with an igb card (polled igc driver, slirp), qemu-xhci (the stick)
# and 16 CPUs (RB_SMP): refusals (ftp://, a public address), a dry run, ten reloads in a row (counter, TSC rate,
# CONFIG and a different initramfs each time; the old kernel stopped its 15 APs and sent them INIT, the new one
# starts all 16 CPUs again; MCP back each time within 20 s), one reload under memory pressure (the new kernel gets
# all memory back) and one refused for lack of memory (the old kernel goes on). The bundles come from a host HTTP
# server in the RB port range (guest: http://10.0.2.2:<port>/<k>/KERNEL + INITRD). End lines are read from the
# serial log, the late ones (CONFIG, TSC re-check) from the new kernel's log over MCP (RB-fl).
# Ports: BITSOS_PORT_LO defaults to RB's range (23000); outside the suite another lane passes a port of its own
# range with BITSOS_LANE (e.g. BITSOS_LANE=desk BITSOS_PORT_LO=20000, BITSOS_LANE=mm BITSOS_PORT_LO=21800).
set -eu
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/rb-20-reload}"
mkdir -p "$RUN_DIR"
RUN_DIR="$(realpath "$RUN_DIR")"
HERE="$(pwd)/host/tests/rb"
LO="${BITSOS_PORT_LO:-23000}"
MCP_FWD=$((LO + 100))
HTTP=$((LO + 110))
TOKEN="rb5a-scenario-token-91c2"
LOG="$RUN_DIR/serial.log"
NAME=rb-reload
SMP="${RB_SMP:-16}"

# Needs the desk's kernel lines (plan/notes/rb-RB-5a.md, change request 1): without kexec::set_boot_info the
# kernel cannot hand its BootInfo on, without net::init MCP is not reachable from the host.
if ! grep -q "kexec::set_boot_info" kernel/src/main.rs || ! grep -q "net::init" kernel/src/main.rs; then
    echo "SKIP: kernel/src/main.rs lacks kexec::set_boot_info or net::init (desk change request 1 of RB-5a / NW-3)"
    exit 0
fi

# The MCP log parsing first (seconds, no QEMU); 15-reload-parser.sh runs the same in the plain gate.
python3 -I "$HERE/reload_scenario.py" --selftest > "$RUN_DIR/selftest.out" || { cat "$RUN_DIR/selftest.out"; exit 1; }

cargo xtask image --name "$NAME" --config-extra "token=$TOKEN;mcp_port=8765;loglevel=info;rb_squeeze_mb=256;rb_choke_mb=10"
BUNDLES="$RUN_DIR/bundles"
rm -rf "$BUNDLES"
mkdir -p "$BUNDLES/base"
cp "target/xtask/esp-$NAME/BITSOS2/SYS/1/KERNEL" "target/xtask/esp-$NAME/BITSOS2/SYS/1/INITRD" "$BUNDLES/base/"

python3 -I -m http.server --bind 127.0.0.1 --directory "$BUNDLES" "$HTTP" > "$RUN_DIR/http.log" 2>&1 &
HPID=$!
rm -f "$LOG"
cargo xtask qemu --name "$NAME" --smp "$SMP" --net igb --hostfwd "$MCP_FWD:8765" --timeout 540 --serial-log "$LOG" > "$RUN_DIR/qemu.out" 2>&1 &
QPID=$!
cleanup() {
    # xtask's own QEMU child goes with it; stop it by the PID xtask printed ("qemu pid N") if it outlived xtask.
    # Not by name: the name carries the lane (v2-<lane>-<name>), and a run under another lane left its QEMU
    # holding the forwarded port (desk, 20.10.).
    local qemu
    qemu=$(sed -n 's/^xtask: qemu pid \([0-9]*\).*/\1/p' "$RUN_DIR/qemu.out" 2>/dev/null | head -1)
    for p in "$QPID" "$HPID" $qemu; do
        if kill -0 "$p" 2>/dev/null; then kill "$p" 2>/dev/null || true; wait "$p" 2>/dev/null || true; fi
    done
}
trap cleanup EXIT

python3 -I "$HERE/reload_scenario.py" "$MCP_FWD" "$TOKEN" "$LOG" "$BUNDLES" "$HTTP" "$SMP"
grep -a "kexec \(handover\|reload\.\(jump\|refused\)\)" "$LOG" | tail -30
