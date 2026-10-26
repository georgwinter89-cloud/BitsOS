#!/bin/bash
# slow
# RB-4: the console mirror against V1's unchanged host listener (BitsOS-Plattform/console-listen.mjs, run from its
# read-only place). For each card (virtio-net, then igb; QEMU user network, guest sends to 10.0.2.2:<port>):
#   boot 1: ktests rb.mirror (live mirror, an emergency line through poll_tx with interrupts off, a quiesce of
#           the mirror and a resume): the listener shows the start line, the boot lines from before DHCP (kernel
#           start, klog ring.early) before the DHCP lease, the emergency line exactly once, "mirror ended", a
#           second start and the line after it, no loss marker;
#   boot 2: a forced panic after the boot went out: its line can only come through the panic path
#           (try_event! + NetDevice::poll_tx). Needs the desk's panic-handler lines and an allocation-free
#           console emergency write (plan/notes/rb-RB-4.md, change requests 1 and 2). Without them that part is
#           SKIPPED and the result line says so ("GREEN (panic part SKIPPED)"), so RB-4 is not counted as
#           fully proven; BITSOS_RB_MIRROR_PANIC=required turns the skip into RED (desk integration).
# Ports: the listener on LO+300 (never 6666, the real listener's).
set -eu
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/rb-30-mirror}"
mkdir -p "$RUN_DIR"
RUN_DIR="$(realpath "$RUN_DIR")"
LO="${BITSOS_PORT_LO:-23000}"
PORT=$((LO + 300))
LISTENER=/home/georg/BitsOS/BitsOS-Plattform/console-listen.mjs
BOOT_LINE="BdsDxe: loading"

if ! command -v node > /dev/null; then
    echo "SKIP: node is not installed (console-listen.mjs needs it)"
    exit 0
fi
[ -f "$LISTENER" ] || { echo "RED: $LISTENER missing"; exit 1; }

LPID=""
QNAME=""
cleanup() {
    local q=""
    # The QEMU of this run only: its pid file, checked against its -name (v2-<lane>-<name>, any lane).
    if [ -n "$QNAME" ]; then
        q=$(head -1 "target/xtask/qemu-$QNAME.pid" 2> /dev/null || true)
        if [ -z "$q" ] || ! tr '\0' ' ' < "/proc/$q/cmdline" 2> /dev/null | grep -Eq -- "-name v2-[A-Za-z0-9]+-$QNAME( |$)"; then q=""; fi
    fi
    for p in $LPID $q; do
        if kill -0 "$p" 2> /dev/null; then kill "$p" 2> /dev/null || true; wait "$p" 2> /dev/null || true; fi
    done
}
trap cleanup EXIT

listen() { # $1 = output file
    rm -f "$1"
    node "$LISTENER" "$PORT" "$1" > "$1.out" 2>&1 &
    LPID=$!
    for _ in $(seq 50); do grep -q "listening on UDP port $PORT" "$1" 2> /dev/null && return 0; sleep 0.1; done
    echo "RED: the listener did not start"; cat "$1.out"; exit 1
}

stop_listen() {
    sleep 1 # the last datagrams
    kill -INT "$LPID" 2> /dev/null || true
    wait "$LPID" 2> /dev/null || true
    LPID=""
}

boot() { # $1 = image name, $2 = net, $3 = serial log; QEMU's exit status is the caller's business
    QNAME="$1"
    for attempt in 1 2; do
        if cargo xtask qemu --name "$1" --net "$2" --timeout 150 --serial-log "$3" > "$3.qemu" 2>&1; then
            return 0
        fi
        grep -aq "$BOOT_LINE" "$3" && return 1
        echo "note: attempt $attempt: firmware never reached the boot loader, retrying"
    done
    return 1
}

line_no() { grep -an -- "$2" "$1" | head -1 | cut -d: -f1; }

PANIC_CR=yes
grep -qE "klog::flush_emergency\(\)|kexec::crash::report\(info\)" kernel/src/main.rs || PANIC_CR=no
if [ "$PANIC_CR" = no ] && [ "${BITSOS_RB_MIRROR_PANIC:-}" = required ]; then
    echo "RED: BITSOS_RB_MIRROR_PANIC=required but kernel/src/main.rs's panic handler does not call klog::flush_emergency() (change request 1)"
    exit 1
fi
PANIC_SKIPPED=""

for NET in user igb; do
    echo "== $NET =="
    LOG="$RUN_DIR/$NET-ktest.serial"
    OUT="$RUN_DIR/$NET-ktest.console.log"
    cargo xtask image --name rb-mirror --ktest --config-extra "ktest=rb.mirror;mirror=10.0.2.2:$PORT" | tail -1
    listen "$OUT"
    boot rb-mirror "$NET" "$LOG" || { echo "RED: ktests failed ($NET)"; grep -a KTEST "$LOG"; exit 1; }
    stop_listen
    grep -a "KTEST" "$LOG"
    grep -aq "KTEST ok rb.mirror.live" "$LOG" || { echo "RED: rb.mirror.live did not pass"; exit 1; }
    grep -aq "KTEST ok rb.mirror.emergency (" "$LOG" || { echo "RED: rb.mirror.emergency did not pass"; exit 1; }
    grep -aq "KTEST ok rb.mirror.restart" "$LOG" || { echo "RED: rb.mirror.restart did not pass"; exit 1; }
    starts=$(grep -c "start 0: mirror from CONFIG.TXT" "$OUT" || true)
    [ "$starts" -eq 2 ] || { echo "RED: $starts start lines (want 2: boot and the restart)"; grep "====" "$OUT"; exit 1; }
    ended=$(grep -c ": mirror ended ====" "$OUT" || true)
    [ "$ended" -eq 1 ] || { echo "RED: 'mirror ended' $ended times (want 1, from the restart test's quiesce)"; grep "====" "$OUT"; exit 1; }
    after=$(grep -c "log| .*W rb mirror.after_restart " "$OUT" || true)
    [ "$after" -eq 1 ] || { echo "RED: the line after the restart arrived $after times (want 1)"; tail -20 "$OUT"; exit 1; }
    early=$(line_no "$OUT" "log| .* I klog ring.early")
    start=$(line_no "$OUT" "log| .* I kernel start ")
    lease=$(line_no "$OUT" "log| .* I net dhcp.lease ")
    live=$(line_no "$OUT" "log| .* I klog mirror.live ")
    [ -n "$early" ] && [ -n "$start" ] && [ -n "$lease" ] && [ -n "$live" ] || { echo "RED: boot lines missing on the listener"; head -40 "$OUT"; exit 1; }
    [ "$early" -lt "$lease" ] && [ "$start" -lt "$lease" ] || { echo "RED: boot lines not before the lease"; exit 1; }
    n=$(grep -c "E rb mirror.emergency_line marker=" "$OUT" || true)
    [ "$n" -eq 1 ] || { echo "RED: emergency line arrived $n times (want exactly 1)"; exit 1; }
    if grep -q "bytes lost\|records lost" "$OUT"; then echo "RED: loss marker in a normal boot"; grep "lost" "$OUT"; exit 1; fi
    lines=$(grep -c "^log| " "$OUT")
    echo "listener: $lines log lines; ring.early at $early, kernel start at $start, dhcp.lease at $lease, mirror.live at $live"

    if [ "$PANIC_CR" = no ]; then
        echo "SKIPPED (panic part, $NET): kernel/src/main.rs does not call klog::flush_emergency() in the panic handler yet (change requests 1 and 2)"
        PANIC_SKIPPED="$PANIC_SKIPPED $NET"
        continue
    fi
    LOG="$RUN_DIR/$NET-panic.serial"
    OUT="$RUN_DIR/$NET-panic.console.log"
    cargo xtask image --name rb-mirror-panic --ktest --config-extra "ktest=rb.mirror.forced_panic;rb_mirror_panic=on;mirror=10.0.2.2:$PORT" | tail -1
    listen "$OUT"
    boot rb-mirror-panic "$NET" "$LOG" || true
    stop_listen
    grep -aq "KTEST PANIC" "$LOG" && grep -aq "^rb.mirror forced panic marker=" "$LOG" || { echo "RED: no forced panic on serial ($NET)"; grep -a -A1 KTEST "$LOG"; exit 1; }
    if grep -aq "memory allocation of .* failed" "$LOG"; then echo "RED: the panic path allocated ($NET)"; grep -a -B1 "memory allocation" "$LOG"; exit 1; fi
    grep -q "log| .*W rb mirror.panic_next" "$OUT" || { echo "RED: the line before the panic is missing"; tail -20 "$OUT"; exit 1; }
    grep -q "rb.mirror forced panic marker=" "$OUT" || { echo "RED: the panic line did not reach the listener ($NET)"; tail -20 "$OUT"; exit 1; }
    grep "rb.mirror forced panic" "$OUT"
done
if [ -n "$PANIC_SKIPPED" ]; then
    echo "GREEN (panic part SKIPPED:$PANIC_SKIPPED; RB-4 not fully proven until change requests 1 and 2 are in): console mirror (virtio-net and igb) with the unchanged console-listen.mjs"
else
    echo "GREEN: console mirror (virtio-net and igb, forced panic through poll_tx included) with the unchanged console-listen.mjs"
fi
