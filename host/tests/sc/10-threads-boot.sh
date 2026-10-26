#!/bin/bash
# SC-1: a normal (non-ktest) boot. Covers what the ktests cannot: the boot thread (whose stack is bitsboot's,
# not a hal stack) runs the smoke programs in user mode next to the MCP acceptor thread and then ends with
# `sched::exit()`; its last switch must leave the kernel running on the idle and MCP threads.
# Passes when the scheduler reported its XSAVE mode, both smoke programs printed, the boot thread ended and
# nothing panicked within 12 s. SC-2: and the lock-order checker (debug kernel) found nothing in a normal boot
# (no `E ksync lock.violation`, `lock_checker=true` in the ready event). SC-3: the AP (xtask's default -smp 2)
# joined the scheduler (`sched cpu.join`), and no reaper stall or failed join was reported. SC-4: the
# scheduler's hrtimers are set up (`sched timers.ready sleep=hrtimer`) and no scheduler error was reported
# (`E sched`). A `sched timer.fallback` line is printed for information only: during start both CPUs can be
# busy (start code, klogd drawing the console), and then a busy thread's preemption point ends the sleeps.
set -eu
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/sc-10-threads-boot}"
mkdir -p "$RUN_DIR"
RUN_DIR="$(realpath "$RUN_DIR")"
LOG="$RUN_DIR/boot.log"
NAME="sc-threads-boot"

cargo xtask image --name "$NAME" --config-extra "token=sc1-scenario-token;loglevel=debug"
# xtask stops its own QEMU at the timeout and then exits non-zero; the log is the verdict.
cargo xtask qemu --name "$NAME" --timeout 12 --net user --serial-log "$LOG" || true

grep -a "sched ready\|sched timers.ready\|sched timer.fallback\|sched cpu.join\|smoke \|thread.exit id=1 \|PANIC\|panicked\|switch.failed\|spawn.failed\|stack_overflow\|ksync lock\.\|reap.stalled" "$LOG" || true
grep -aq "sched ready mode=sc3-smp" "$LOG" || { echo "RED: no sched ready event"; exit 1; }
grep -aq "sched timers.ready sleep=hrtimer tick=hrtimer_when_busy" "$LOG" || { echo "RED: no sched timers.ready event"; exit 1; }
if grep -aq "E sched " "$LOG"; then echo "RED: scheduler error event"; exit 1; fi
grep -aq "sched cpu.join cpu=1 " "$LOG" || { echo "RED: the AP did not join the scheduler"; exit 1; }
if grep -aq "reap.stalled\|cpu.join_failed" "$LOG"; then echo "RED: reaper stall or a CPU that could not join"; exit 1; fi
[ "$(grep -ac "smoke ok program=" "$LOG")" -eq 2 ] || { echo "RED: the smoke programs did not both pass"; exit 1; }
grep -aq "thread.exit id=1 name=boot" "$LOG" || { echo "RED: the boot thread did not end"; exit 1; }
if grep -aq "PANIC\|panicked\|switch.failed\|spawn.failed" "$LOG"; then echo "RED: panic or scheduler error"; exit 1; fi
grep -aq "sched ready mode=sc3-smp lock_checker=true" "$LOG" || { echo "RED: lock checker not on in the debug kernel"; exit 1; }
if grep -aq "E ksync lock.violation" "$LOG"; then echo "RED: lock-order violation in a normal boot"; exit 1; fi
echo "GREEN"
