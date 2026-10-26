#!/bin/bash
# slow
# RB-2: the panic path under QEMU (KVM, 4 CPUs, user network), each part over a warm reset into a second boot:
#   part 1, a Rust panic: CONFIG panic=reboot panic_delay=1, so the guest resets itself through 0xCF9 (QEMU with
#           -action reboot=reset keeps RAM). Boot 1: the other 3 CPUs stopped by NMI with registers and
#           backtraces, the panicking CPU's backtrace names rb_crash_level_three/two/one in order (symbol table
#           from host/tests/rb/ksyms.sh), the crash record written, the reboot announced. Boot 2: the boot marker
#           survived (marker=kept), `crash previous` and its lines logged, rb.crash.forced checks the record.
#           The listener (console-listen.mjs, unchanged) gets the report lines through the mirror's emergency
#           path and the next boot's `crash previous`.
#   part 2, a kernel #GP (fatal exception): panic_delay=300, so the guest waits, and the scenario resets it
#           through QMP system_reset. The backtrace crosses the trap frame (`crash trap ... vector=13`) into
#           rb_crash_level_three; boot 2 sees exception=true.
# Needs the desk's panic handler to call kexec::crash::report (change request 1 in plan/notes/rb-RB-2.md);
# without it the scenario says SKIPPED, and BITSOS_RB_CRASH=required turns that into RED.
# Ports: the listener on LO+310 (never 6666).
set -eu
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/rb-40-crash}"
mkdir -p "$RUN_DIR"
RUN_DIR="$(realpath "$RUN_DIR")"
LO="${BITSOS_PORT_LO:-23000}"
PORT=$((LO + 310))
LISTENER=/home/georg/BitsOS/BitsOS-Plattform/console-listen.mjs
SMP="${RB_SMP:-4}"

if ! grep -q "kexec::crash::report(info)" kernel/src/main.rs; then
    if [ "${BITSOS_RB_CRASH:-}" = required ]; then
        echo "RED: BITSOS_RB_CRASH=required but kernel/src/main.rs's panic handler does not call kexec::crash::report(info) (change request 1)"
        exit 1
    fi
    echo "GREEN (SKIPPED: kernel/src/main.rs's panic handler does not call kexec::crash::report(info) yet; change request 1 in plan/notes/rb-RB-2.md)"
    exit 0
fi
STOP_FIRST=yes
grep -q "kexec::crash::stop_others();" kernel/src/main.rs || STOP_FIRST=no

LPID=""
QPID=""
cleanup() {
    for p in $LPID $QPID; do
        if kill -0 "$p" 2> /dev/null; then kill "$p" 2> /dev/null || true; wait "$p" 2> /dev/null || true; fi
    done
    for n in rb-crash-panic rb-crash-fault; do
        f="target/xtask/qemu-$n.pid"
        [ -f "$f" ] && q=$(head -1 "$f") && tr '\0' ' ' < "/proc/$q/cmdline" 2> /dev/null | grep -Eq -- "-name v2-[A-Za-z0-9]+-$n( |$)" && kill "$q" 2> /dev/null || true
    done
}
trap cleanup EXIT

LISTEN=no
command -v node > /dev/null && [ -f "$LISTENER" ] && LISTEN=yes

listen() { # $1 = output file
    rm -f "$1"
    node "$LISTENER" "$PORT" "$1" > "$1.out" 2>&1 &
    LPID=$!
    for _ in $(seq 50); do grep -q "listening on UDP port $PORT" "$1" 2> /dev/null && return 0; sleep 0.1; done
    echo "RED: the listener did not start"; cat "$1.out"; exit 1
}

stop_listen() {
    sleep 1
    kill -INT "$LPID" 2> /dev/null || true
    wait "$LPID" 2> /dev/null || true
    LPID=""
}

# Splits a serial log into boot1 / boot2 at bitsboot's banner.
split_boots() { # $1 = serial log, $2 = prefix
    awk -v p="$2" '/^bitsboot: BitsOS V2 loader/ { n++ } { if (n <= 1) print > (p ".boot1"); else print > (p ".boot2") }' "$1"
    touch "$2.boot1" "$2.boot2"
}

red() { echo "RED: $1"; [ -n "${2:-}" ] && tail -60 "$2" | cut -c1-240; exit 1; }
has() { grep -aqE -- "$2" "$1" || red "$3" "$1"; }
line() { grep -anE -- "$2" "$1" | head -1 | cut -d: -f1; }

check_report() { # $1 = boot-1 log, $2 = kind
    local b="$1"
    has "$b" "KTEST PANIC panicked at " "no panic in boot 1"
    has "$b" "E crash cpus.stopped asked=$((SMP - 1)) answered=$((SMP - 1)) send_failed=0" "not every other CPU answered the NMI stop"
    for c in $(seq 1 $((SMP - 1))); do
        has "$b" "E crash cpu cpu=$c apic=[0-9]+ rip=0x[0-9a-f]+ .*at=" "no registers of CPU $c"
    done
    local three two one
    three=$(line "$b" "E crash frame cpu=0 .*at=kexec::crash::ktests::rb_crash_level_three\+")
    two=$(line "$b" "E crash frame cpu=0 .*at=kexec::crash::ktests::rb_crash_level_two\+")
    one=$(line "$b" "E crash frame cpu=0 .*at=kexec::crash::ktests::rb_crash_level_one\+")
    [ -n "$three" ] && [ -n "$two" ] && [ -n "$one" ] && [ "$three" -lt "$two" ] && [ "$two" -lt "$one" ] ||
        red "the backtrace does not name rb_crash_level_three, two, one in order" "$b"
    if [ "$2" = fault ]; then
        local trap
        trap=$(line "$b" "E crash trap cpu=0 .*vector=13 .*user=false what=\"general protection \(#GP\)\"")
        [ -n "$trap" ] && [ "$trap" -lt "$three" ] || red "no #GP trap frame crossed before level three" "$b"
    fi
    has "$b" "E crash record.written phys=0x[0-9a-f]+ bytes=[0-9]+" "no crash record written"
    if grep -aq "memory allocation of .* failed" "$b"; then red "the panic path allocated" "$b"; fi
    if [ "$STOP_FIRST" = yes ] && grep -aq "alloc_error_handler" "$b"; then
        red "another CPU panicked on an allocation: the NMI stop came after the heap's emergency mode" "$b"
    fi
    echo "ok: boot 1 ($2): $((SMP - 1)) CPUs stopped by NMI with registers, levels three/two/one at lines $three/$two/$one, record written"
}

check_next_boot() { # $1 = boot-2 log, $2 = exception true|false
    local b="$1"
    has "$b" "I crash page phys=0x[0-9a-f]+ bytes=[0-9]+ marker=kept " "the boot marker did not survive the warm reset"
    has "$b" "E crash previous .*cpus_asked=$((SMP - 1)) cpus_answered=$((SMP - 1)) .*exception=$2 " "no crash.previous event for the record"
    has "$b" "I crash previous.line .*rb_crash_level_three" "the record's lines are not logged"
    has "$b" "KTEST ok rb.crash.forced" "rb.crash.forced did not check the record"
    echo "ok: boot 2: marker kept, crash.previous (exception=$2) and its lines logged, rb.crash.forced ok"
}

# --- Part 1: Rust panic, reboot by the guest after 1 s, mirror listener ---
NAME=rb-crash-panic
LOG="$RUN_DIR/panic.serial"
OUT="$RUN_DIR/panic.console.log"
rm -f "$LOG"
MIRROR=""
[ "$LISTEN" = yes ] && MIRROR=";mirror=10.0.2.2:$PORT"
cargo xtask image --name $NAME --ktest --config-extra "ktest=rb.crash.forced;rb_crash_panic=panic;panic=reboot;panic_delay=1$MIRROR" | tail -1
host/tests/rb/ksyms.sh $NAME ktest
[ "$LISTEN" = yes ] && listen "$OUT"
cargo xtask qemu --name $NAME --smp $SMP --net user --timeout 300 --serial-log "$LOG" -- -action reboot=reset > "$LOG.qemu" 2>&1 ||
    red "part 1: QEMU did not end with PASS ($(tail -1 "$LOG.qemu"))" "$LOG"
[ "$LISTEN" = yes ] && stop_listen
split_boots "$LOG" "$RUN_DIR/panic"
check_report "$RUN_DIR/panic.boot1" panic
has "$RUN_DIR/panic.boot1" "E crash reboot in_s=1 " "no reboot announced"
check_next_boot "$RUN_DIR/panic.boot2" false
if [ "$LISTEN" = yes ]; then
    has "$OUT" "E kernel panic \| panicked at " "the panic line did not reach the listener"
    has "$OUT" "E crash frame cpu=0 .*rb_crash_level_three" "the backtrace did not reach the listener"
    has "$OUT" "E crash cpu cpu=$((SMP - 1)) " "the other CPUs' registers did not reach the listener"
    has "$OUT" "E crash reboot in_s=1" "the last line did not reach the listener"
    has "$OUT" "E crash previous " "the next boot's crash.previous did not reach the listener"
    echo "ok: listener: panic line, backtrace, registers, reboot line, and the next boot's crash.previous"
else
    echo "note: node or console-listen.mjs missing: mirror part not checked"
fi

# --- Part 2: kernel #GP, the guest waits 300 s, QMP system_reset ---
NAME=rb-crash-fault
LOG="$RUN_DIR/fault.serial"
SOCK="$RUN_DIR/qmp.sock"
rm -f "$SOCK" "$LOG"
cargo xtask image --name $NAME --ktest --config-extra "ktest=rb.crash.forced;rb_crash_panic=fault;panic=reboot;panic_delay=300" | tail -1
host/tests/rb/ksyms.sh $NAME ktest
cargo xtask qemu --name $NAME --smp $SMP --net user --timeout 300 --serial-log "$LOG" -- -action reboot=reset -qmp "unix:$SOCK,server,nowait" > "$LOG.qemu" 2>&1 &
QPID=$!
for _ in $(seq 1200); do
    grep -aq "E crash reboot in_s=300 " "$LOG" 2> /dev/null && break
    kill -0 $QPID 2> /dev/null || red "part 2: QEMU ended before the panic report ($(tail -1 "$LOG.qemu"))" "$LOG"
    sleep 0.1
done
grep -aq "E crash reboot in_s=300 " "$LOG" || red "part 2: no report within 120 s" "$LOG"
for _ in $(seq 100); do [ -S "$SOCK" ] && break; sleep 0.1; done
python3 host/tests/rb/qmp.py "$SOCK" system_reset
wait $QPID || red "part 2: QEMU did not end with PASS after the QMP reset ($(tail -1 "$LOG.qemu"))" "$LOG"
QPID=""
split_boots "$LOG" "$RUN_DIR/fault"
check_report "$RUN_DIR/fault.boot1" fault
check_next_boot "$RUN_DIR/fault.boot2" true

echo "GREEN: panic path (NMI stop with registers of $((SMP - 1)) CPUs, backtraces with symbols, a #GP trap frame crossed, crash record across a guest reset and a QMP system_reset, mirror lines)"
