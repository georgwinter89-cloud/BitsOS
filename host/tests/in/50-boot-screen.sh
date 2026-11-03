#!/bin/bash
# slow
# DS-4: the boot screen on QEMU's 2560x1440 GOP screen, eight boots (QEMU's desktop=auto means off, so each boot
# that wants the screen says desktop=on):
#  1. fail (desktop_program=/bin/sleep 15: a "desktop" that ends after 15 s without taking the screen; long enough
#     for a loaded host's checker to be in place before the first 1 s retry gap it must see): the boot
#     screen shows early, its picture equals the Rust model drawn from /proc/bitsos/boot pixel for pixel (also with
#     the failing desktop step), three failed starts end it with the text console and a message.
#  2. key: a key pressed while the boot screen is up shows the text console at once.
#  3. up: /bin/bitscomp starts by itself and takes the screen (bootscreen.end reason=handover).
#  4. off (the default under QEMU): no boot screen, the step list still in /proc/bitsos/boot; today's console.
#  5. emergency (ktest in.boot.emergency while the boot screen is up): the panic path's console write ends the boot
#     screen and paints the text console by itself (a real panic exits QEMU before a screendump could see it).
#  6. missing (desktop_program=/nonexistent): a program that cannot be spawned counts as a failed start; three
#     of them (1 s, 2 s apart) end in the text console with the message.
#  7. errlvl (IN-9, loglevel=error as on Georg's Mini-PC): every step settles and bitscomp takes the screen,
#     although no Info line reaches the ring (klog's watch feeds the list); none of them is on serial.
#  8. warnlvl: the same at loglevel=warn.
# BITSOS_REQUIRE_WATCH=1 (the desk's gate once IN-9's klog/kernel change requests are in): boots 7/8 and the
# in.boot.watch ktest must run, not skip.
# Before the boots: the font rasteriser is no normal dependency of the console (kernel) crate.
# Prints the kernel-ready times of boots 4 and 1 (numbers only; timing is judged on the Mini-PC).
set -eu
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/in-50-boot-screen}"
mkdir -p "$RUN_DIR"
RUN_DIR="$(realpath "$RUN_DIR")"
HERE="$(pwd)/host/tests/in"
LO="${BITSOS_PORT_LO:-26000}"
FWD=$((LO + 108))
TOKEN="ds4-scenario-token-7e21"
QMP="$RUN_DIR/qmp.sock"
NAME=in-boot

qemu_pid() {
    local pf="target/xtask/qemu-$NAME.pid" pid
    [ -f "$pf" ] || return 0
    pid="$(head -1 "$pf" 2>/dev/null || true)"
    case "$pid" in ''|*[!0-9]*) return 0 ;; esac
    if tr '\0' ' ' < "/proc/$pid/cmdline" 2>/dev/null | grep -Eq -- "-name v2-[A-Za-z0-9]+-$NAME( |$)"; then echo "$pid"; fi
}
stop_qemu() {
    local pid
    pid="$(qemu_pid)"
    if [ -n "$pid" ]; then
        kill "$pid" 2>/dev/null || true
        for _ in $(seq 1 50); do kill -0 "$pid" 2>/dev/null || break; sleep 0.1; done
        if kill -0 "$pid" 2>/dev/null; then kill -9 "$pid" 2>/dev/null || true; fi
    fi
    rm -f "target/xtask/qemu-$NAME.pid"
}
QPID=""
cleanup() {
    if [ -n "$QPID" ] && kill -0 "$QPID" 2>/dev/null; then kill "$QPID" 2>/dev/null || true; fi
    stop_qemu
    if [ -n "$QPID" ]; then wait "$QPID" 2>/dev/null || true; fi
}
trap cleanup EXIT

boot() {
    local mode="$1" extra="$2" opts="${3:-}" level="${4:-info}"
    LOG="$RUN_DIR/serial-$mode.log"
    cargo xtask image --name "$NAME" $opts --config-extra "token=$TOKEN;mcp_port=8765;loglevel=$level$extra"
    stop_qemu
    rm -f "$LOG" "$QMP"
    cargo xtask qemu --name "$NAME" --net virtio --hostfwd "$FWD:8765" --timeout 400 --serial-log "$LOG" -- \
        -machine i8042=off -device qemu-xhci,id=xb -device usb-kbd,bus=xb.0,port=1,id=kbd1 \
        -qmp "unix:$QMP,server=on,wait=off" > "$RUN_DIR/qemu-$mode.out" 2>&1 &
    QPID=$!
    python3 -I "$HERE/boot_check.py" "$mode" "$FWD" "$TOKEN" "$QMP" "$RUN_DIR" "$(pwd)" "$LOG"
    cleanup
    QPID=""
    if grep -aq "lock.violation" "$LOG"; then echo "RED: lock order broken ($mode)"; grep -a "lock.violation" "$LOG" | head; exit 1; fi
    if grep -aq "PANIC" "$LOG"; then echo "RED: a panic ($mode)"; exit 1; fi
    if grep -aq "$TOKEN" "$LOG"; then echo "RED: the token appears in the log (R8i, $mode)"; exit 1; fi
}

if cargo tree -p console -e normal --offline 2>/dev/null | grep -Eq "fontdue|ttf-parser"; then
    echo "RED: fontdue/ttf-parser are normal dependencies of console (the kernel); they belong in [build-dependencies]"; exit 1
fi

need_line() {
    grep -aq -- "$2" "$RUN_DIR/serial-$1.log" || { echo "RED ($1): no '$2' in the serial log"; exit 1; }
}

boot fail ";desktop=on;desktop_program=/bin/sleep 15"
need_line fail "console bootscreen.begin width=2560 height=1440"
need_line fail "console bootscreen.drawn us="
need_line fail "console desktop.failed program=/bin/sleep attempt=1 reason=ended"
need_line fail "console desktop.given_up program=/bin/sleep attempts=3 reason=ended"
need_line fail "console bootscreen.end reason=desktop-failed"

boot key ";desktop=on;desktop_program=/bin/sleep 30"
need_line key "console bootscreen.end reason=key"

boot up ";desktop=on"
need_line up "console desktop.up program=/bin/bitscomp"
need_line up "console bootscreen.end reason=handover"

boot off ""
# No desktop= key at all: QEMU's auto resolves to off (every other lane's QEMU test keeps the text console).
need_line off "console desktop.mode on=false"
grep -a "console desktop.mode on=false" "$RUN_DIR/serial-off.log" | grep -q "hypervisor" || { echo "RED (off): desktop=auto did not resolve to off for the hypervisor"; exit 1; }
need_line off "console desktop.off"
if grep -aq "console bootscreen.begin" "$RUN_DIR/serial-off.log"; then echo "RED (off): a boot screen without desktop=on"; exit 1; fi

boot missing ";desktop=on;desktop_program=/nonexistent"
need_line missing "console desktop.failed program=/nonexistent attempt=1 reason="
grep -aEq "console desktop.given_up program=/nonexistent attempts=3 reason=(not_found|refused)" "$RUN_DIR/serial-missing.log" || { echo "RED (missing): no given_up after 3 spawn errors"; exit 1; }
need_line missing "console bootscreen.end reason=desktop-failed"

boot emergency ";desktop=on;ktest=in.boot" --ktest
need_line emergency "KTEST ok in.boot.emergency"
want_done="KTEST done passed=4 failed=0 skipped=0|KTEST done passed=3 failed=0 skipped=1"
if [ "${BITSOS_REQUIRE_WATCH:-0}" = 1 ]; then want_done="KTEST done passed=4 failed=0 skipped=0"; fi
grep -aEq "$want_done" "$RUN_DIR/serial-emergency.log" \
    || { echo "RED (emergency): want the four in.boot ktests passed (in.boot.watch may skip only without klog's watch)"; exit 1; }

# No extra lines below the level: the step events (Info) never reach the ring, so serial (which prints the ring)
# has none after klog applied the level (the early ring's own lines come before it).
for lvl in error warn; do
    if [ "$lvl" = error ]; then mode=errlvl; else mode=warnlvl; fi
    boot "$mode" ";desktop=on" "" "$lvl"
    for l in "I console desktop.up" "I kernel ready" "I ul init.start" "I console bootscreen.end"; do
        if grep -aq "$l" "$RUN_DIR/serial-$mode.log"; then echo "RED ($mode): '$l' reached the ring at loglevel=$lvl"; exit 1; fi
    done
done

ready_ms() { grep -a "I kernel ready" "$RUN_DIR/serial-$1.log" | head -1 | sed -E 's/^\[ *([0-9]+)\.([0-9]{3}).*/\1\2/' | sed 's/^0*//'; }
echo "kernel ready (QEMU, numbers only): desktop=off $(ready_ms off) ms, boot screen on $(ready_ms fail) ms, $(ready_ms up) ms"
grep -a "console bootscreen.drawn" "$RUN_DIR"/serial-*.log | sed 's/^/  /'
echo "in boot screen: green (early picture = model, failing step drawn, 3 failed starts -> text console with message, key -> text console, bitscomp takes the screen, desktop=off keeps the console, loglevel=error and warn start the desktop)"
