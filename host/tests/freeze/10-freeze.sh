#!/bin/bash
# slow
# SC-7c freeze suite (B4 "the desktop never freezes", V2-LANES SC-7 row): a normal boot with -smp 16 and MCP from
# the host. freeze_check.py starts the program in front (abi-freeze probe, made Interactive with sched_focus), then
# 64 busy threads (stress), a hung program holding a PI futex forever (abi-freeze hold), FS-12's in-kernel disk hog
# (ktest block.ram.hog on a slow RamDisk, looped) and a user-space write + fsync hog (abi-freeze hog; on /tmp, as
# MCP programs are uid 1000 and may not write a FAT disk: page cache, no device I/O); meanwhile send_keys injects
# key presses. Green when the probe's threads are in class interactive under the load, every deadline of its
# clock thread and every key event its reader sees are within 500 ms, no press is lost, every busy thread counted,
# the hogs made progress and stop ends the hung program. Functional only: the QEMU numbers go to numbers.txt; the
# real numbers come from the Mini-PC (plan/notes/sc-SC-7c.md). Before the desk applied the SC-7a/7b integration
# the run is red at the first missing piece; FREEZE_EXPECT_B4=0 runs every check anyway and ends red by name.
set -eu
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
# The folder is not a lane code, so the runner hands it the default (desk) range: the suite is SC's (V2-LANES
# SC-7) and uses the top of SC's range, which SC's own scripts leave free.
export BITSOS_LANE=sc
LO=22000
FWD=$((LO + 401))
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/freeze-10-freeze}"
mkdir -p "$RUN_DIR"
RUN_DIR="$(realpath "$RUN_DIR")"
HERE="$(pwd)/host/tests/freeze"
TOKEN="sc7c-freeze-token-51e9"
LOG="$RUN_DIR/serial.log"
NAME=sc-freeze

# PID of the QEMU this script started (checked against its -name, never another process).
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

if [ ! -f user/sysroot/out/sysroot/usr/lib/libc.a ] && [ -z "${V2_SYSROOT:-}" ]; then
    echo "RED: no musl sysroot (user/sysroot/out): abi-freeze would be missing from the image; run cargo xtask abi-diff once"
    exit 1
fi
cargo xtask image --name "$NAME" --config-extra "token=$TOKEN;mcp_port=8765;loglevel=info" > "$RUN_DIR/image.txt" 2>&1 || { tail -20 "$RUN_DIR/image.txt"; exit 1; }
stop_qemu
rm -f "$LOG"
cargo xtask qemu --name "$NAME" --net virtio --hostfwd "$FWD:8765" --timeout 540 --serial-log "$LOG" -- \
    -smp 16 > "$RUN_DIR/qemu.out" 2>&1 &
QPID=$!

# The log checks run after a red checker too: a panic or a lock violation is the likeliest cause of one.
pyfail=0
python3 -I "$HERE/freeze_check.py" "$FWD" "$TOKEN" "$RUN_DIR" || pyfail=1
fail=0
if grep -aq "lock.violation" "$LOG"; then echo "RED: lock order broken"; grep -a "lock.violation" "$LOG" | head; fail=1; fi
if grep -aq "PANIC\|reap.stalled\|run.kill_given_up\|pi.refresh_fallback\|E sched " "$LOG"; then
    echo "RED: a failure line in the log"; grep -a "PANIC\|reap.stalled\|run.kill_given_up\|pi.refresh_fallback\|E sched " "$LOG" | head; fail=1
fi
if grep -aq "$TOKEN" "$LOG"; then echo "RED: the token appears in the log (R8i)"; fail=1; fi
grep -ac "sched interactive.late" "$LOG" | sed 's/^/interactive.late events in the serial log: /' || true
[ "$((fail | pyfail))" = 0 ] && echo "freeze suite: green (64 busy threads, a hung PI-futex holder and disk hogs; the Interactive probe kept its deadlines and read every key event)"
exit $((fail | pyfail))
