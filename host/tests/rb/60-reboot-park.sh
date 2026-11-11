#!/bin/bash
# slow
# time-limit: 1500
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
# RB-14: the same for the storage stop before the park (change requests CR-1 reboot.rs, CR-2 platform/power.rs of
# plan/notes/rb-RB-14.md): once the source calls block::shutdown::stop_all there, runs 3 and 5 must log
# `block shutdown.done why=<reboot|mcp-reboot> ... quiet=true` before the park line; until then SKIP.
# RB-15: the shutdown order of klog::shutdown on the console (runs 3 and 5): the mirror paused before the storage
# stop, the storage step, the park, the quiesce step, then the drivers' quiesce and the reset; no step out of
# order. Run 5 also has the console mirror on (CONFIG mirror= to V1's console-listen.mjs, igb): the listener
# must get every shutdown line (mirror_paused, shutdown.done, cpus.parked, step=quiesce) and "mirror ended"
# before the second start's start line, i.e. the end datagram left the card before the reset.
# Ports: RB's range (BITSOS_PORT_LO, default 23000); MCP forward at LO+120, the mirror listener at LO+130.
set -eu
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
RUN="${BITSOS_RUN_DIR:-target/xtask/runs/rb-60-reboot-park}"
mkdir -p "$RUN"
RUN="$(realpath "$RUN")"
HERE="$(pwd)/host/tests/rb"
QMP_EVENTS="$(pwd)/host/tests/cp/qmp_events.py"
LO="${BITSOS_PORT_LO:-23000}"
MCP_FWD=$((LO + 120))
MIRROR_PORT=$((LO + 130))
# V1's host listener, unchanged (BITSOS_LISTENER overrides where V1 lies; no home value in the public tree).
LISTENER="${BITSOS_LISTENER:-$HOME/BitsOS/BitsOS-Plattform/console-listen.mjs}"
TOKEN="rbq-scenario-token-5e7a"
SMP="${RB_SMP:-16}"
OTHERS=$((SMP - 1))
fail=0
missing_glue() { if [ "${RB_REQUIRE_GLUE:-0}" = 1 ]; then echo "FAIL $1 (RB_REQUIRE_GLUE=1)"; fail=1; else echo "SKIP $1"; fi; }
check() { if grep -aEq -- "$2" "$1"; then echo "ok   $3"; else echo "FAIL $3 (no '$2' in $(basename "$1"))"; fail=1; fi; }
# order FILE A B WHAT: the first line matching A comes before the first line matching B.
order() {
    local a b
    a=$(grep -anE -- "$2" "$1" | head -1 | cut -d: -f1)
    b=$(grep -anE -- "$3" "$1" | head -1 | cut -d: -f1)
    if [ -n "$a" ] && [ -n "$b" ] && [ "$a" -lt "$b" ]; then echo "ok   $4"; else echo "FAIL $4 (line ${a:-none} of '$2', ${b:-none} of '$3')"; fail=1; fi
}

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
grep -Eq 'park_others\("reboot"\)|stop_and_(park|quiesce)\("reboot"' crates/platform/src/power.rs \
    && grep -Eq 'park_others\("poweroff"\)|stop_and_(park|quiesce)\("poweroff"' crates/platform/src/power.rs && glue_cp=1
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
if grep -q 'block::shutdown::stop_all(' crates/platform/src/power.rs; then
    check "$RUN/preboot.log" 'block shutdown\.done why="?reboot"? .*quiet=true' "platform reboot: storage stopped quiet (RB-14)"
    order "$RUN/preboot.log" 'block shutdown\.done why="?reboot"? ' 'power cpus\.parked what="?reboot"? ' "platform reboot: storage stop before the park (RB-14)"
else
    missing_glue "platform storage stop: crates/platform/src/power.rs calls no block::shutdown::stop_all yet (RB-14 CR-2)"
fi

# shutdown_order LOG WHAT LABEL STOP_LINE: RB-15's order of klog::shutdown on the console.
shutdown_order() {
    local w="what=\"?$2\"?"
    order "$1" "shutdown mirror_paused $w" "block shutdown\.done why=\"?$2\"? " "$3: mirror paused before the storage stop (RB-15)"
    order "$1" "block shutdown\.done why=\"?$2\"? " "shutdown step $w step=\"?storage" "$3: storage step after the storage stop (RB-15)"
    order "$1" "shutdown step $w step=\"?storage" "power cpus\.parked $w " "$3: park after the storage step (RB-15)"
    order "$1" "power cpus\.parked $w " "shutdown step $w step=\"?parked" "$3: parked step after the park (RB-15)"
    order "$1" "shutdown step $w step=\"?parked" "shutdown step $w step=\"?quiesce" "$3: quiesce step after the parked step (RB-15)"
    order "$1" "shutdown step $w step=\"?quiesce" "$4" "$3: the drivers' quiesce after the quiesce step (RB-15)"
    order "$1" "$4" "shutdown step $w step=\"?reset" "$3: the reset step last (RB-15)"
    if grep -aEq "shutdown step $w .*in_order=false" "$1"; then echo "FAIL $3: a shutdown step out of order"; fail=1; else echo "ok   $3: no step out of order"; fi
}
if grep -Eq 'klog::shutdown::(pause_mirror|run\()' crates/platform/src/power.rs; then
    shutdown_order "$RUN/preboot.log" reboot "platform reboot" 'power reboot\.requested '
else
    missing_glue "platform shutdown order: crates/platform/src/power.rs pauses no mirror yet (RB-15)"
fi

# 5. MCP reboot with load on the APs (a normal image, not a ktest image), the console mirror on.
glue_mc=0
grep -q 'park_others("mcp-reboot")' crates/mcp/src/tools/reboot.rs && glue_mc=1
tag=mcp
log="$RUN/$tag.log"
sock="$RUN/$tag.qmp"
rm -f "$log" "$sock"
listen_out="$RUN/$tag-listener.txt"
lpid=""
rm -f "$listen_out"
mirror_cfg=""
if command -v node > /dev/null && [ -f "$LISTENER" ]; then
    node "$LISTENER" "$MIRROR_PORT" "$listen_out" > "$listen_out.out" 2>&1 &
    lpid=$!
    PIDS+=("$lpid")
    for _ in $(seq 50); do grep -q "listening on UDP port $MIRROR_PORT" "$listen_out" 2> /dev/null && break; sleep 0.1; done
    mirror_cfg=";mirror=10.0.2.2:$MIRROR_PORT"
fi
cargo xtask image --name rbq-mcp --config-extra "token=$TOKEN;mcp_port=8765;loglevel=info;rb_quiesce_load_s=120$mirror_cfg" > "$RUN/$tag-image.txt" 2>&1
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
if grep -q 'block::shutdown::stop_all("mcp-reboot"' crates/mcp/src/tools/reboot.rs; then
    check "$log" 'block shutdown\.done why="?mcp-reboot"? .*quiet=true' "MCP reboot: storage stopped quiet (RB-14)"
    check "$log" 'mcp reboot\.(synced|sync_timeout|sync_skipped) .*ms=[0-9]+' "MCP reboot: the write-back's time is logged (RB-14)"
    order "$log" 'block shutdown\.done why="?mcp-reboot"? ' 'power cpus\.parked what="?mcp-reboot"? ' "MCP reboot: storage stop before the park (RB-14)"
else
    missing_glue "MCP storage stop: crates/mcp/src/tools/reboot.rs calls no block::shutdown::stop_all(\"mcp-reboot\") yet (RB-14 CR-1)"
fi
if grep -Eq 'klog::shutdown::(pause_mirror\("mcp-reboot"|run\()' crates/mcp/src/tools/reboot.rs; then
    shutdown_order "$log" mcp-reboot "MCP reboot" 'mcp reboot\.quiesced '
    check "$log" 'shutdown mirror_paused what="?mcp-reboot"? .*done=true' "MCP reboot: the mirror paused, the card said its raw frames were sent (RB-15)"
    check "$log" 'mcp reboot\.quiesced devices="?([^ ]*,)?mirror,([^ ]*,)?igc0' "MCP reboot: the mirror quiesced before the card (RB-15)"
    if [ -n "$lpid" ]; then
        sleep 1
        kill -INT "$lpid" 2> /dev/null || true
        wait "$lpid" 2> /dev/null || true
        for l in 'shutdown mirror_paused what="?mcp-reboot"?' 'block shutdown\.done why="?mcp-reboot"? ' 'power cpus\.parked what="?mcp-reboot"? ' 'shutdown step what="?mcp-reboot"? step="?quiesce'; do
            check "$listen_out" "$l" "MCP reboot listener: '$l' arrived (RB-15, raw path)"
        done
        order "$listen_out" 'shutdown step what="?mcp-reboot"? step="?quiesce' 'mirror ended' "MCP reboot listener: the quiesce step before the end datagram (RB-15)"
        # The end datagram goes out in the quiesce, before the reset; the second kernel's mirror start (if it went
        # live before QEMU was stopped) comes after it.
        ended=$(grep -an 'mirror ended' "$listen_out" | head -1 | cut -d: -f1)
        second=$(grep -anE ' start [0-9]+: ' "$listen_out" | sed -n 2p | cut -d: -f1)
        if [ -n "$ended" ] && { [ -z "$second" ] || [ "$ended" -lt "$second" ]; }; then
            echo "ok   MCP reboot listener: 'mirror ended' (line $ended) before the reset${second:+ and the second start (line $second)} (RB-15)"
        else
            echo "FAIL MCP reboot listener: 'mirror ended' line ${ended:-none}, second start line ${second:-none} (RB-15)"; fail=1
        fi
        if grep -aq 'bytes lost\]' "$listen_out"; then echo "FAIL MCP reboot listener: a loss marker"; fail=1; else echo "ok   MCP reboot listener: nothing lost"; fi
        # RB-16: a breadcrumb before each step and each device's quiesce on the wire (the mirror and its card
        # quiesced after every other device); the card's own crumb under the crumb stream after the end datagram.
        if grep -q 'klog::crumb::leave' crates/devmodel/src/lib.rs; then
            check "$listen_out" 'crumb [0-9]+ quiesce "?mcp-reboot' "MCP reboot listener: the quiesce step's breadcrumb (RB-16)"
            order "$listen_out" 'crumb [0-9]+ device rb-quiesce-test' 'crumb [0-9]+ device mirror' "MCP reboot listener: the test device's breadcrumb before the mirror's (RB-16)"
            order "$listen_out" 'crumb [0-9]+ device mirror' 'mirror ended' "MCP reboot listener: the mirror's breadcrumb before its end datagram (RB-16)"
            order "$listen_out" 'mirror ended' 'crumb [0-9]+ device igc0' "MCP reboot listener: the card's breadcrumb after the end, quiesced last (RB-16)"
            check "$log" 'mcp reboot\.quiesced devices="?([^ ]*,)?rb-quiesce-test,([^ ]*,)?mirror,igc0"?([[:space:]]|$)' "MCP reboot: the transport (mirror, igc0) quiesced last (RB-16)"
        fi
    else
        echo "SKIP MCP reboot listener: node or $LISTENER missing"
    fi
else
    missing_glue "MCP shutdown order: crates/mcp/src/tools/reboot.rs pauses no mirror yet (RB-15)"
fi
# RB-17 CR-2: acpi::power::reboot leaves a crumb right before each reset write (FADT, CF9 6, CF9 0xE, 8042).
# QEMU cannot show those crumbs: the wire (mirror, card) is quiesced before the reset and QEMU has no FCH
# watchdog to keep them; CR-2's host test checks the mapping, the Mini-PC list (RB-17 K4) the next start's
# reading. Here only the source glue.
if grep -Eq 'klog::crumb::leave\(' crates/acpi/src/power.rs && grep -q 'fn crumb' crates/acpi/src/power.rs; then
    echo "ok   reboot: a crumb before each reset write in acpi::power::reboot (RB-17 CR-2)"
else
    missing_glue "reboot: crates/acpi/src/power.rs leaves no crumb before its reset writes yet (RB-17 CR-2)"
fi
# TS-1: this test ends the boot (or answers over MCP), so it prints no `KTEST ok` on the serial line: the ledger reads it here.
[ "$fail" != 0 ] || printf 'KTEST ok rb.quiesce.qemu_action (checked by this script)\nKTEST ok rb.quiesce.load_for_mcp (checked by this script)\n' >> "$RUN/ledger.txt"
exit $fail
