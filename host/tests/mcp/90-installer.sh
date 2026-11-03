#!/bin/bash
# slow
# MC-9a: the installer bitsinstall behind bitsmcp (CONFIG.TXT mcp=both, mcp_confirmer=test): the kernel gives the
# installer's role only to the boot's /bin/bitsinstall as root (a uid-1000 program, a root copy and a second claim
# are refused), a confirmer that runs another file is refused, without a confirmer every request is refused; the
# test confirmer answers no, yes (the tool runs), silence (timeout), an injected yes (refused); a grant for `pin`
# lets a pin through without a dialog until it expires by itself; every decision is a kernel event; a confirmed
# real reboot answers first and then restarts the machine (installer_check.py). bitsmcp and bitsinstall come from
# init's glue (plan/notes/mc-MC-8a-init.patch, mc-MC-9a-init.patch) or, without it, are typed into the root
# console shell through QEMU's USB keyboard and QMP.
set -eu -o pipefail
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/mc-90-installer}"
mkdir -p "$RUN_DIR"
RUN_DIR="$(realpath "$RUN_DIR")"
HERE="$(pwd)/host/tests/mcp"
# shellcheck source=host/tests/mcp/lib.bash
. "$HERE/lib.bash"
LO="${BITSOS_PORT_LO:-25000}"
KFWD=$((LO + 110))
SFWD=$((LO + 111))
TOKEN="mc9a-scenario-token-7e30"
NAME=mc-installer

QPID=""
trap '[ -n "$QPID" ] && stop_run "$QPID" "$NAME"' EXIT

cargo xtask image --name "$NAME" --config-extra "token=$TOKEN;mcp_port=8765;mcp=both;mcp_confirmer=test;loglevel=info;data_img_mb=64" | tail -1
img="target/xtask/$NAME.img"
kib=$(( $(stat -c %s "$img") / 1024 + 700 * 1024 ))
rm -f "$img"
mkfs.fat -F 32 -n BITSOS -C "$img" "$kib" > /dev/null
mcopy -s -i "$img" "target/xtask/esp-$NAME/EFI" "target/xtask/esp-$NAME/BITSOS2" ::/
stop_qemu "$NAME"
QMP="$RUN_DIR/$NAME.qmp"
LOG="$RUN_DIR/$NAME.serial"
rm -f "$QMP" "$LOG"
cargo xtask qemu --name "$NAME" --net virtio --hostfwd "$KFWD:8765" --hostfwd "$SFWD:8766" --timeout 600 --serial-log "$LOG" \
    -- -machine i8042=off -device qemu-xhci,id=xb -device usb-kbd,bus=xb.0,port=1,id=kbd1 -qmp "unix:$QMP,server=on,wait=off" > "$RUN_DIR/$NAME.qemu" 2>&1 &
QPID=$!
for _ in $(seq 120); do grep -aq "mcp gate.armed mode=both port=8766" "$LOG" 2>/dev/null && break; sleep 1; done
grep -aq "mcp gate.armed mode=both port=8766" "$LOG" || { echo "RED: no gate.armed event"; exit 1; }
grep -aq "mcp installer.test_confirmer" "$LOG" || { echo "RED: mcp_confirmer=test not seen by the kernel"; exit 1; }
python3 -I "$HERE/installer_check.py" "$KFWD" "$SFWD" "$TOKEN" "$QMP" "$LOG" | tee "$RUN_DIR/check.txt"
# The reboot: the machine starts again (a second gate) or QEMU ends with the reset.
for _ in $(seq 90); do
    [ "$(grep -ac "mcp gate.armed mode=both" "$LOG")" -ge 2 ] && break
    kill -0 "$QPID" 2>/dev/null || break
    sleep 1
done
if [ "$(grep -ac "mcp gate.armed mode=both" "$LOG")" -ge 2 ]; then
    echo "ok: the machine started again after the confirmed reboot"
elif ! kill -0 "$QPID" 2>/dev/null; then
    echo "ok: QEMU ended with the confirmed reboot's reset"
else
    echo "RED: no restart after the confirmed reboot"; tail -20 "$LOG"; exit 1
fi
if grep -aq "$TOKEN" "$LOG"; then echo "RED: the token appears in the log (R8i)"; exit 1; fi
if grep -aq "grants not kept on the stick" "$LOG"; then echo "RED: the grant was not kept on the stick"; exit 1; fi
echo "mc 90-installer: green (role only for the boot's bitsinstall, no session = no, dialog yes/no/timeout/injected, grant until it expires, journal, reboot after its answer)"
