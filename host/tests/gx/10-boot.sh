#!/bin/bash
# slow
# GX-8c in QEMU, which has no chip: the GPU's bring-up runs at every boot (no key keeps it down). Up: the desktop's start
# follows its end. Failed, past its bound, or no GPU at all: no desktop, one plain line on the text console, MCP answers.
# gpu_boot_fake= (test only) plays the bring-up (only under a hypervisor); the chain itself is the gx.hw.boot_* ktests'.
#  1. up: passes on the fake's node (drm=fake): the desktop's start comes after the end, the row says ready; again
#     after a Reload.
#  2. fail: a stage fails: stopped, reason in the log, no desktop: the text console's one line.
#  3. hang: the chain outlives the bound: at the bound no desktop, the line; the failure comes behind it.
#  4. nochip: no script: no AMD function, nothing is held, no desktop either (the line says so). Then, over MCP,
#     gx.hw.boot_thread: the thread and the hold on the wall clock.
# In boots 1-3, over MCP, gx.hw.boot_report: what the run left (the desktop told, a render node only when up).
# One image, its CONFIG.TXT replaced before each boot: an image costs minutes while the gate runs its chains (600 s for a
# script), and this script has four boots.
# In each boot gx_bringup until=boot answers how it went, /proc/bitsos/boot has the row, MCP answers.
set -eu
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/gx-10-boot}"
mkdir -p "$RUN_DIR"
RUN_DIR="$(realpath "$RUN_DIR")"
LO="${BITSOS_PORT_LO:-27000}"
FWD=$((LO + 110))
TOKEN="gx8c-scenario-token-5b17"
NAME="${BITSOS_LANE:-gx}-boot8c"
TARGET="${CARGO_TARGET_DIR:-target}"
TF="$RUN_DIR/token"
printf '%s\n' "$TOKEN" > "$TF"

qemu_pid() {
    local pf="$TARGET/xtask/qemu-$NAME.pid" pid
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
    rm -f "$TARGET/xtask/qemu-$NAME.pid"
}
QPID=""
cleanup() {
    if [ -n "$QPID" ] && kill -0 "$QPID" 2>/dev/null; then kill "$QPID" 2>/dev/null || true; fi
    stop_qemu
    if [ -n "$QPID" ]; then wait "$QPID" 2>/dev/null || true; fi
    QPID=""
}
trap cleanup EXIT

red() { echo "RED ($MODE): $*"; [ -f "$LOG" ] && grep -aE " gx boot\.| desktop\.| PANIC" "$LOG" | tail -20; exit 1; }
need() { grep -aq -- "$1" "$LOG" || red "no '$1' in the serial log"; }
none() { if grep -aq -- "$1" "$LOG"; then red "'$1' in the serial log"; fi; }
line() { grep -an -- "$1" "$LOG" | head -1 | cut -d: -f1; }
# Line of $1 comes before line of $2.
before() {
    local a b
    a="$(line "$1")"; b="$(line "$2")"
    [ -n "$a" ] && [ -n "$b" ] && [ "$a" -lt "$b" ] || red "'$1' (line $a) must come before '$2' (line $b)"
}
call() { node host/mcp/mcp-call.mjs "127.0.0.1:$FWD" "$1" "$2" --token-file "$TF" 2>&1; }
wait_line() {
    local s=0
    until grep -aq -- "$1" "$LOG" 2>/dev/null; do
        s=$((s + 1))
        [ "$s" -le "$2" ] || red "no '$1' within $2 s"
        kill -0 "$QPID" 2>/dev/null || red "QEMU ended"
        sleep 1
    done
}

cargo xtask image --name "$NAME" --config-extra "token=$TOKEN;mcp_port=8765;loglevel=info;ktest_run=gx/10-boot" > "$RUN_DIR/image.out" 2>&1
IMG="$TARGET/xtask/$NAME.img"
BASE="$TARGET/xtask/esp-$NAME/BITSOS2/CONFIG.TXT"
[ -f "$IMG" ] && [ -f "$BASE" ] || { echo "RED: no image $IMG with $BASE"; exit 1; }

START="console desktop.start program=/boot/BITSOS2/bin/bitscomp"
NO_GPU="console desktop.no_gpu"
# boot <mode> <end> <lines of config>: up to <end> (the desktop's start, or its no_gpu line) and MCP's answer.
boot() {
    MODE="$1"
    LOG="$RUN_DIR/serial-$MODE.log"
    { cat "$BASE"; printf '%s\n' "${@:3}"; } > "$RUN_DIR/config-$MODE"
    mcopy -o -i "$IMG" "$RUN_DIR/config-$MODE" ::/BITSOS2/CONFIG.TXT
    stop_qemu
    rm -f "$LOG"
    cargo xtask qemu --name "$NAME" --mem 4G --net virtio --hostfwd "$FWD:8765" --timeout 400 --serial-log "$LOG" -- \
        -machine i8042=off -device qemu-xhci,id=xb -device usb-kbd,bus=xb.0,port=1,id=kbd1 > "$RUN_DIR/qemu-$MODE.out" 2>&1 &
    QPID=$!
    wait_line "$2" 240
    none "PANIC"
    none "lock.violation"
    echo "ok   $MODE: '$2'"
}
# No desktop: the one line on the text console (the event carries it), and nothing started.
text_console() {
    need "$NO_GPU reason=$1"
    need "$2"
    none "$START"
    echo "ok   $MODE: no desktop, the line '$2'"
}
# A ktest of class Run("gx/10-boot") over MCP; the ledger counts what a run's logs say.
ktest() {
    local a
    a="$(call ktest "{\"filter\":\"$1\"}")" || red "ktest failed: $a"
    case "$a" in *"ok $1"*) ;; *) red "$1: $a" ;; esac
    printf '%s\n' "$a" | sed -n "s/^ok $1/KTEST ok $1/p" | head -1 >> "$RUN_DIR/ktest.txt"
    echo "ok   $MODE: $1"
}
# How much the machine answers at the end of a boot.
answers() {
    local a
    a="$(call version '{}')" || red "MCP did not answer: $a"
    a="$(call gx_bringup '{"until":"boot"}')" || red "until=boot failed: $a"
    printf '%s\n' "$a" > "$RUN_DIR/answer-$MODE.txt"
    grep -q -- "$1" "$RUN_DIR/answer-$MODE.txt" || red "until=boot: no '$1' in: $a"
    a="$(call run '{"path":"/bin/sh","args":["-c","cat /proc/bitsos/boot; ls /dev/dri 2>\u00261; true"],"env":{"PATH":"/bin"}}')" || red "run failed: $a"
    printf '%s\n' "$a" > "$RUN_DIR/proc-$MODE.txt"
    grep -q -- "$2" "$RUN_DIR/proc-$MODE.txt" || red "/proc/bitsos/boot: no '$2' in: $a"
    grep -q -- "$3" "$RUN_DIR/proc-$MODE.txt" || red "/dev/dri: no '$3' in: $a"
    echo "ok   $MODE: MCP, until=boot, the row '$2'"
}

rm -f "$RUN_DIR/ktest.txt"

# 1. Up.
boot up "$START" drm=fake gpu_boot_fake=up
need "gx boot.start bound_s=30"
need "gx boot.up minor=128"
# hold_end only when the thread looked before the end: the order below is the rule.
before "gx boot.up minor=128" "$START"
none "gx boot.failed"
none "$NO_GPU"
answers "^boot: up on renderD128" "gpu bereit, renderD128" "renderD128"
ktest gx.hw.boot_report
# The new kernel of a Reload does it again by itself, with no `again`.
node host/mcp/reload.mjs "127.0.0.1:$FWD" --name "$NAME" --no-build --bind 127.0.0.1 --port $((LO + 111)) --token-file "$TF" > "$RUN_DIR/reload.out" 2>&1 || { cat "$RUN_DIR/reload.out"; red "the Reload failed"; }
s=0
until [ "$(grep -ac "$START" "$LOG")" -ge 2 ]; do
    s=$((s + 1))
    [ "$s" -le 120 ] || red "no desktop start after the Reload"
    sleep 1
done
[ "$(grep -ac "gx boot.up minor=128" "$LOG")" -eq 2 ] || red "the Reload's kernel did not bring it up"
none "PANIC"
answers "^boot: up on renderD128" "gpu bereit, renderD128" "renderD128"
echo "ok   up: again after a Reload"
cleanup

# 2. A stage fails: no desktop, the line.
boot fail "$NO_GPU" gpu_boot_fake=fail
need "gx boot.failed stage=sdma stop=done"
before "gx boot.failed" "$NO_GPU"
none "gx boot.up"
text_console gpu "Die Grafik ist nicht hochgekommen (sdma)"
answers "^gx_bringup: FAIL" "gestoppt bei sdma, textkonsole" "."
grep -q "script: stopped as until=stop does" "$RUN_DIR/answer-fail.txt" || red "no stop in the answer"
ktest gx.hw.boot_report
cleanup

# 3. The chain outlives the bound: at the bound no desktop, the line; the failure comes behind it.
boot hang "$NO_GPU" gpu_boot_fake=hang
need "console desktop.held"
text_console late "Die Grafik ist nicht rechtzeitig hochgekommen"
wait_line "gx boot.failed stage=late stop=done" 30
before "$NO_GPU" "gx boot.failed stage=late"
answers "^gx_bringup: FAIL" "gestoppt bei late, textkonsole" "."
ktest gx.hw.boot_report
cleanup

# 4. A machine without the chip: nothing is held, no desktop (the line says why), the tool answers as without a chip.
# drm=fake (test only) gives gx.hw.boot_thread's stand-in a node.
boot nochip "$NO_GPU" drm=fake
need "gx boot.no_chip"
none "gx boot.start"
none "console desktop.held"
none "gx boot.failed"
text_console gpu "Dieser Rechner hat keine Grafik, die BitsOS hochf"
answers "no AMD GPU function on this machine" "keine amd-gpu" "renderD128"
a="$(call gx_bringup '{"until":"stop"}' || true)"
case "$a" in *"no AMD GPU function"*) ;; *) red "until=stop without a chip: $a" ;; esac
ktest gx.hw.boot_thread
cleanup

echo "gx 10-boot: green"
