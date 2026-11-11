#!/bin/bash
# slow
# GX-8c with the gpu build of bitscomp (CONFIG bitscomp=gpu|gles, the build and Mesa tree of wy/tools/gpu-image.sh): whatever
# the boot's bring-up did, the compositor comes up, hands over and draws. 10-boot.sh has the same boots on the rescue build.
#  1. gpu-up: the node stands (drm=fake) before the desktop starts: the gpu build, Pixman.
#  2. gles-up: the same, drawing with GLES (llvmpipe, nothing of the fake node).
#  3. gpu-fail: a stage fails and there is no render node at all (drm=off): the gpu build, Pixman.
#  4. gles-fail: the same with GLES.
#  5. gpu-hang: the chain outlives the bound, no node: the gpu build starts at the bound, the failure comes behind it.
# Red when the hand-over falls back to the rescue build: a chip that failed must not cost the requested build. The Mesa
# tree is needed (gpu-image.sh makes it; if it cannot, red). bitscomp's drawing is read from `ctl stats`, no timing is judged.
# In each boot, over MCP, gx.hw.boot_report (whose class is what brings this script into the gate).
set -eu
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/gx-15-boot-gpu}"
mkdir -p "$RUN_DIR"
RUN_DIR="$(realpath "$RUN_DIR")"
LO="${BITSOS_PORT_LO:-27000}"
FWD=$((LO + 115))
TOKEN="gx8c-gpu-scenario-token-71c4"
NAME="${BITSOS_LANE:-gx}-boot8g"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$PWD/target}"
TARGET="$CARGO_TARGET_DIR"
TF="$RUN_DIR/token"
printf '%s\n' "$TOKEN" > "$TF"
export WY_GPU=1

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

MODE=image
LOG=/dev/null
red() { echo "RED ($MODE): $*"; [ -f "$LOG" ] && grep -aE " gx boot\.| desktop\.| PANIC" "$LOG" | tail -20; exit 1; }
need() { grep -aq -- "$1" "$LOG" || red "no '$1' in the serial log"; }
none() { if grep -aq -- "$1" "$LOG"; then red "'$1' in the serial log"; fi; }
line() { grep -an -- "$1" "$LOG" | head -1 | cut -d: -f1; }
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

cargo xtask image --name "$NAME" --config-extra "token=$TOKEN;mcp_port=8765;loglevel=info;desktop=on;ktest_run=gx/15-boot-gpu$(bash host/tests/wy/tools/gpu-image.sh config)" > "$RUN_DIR/image.out" 2>&1
BASE="$TARGET/xtask/esp-$NAME/BITSOS2/CONFIG.TXT"
IMG="$TARGET/xtask/$NAME.img"
bash host/tests/wy/tools/gpu-image.sh image "$NAME" > "$RUN_DIR/gpu-image.out" 2>&1 || { tail -20 "$RUN_DIR/gpu-image.out"; red "no gpu build or Mesa tree"; }
[ -f "$IMG" ] && [ -f "$BASE" ] || red "no image $IMG with $BASE"

boot() {
    MODE="$1"
    LOG="$RUN_DIR/serial-$MODE.log"
    { grep -av '^bitscomp=' "$BASE"; printf '%s\n' "${@:2}"; } > "$RUN_DIR/config-$MODE"
    mcopy -o -i "$IMG" "$RUN_DIR/config-$MODE" ::/BITSOS2/CONFIG.TXT
    stop_qemu
    rm -f "$LOG"
    cargo xtask qemu --name "$NAME" --mem 4G --net virtio --hostfwd "$FWD:8765" --timeout 400 --serial-log "$LOG" -- \
        -smp 4 -machine i8042=off -device qemu-xhci,id=xb -device usb-kbd,bus=xb.0,port=1,id=kbd1 > "$RUN_DIR/qemu-$MODE.out" 2>&1 &
    QPID=$!
    wait_line "console desktop.up program=/bin/bitscomp" 240
}
# The compositor at the end of a boot: this build and renderer, and it has drawn. The hand-over's check runs first and may
# read the tree from the stick for a while (GLES: up to 30 s), so no answer yet is no verdict until the wait is over.
draws() {
    local s=0 a=""
    until printf '%s\n' "$a" | grep -q "frames=[1-9].* build=gpu renderer=$1"; do
        s=$((s + 1))
        [ "$s" -le 150 ] || red "bitscomp build=gpu renderer=$1 did not draw; last answer: $a"
        kill -0 "$QPID" 2>/dev/null || red "QEMU ended"
        case "$a" in *"build=rescue"*) red "the hand-over fell back to the rescue build: $a" ;; esac
        sleep 2
        a="$(call run '{"path":"/bin/bitscomp","args":["ctl","stats"],"env":{"PATH":"/bin","XDG_RUNTIME_DIR":"/run/user/1000"},"timeout_ms":20000}' || true)"
    done
    printf '%s\n' "$a" > "$RUN_DIR/stats-$MODE.txt"
    none "PANIC"
    none "lock.violation"
    echo "ok   $MODE: build=gpu renderer=$1 draws"
}
# until=boot's head, the row on /proc/bitsos/boot, and whether a render node is there.
state() {
    local a
    a="$(call gx_bringup '{"until":"boot"}')" || red "until=boot failed: $a"
    printf '%s\n' "$a" | grep -q -- "$1" || red "until=boot: no '$1' in: $a"
    a="$(call run '{"path":"/bin/sh","args":["-c","cat /proc/bitsos/boot; echo dri:; ls /dev/dri 2>&1; true"],"env":{"PATH":"/bin"}}')" || red "run failed: $a"
    printf '%s\n' "$a" > "$RUN_DIR/proc-$MODE.txt"
    grep -q -- "$2" "$RUN_DIR/proc-$MODE.txt" || red "/proc/bitsos/boot: no '$2' in: $a"
    if [ "$3" = node ]; then
        grep -q "renderD128" "$RUN_DIR/proc-$MODE.txt" || red "no render node: $a"
    elif grep -q "renderD" "$RUN_DIR/proc-$MODE.txt"; then
        red "a render node, and there must be none: $a"
    fi
    a="$(call ktest '{"filter":"gx.hw.boot_report"}')" || red "ktest failed: $a"
    case "$a" in *"ok gx.hw.boot_report"*) ;; *) red "gx.hw.boot_report: $a" ;; esac
    # The ledger counts what a run's logs say.
    printf '%s\n' "$a" | sed -n 's/^ok gx.hw.boot_report/KTEST ok gx.hw.boot_report/p' > "$RUN_DIR/ktest.txt"
    echo "ok   $MODE: until=boot, the row '$2', render node: $3, gx.hw.boot_report"
}

for r in gpu:pixman gles:gles; do
    boot "${r%%:*}-up" drm=fake gpu=on gpu_boot_fake=up "bitscomp=${r%%:*}"
    need "gx boot.up minor=128"
    before "gx boot.up minor=128" "console desktop.start program=/bin/bitscomp"
    draws "${r##*:}"
    state "^boot: up on renderD128" "gpu bereit, renderD128" node
    cleanup

    boot "${r%%:*}-fail" drm=off gpu=on gpu_boot_fake=fail "bitscomp=${r%%:*}"
    need "gx boot.failed stage=sdma stop=done"
    before "gx boot.failed" "console desktop.start program=/bin/bitscomp"
    draws "${r##*:}"
    state "^gx_bringup: FAIL" "gestoppt bei sdma, ohne gpu" no-node
    cleanup
done

boot gpu-hang drm=off gpu=on gpu_boot_fake=hang bitscomp=gpu
need "console desktop.hold_over"
before "console desktop.hold_over" "console desktop.start program=/bin/bitscomp"
draws pixman
wait_line "gx boot.failed stage=late stop=done" 30
before "console desktop.start program=/bin/bitscomp" "gx boot.failed stage=late"
state "^gx_bringup: FAIL" "gestoppt bei late, ohne gpu" no-node
cleanup

echo "gx 15-boot-gpu: green"
