#!/bin/bash
# GM-6w: the desktop's power request over init's /run/bitsos/power.sock in QEMU, one normal boot:
#  peers that do not hold fb0's compositor grant are refused (user 1000 from MCP's run, root typed on the keyboard
#  with plain words), each with an init log line; the grant holder without "input":"physical" is refused too; then `init --power-holder poweroff` typed on the (QEMU USB)
#  keyboard takes the grant, sends {"op":"poweroff","input":"physical"} and the guest powers off (QEMU ends).
set -eu
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/ul-60-power-sock}"
mkdir -p "$RUN_DIR"
RUN_DIR="$(realpath "$RUN_DIR")"
HERE="$(pwd)/host/tests/ul"
LO="${BITSOS_PORT_LO:-28500}"
FWD=$((LO + 80))
TOKEN="gm6w-power-sock-token-71c4"
QMP="$RUN_DIR/qmp.sock"
LOG="$RUN_DIR/serial.log"
NAME="${BITSOS_LANE:-UL}-power-sock"
fail() { echo "RED: $*"; exit 1; }

QPID=""
cleanup() {
    if [ -n "$QPID" ] && kill -0 "$QPID" 2>/dev/null; then kill "$QPID" 2>/dev/null || true; fi
    local vm
    vm=$(grep -aoE 'qemu pid [0-9]+' "$RUN_DIR"/qemu.out 2>/dev/null | tail -1 | awk '{print $3}' || true)
    if [ -n "$vm" ] && kill -0 "$vm" 2>/dev/null; then kill "$vm" 2>/dev/null || true; fi
    if [ -n "$QPID" ]; then wait "$QPID" 2>/dev/null || true; fi
}
trap cleanup EXIT

cargo xtask image --name "$NAME" --config-extra "token=$TOKEN;mcp_port=8765;loglevel=info" > "$RUN_DIR/image.txt" 2>&1 \
    || fail "the image did not build (see $RUN_DIR/image.txt)"
rm -f "$LOG" "$QMP"
cargo xtask qemu --name "$NAME" --net virtio --hostfwd "$FWD:8765" --timeout 400 --serial-log "$LOG" -- \
    -machine i8042=off -device qemu-xhci,id=xb -device usb-kbd,bus=xb.0,port=1,id=kbd1 \
    -qmp "unix:$QMP,server=on,wait=off" > "$RUN_DIR/qemu.out" 2>&1 &
QPID=$!
python3 -I "$HERE/power_sock_check.py" "$FWD" "$TOKEN" "$QMP" || fail "the check failed"
for _ in $(seq 1 60); do kill -0 "$QPID" 2>/dev/null || break; sleep 1; done
kill -0 "$QPID" 2>/dev/null && fail "QEMU still runs after the power-off"
cleanup
QPID=""

grep -aq "PANIC" "$LOG" && fail "a panic"
grep -aq "$TOKEN" "$LOG" && fail "the token appears in the log (R8i)"
grep -aq "init: power socket refused: user 1000 asks only through /boot/BITSOS2/bin/bitscomp" "$LOG" \
    || fail "no refusal event for user 1000 from MCP's run"
grep -aq "init: power socket refused power off: no process holds the compositor grant" "$LOG" \
    || fail "no refusal event for the root peer without the compositor grant"
grep -aq "init: power socket refused power off: the request does not say that real input asked for it" "$LOG" \
    || fail "no refusal event for the grant holder without the real-input statement"
grep -aq "compositor.granted" "$LOG" || fail "the holder peer did not get the compositor grant"
grep -aq "init: the system is going down for power off (power socket, pid [0-9]* uid 0 /bin/init, holds the compositor grant" "$LOG" \
    || fail "init did not go down for the compositor's request"
grep -aq "ul reboot.poweroff" "$LOG" || fail "no ul reboot.poweroff event from reboot(2)"
grep -aq "power off.requested" "$LOG" || fail "no power off.requested (the clean power-off path)"
grep -aq "power-off did not happen" "$LOG" && fail "S5 did not switch QEMU off"
echo "ul power-sock: green (non-holder peers refused with events, the compositor's JSON request powers the guest off)"
