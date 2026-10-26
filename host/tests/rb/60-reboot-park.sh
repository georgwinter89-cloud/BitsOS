#!/bin/bash
# slow
# RB-quiesce: reboot and power-off with busy work on 15 APs (QEMU -smp 16, KVM). The other CPUs are parked before
# the drivers' quiesce (hal::kexec_stub::park_for_shutdown, Reload's stop): the test device's Quiesce
# (kexec quiesce_ktests.rs) logs `rb quiesce.test cpu=0 others_ran=0 lock_ok=true` when it ran on CPU 0 while
# the workers stood still and their lock was free. Runs:
#  1. reboot   (ktest rb.quiesce.qemu_action, rb_quiesce_test=reboot): park, quiesce_all, acpi::power::reboot
#              (it sleeps between reset steps beside the parked CPUs) -> RESET guest-reset, a second kernel start;
#  2. poweroff (rb_quiesce_test=poweroff) -> SHUTDOWN guest-shutdown;
#  3. platform-reboot / 4. platform-poweroff: platform::power::reboot/poweroff (the console's commands);
#  5. MCP: ktest rb.quiesce.load_for_mcp starts the workers, then the MCP tool `reboot`.
# Runs 3-5 check the park only when the desk's glue is in (change requests 1 and 2 of plan/notes/rb-RB-quiesce.md:
# `park_others` in crates/platform/src/power.rs and crates/mcp/src/tools/reboot.rs); without it they check that
# the reset or power-off still happens and say SKIP for the park lines. RB_REQUIRE_GLUE=1 (the desk's acceptance
# run once CR1/CR2 are in) turns those SKIPs into FAILs.
# Ports: RB's range (BITSOS_PORT_LO, default 23000); MCP forward at LO+120.
set -eu
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
RUN="${BITSOS_RUN_DIR:-target/xtask/runs/rb-60-reboot-park}"
mkdir -p "$RUN"
RUN="$(realpath "$RUN")"
HERE="$(pwd)/host/tests/rb"
QMP_EVENTS="$(pwd)/host/tests/cp/qmp_events.py"
LO="${BITSOS_PORT_LO:-23000}"
MCP_FWD=$((LO + 120))
TOKEN="rbq-scenario-token-5e7a"
SMP="${RB_SMP:-16}"
OTHERS=$((SMP - 1))
fail=0
missing_glue() { if [ "${RB_REQUIRE_GLUE:-0}" = 1 ]; then echo "FAIL $1 (RB_REQUIRE_GLUE=1)"; fail=1; else echo "SKIP $1"; fi; }
check() { if grep -aEq -- "$2" "$1"; then echo "ok   $3"; else echo "FAIL $3 (no '$2' in $(basename "$1"))"; fail=1; fi; }

PIDS=()
stop_run() { # qemu.out of the run, xtask pid
    local vm
    vm=$(grep -aoE 'qemu pid [0-9]+' "$1" 2>/dev/null | tail -1 | awk '{print $3}' || true)
    # Our own QEMU by the PID xtask printed, never by name.
    if [ -n "$vm" ] && kill -0 "$vm" 2>/dev/null; then kill "$vm" 2>/dev/null || true; fi
    if kill -0 "$2" 2>/dev/null; then kill "$2" 2>/dev/null || true; fi
    wait "$2" 2>/dev/null || true
}
cleanup() { for p in "${PIDS[@]}"; do kill -0 "$p" 2>/dev/null && kill "$p" 2>/dev/null; done; true; }
trap cleanup EXIT

run() { # tag, config extra, qmp helper args, extra qemu args...
    local tag="$1" cfg="$2" helper="$3"
    shift 3
    local name="rbq-$tag" sock="$RUN/$tag.qmp" log="$RUN/$tag.log"
    rm -f "$sock" "$log"
    cargo xtask image --name "$name" --ktest --config-extra "$cfg" > "$RUN/$tag-image.txt" 2>&1
    cargo xtask qemu --name "$name" --smp "$SMP" --timeout 180 --boot-retries 4 --serial-log "$log" -- -qmp "unix:$sock,server,wait=off" "$@" > "$RUN/$tag-qemu.txt" 2>&1 &
    local qpid=$!
    PIDS+=("$qpid")
    # shellcheck disable=SC2086
    python3 -I "$QMP_EVENTS" "$sock" 150 $helper --serial "$log" --quit > "$RUN/$tag-events.txt" 2>&1 || true
    for _ in $(seq 1 30); do kill -0 "$qpid" 2>/dev/null || break; sleep 1; done
    stop_run "$RUN/$tag-qemu.txt" "$qpid"
    touch "$log"
}

parked_lines() { # log, label
    check "$1" "rb quiesce\.test cpu=0 others_ran=0 lock_ok=true workers=$OTHERS" "$2: the test Quiesce ran on CPU 0 while the $OTHERS workers stood still, their lock free"
}

reset_lines() { # tag, label
    check "$RUN/$1-events.txt" '"event": "RESET".*"reason": "guest-reset"' "$2: QEMU saw a guest reset"
    local n
    n=$(grep -ac 'kernel start' "$RUN/$1.log" || true)
    if [ "${n:-0}" -ge 2 ]; then echo "ok   $2: the machine came back ($n kernel starts)"; else echo "FAIL $2: $n kernel start(s), wanted 2"; fail=1; fi
}

off_lines() { # tag, label
    check "$RUN/$1-events.txt" '"event": "SHUTDOWN".*"reason": "guest-shutdown"' "$2: QEMU saw the guest switch itself off"
    if grep -aq "KTEST FAIL rb.quiesce.qemu_action" "$RUN/$1.log"; then echo "FAIL $2: power-off came back"; fail=1; fi
}

KT="ktest=rb.quiesce.qemu_action"

run reboot "$KT;rb_quiesce_test=reboot" "--resets 1 --boots 2" -action reboot=reset
check "$RUN/reboot.log" "rb cpus\.parked what=\"?reboot\"? cpus_stopped=$OTHERS by_irq=$OTHERS " "reboot: every AP parked by the park IPI"
check "$RUN/reboot.log" "rb quiesce\.all devices=.*rb-quiesce-test" "reboot: quiesce_all after the park"
parked_lines "$RUN/reboot.log" "reboot"
check "$RUN/reboot.log" 'power reboot\.step n=1 ' "reboot: the reset sequence ran with the CPUs parked"
reset_lines reboot "reboot"

run off "$KT;rb_quiesce_test=poweroff" "--until-shutdown" -action shutdown=pause
check "$RUN/off.log" "rb cpus\.parked what=\"?poweroff\"? cpus_stopped=$OTHERS by_irq=$OTHERS " "poweroff: every AP parked by the park IPI"
parked_lines "$RUN/off.log" "poweroff"
check "$RUN/off.log" 'power off\.slp_typ ' "poweroff: SLP_TYP written with the CPUs parked"
off_lines off "poweroff"

glue_cp=0
grep -q 'park_others("reboot")' crates/platform/src/power.rs && grep -q 'park_others("poweroff")' crates/platform/src/power.rs && glue_cp=1
run preboot "$KT;rb_quiesce_test=platform-reboot" "--resets 1 --boots 2" -action reboot=reset
reset_lines preboot "platform reboot"
run poff "$KT;rb_quiesce_test=platform-poweroff" "--until-shutdown" -action shutdown=pause
off_lines poff "platform poweroff"
if [ "$glue_cp" = 1 ]; then
    check "$RUN/preboot.log" "power cpus\.parked what=\"?reboot\"? cpus_stopped=$OTHERS by_irq=$OTHERS" "platform reboot: parked first"
    parked_lines "$RUN/preboot.log" "platform reboot"
    check "$RUN/poff.log" "power cpus\.parked what=\"?poweroff\"? cpus_stopped=$OTHERS by_irq=$OTHERS" "platform poweroff: parked first"
    parked_lines "$RUN/poff.log" "platform poweroff"
else
    missing_glue "platform park lines: crates/platform/src/power.rs calls no park_others(\"reboot\"/\"poweroff\") yet (change request 1)"
fi

# 5. MCP reboot with load on the APs (a normal image, not a ktest image).
glue_mc=0
grep -q 'park_others("mcp-reboot")' crates/mcp/src/tools/reboot.rs && glue_mc=1
tag=mcp
log="$RUN/$tag.log"
sock="$RUN/$tag.qmp"
rm -f "$log" "$sock"
cargo xtask image --name rbq-mcp --config-extra "token=$TOKEN;mcp_port=8765;loglevel=info;rb_quiesce_load_s=120" > "$RUN/$tag-image.txt" 2>&1
cargo xtask qemu --name rbq-mcp --smp "$SMP" --net igb --hostfwd "$MCP_FWD:8765" --timeout 300 --serial-log "$log" -- -qmp "unix:$sock,server,wait=off" -action reboot=reset > "$RUN/$tag-qemu.txt" 2>&1 &
qpid=$!
PIDS+=("$qpid")
python3 -I "$QMP_EVENTS" "$sock" 240 --resets 1 --boots 2 --serial "$log" --quit > "$RUN/$tag-events.txt" 2>&1 &
epid=$!
PIDS+=("$epid")
python3 -I "$HERE/mcp_reboot.py" "$MCP_FWD" "$TOKEN" > "$RUN/$tag-driver.txt" 2>&1 || { echo "FAIL MCP driver:"; cat "$RUN/$tag-driver.txt"; fail=1; }
cat "$RUN/$tag-driver.txt"
wait "$epid" 2>/dev/null || true
stop_run "$RUN/$tag-qemu.txt" "$qpid"
touch "$log"
reset_lines mcp "MCP reboot"
check "$log" "rb quiesce\.load workers=$OTHERS seconds=120" "MCP reboot: busy workers on every AP"
check "$log" "rb quiesce\.test cpu=[0-9]+ .*lock_ok=true workers=$OTHERS" "MCP reboot: the test Quiesce ran in quiesce_all"
if [ "$glue_mc" = 1 ]; then
    check "$log" "power cpus\.parked what=\"?mcp-reboot\"? cpus_stopped=$OTHERS by_irq=$OTHERS" "MCP reboot: parked first"
    parked_lines "$log" "MCP reboot"
else
    missing_glue "MCP park lines: crates/mcp/src/tools/reboot.rs calls no park_others(\"mcp-reboot\") yet (change request 2)"
fi
exit $fail
