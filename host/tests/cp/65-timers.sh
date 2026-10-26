#!/bin/bash
# slow
# CP-5: the LAPIC timer paths QEMU's default (-cpu host: TSC-deadline) does not take (functional only).
#  1. -cpu host,-tsc-deadline -smp 16: like the Mini-PC (no TSC-deadline), every CPU calibrates its one-shot
#     timer (auto picks it), all 16 go live, and cp.time, contract.hrtimer_sleep and the sched tests pass on it.
#  2. hrtimer=poll -smp 4: the escape hatch: no LAPIC timer, every hrtimer on the polled queue; the contract and
#     sched tests still pass, the per-CPU timer test skips.
#  3. lapic_timer=fast: an unknown value is named in a warning and auto is used.
set -eu
cd "$BITSOS_ROOT"
RUN="$BITSOS_RUN_DIR"
fail=0
check() { if grep -aEq -- "$2" "$1"; then echo "ok   $3"; else echo "FAIL $3 (no '$2' in $(basename "$1"))"; fail=1; fi; }
count() { grep -acE -- "$2" "$1" || true; }

run() { # tag, extra config, ktest filter, qemu args...
    local tag="$1" extra="$2" filter="$3"
    local name="$BITSOS_LANE-$tag"
    shift 3
    cargo xtask image --name "$name" --ktest --config-extra "$extra;ktest=$filter" > "$RUN/$tag-image.txt" 2>&1
    cargo xtask qemu --name "$name" --timeout 600 --boot-retries 4 --serial-log "$RUN/$tag.log" -- "$@" > "$RUN/$tag-qemu.txt" 2>&1 || true
    touch "$RUN/$tag.log"
}

no_fail() { # log, what
    if grep -aqE 'KTEST (FAIL|PANIC)' "$1"; then echo "FAIL $2: $(grep -aE 'KTEST (FAIL|PANIC)' "$1" | head -3 | tr '\n' ' ')"; fail=1; else echo "ok   $2: no ktest failed"; fi
    check "$1" 'KTEST done passed=[0-9]+ failed=0' "$2: the ktest run finished"
}

# 1. One-shot on 16 CPUs (the Mini-PC's mode).
run oneshot "loglevel=info" "cp.time,contract.hrtimer,sched" -smp 16 -cpu host,-tsc-deadline
L="$RUN/oneshot.log"
n=$(count "$L" 'time lapic.timer cpu=[0-9]+ mode=oneshot counts_per_ms=[1-9][0-9]* config=auto')
[ "$n" = 16 ] && echo "ok   16 CPUs calibrated their one-shot timer" || { echo "FAIL $n CPUs in one-shot mode, wanted 16"; fail=1; }
check "$L" 'time timers.ready cpus=16 live=16 mode=oneshot interrupt_driven=true' "every CPU's timers fire by interrupt"
check "$L" 'KTEST ok cp.time.lapic_every_cpu' "a timer fired on every CPU from its own one-shot timer"
check "$L" 'KTEST ok cp.time.hrtimer_cancel_rearm' "cancel and re-arm from the callback in one-shot mode"
check "$L" 'KTEST ok contract.hrtimer_sleep' "the HrTimer contract test"
no_fail "$L" "one-shot"

# 2. hrtimer=poll.
run poll "hrtimer=poll" "cp.time,contract.hrtimer,sched.sleep,sched.wait" -smp 4
L="$RUN/poll.log"
n=$(count "$L" 'time lapic.timer_off cpu=[0-9]+ ')
[ "$n" = 4 ] && echo "ok   hrtimer=poll: no CPU started its LAPIC timer" || { echo "FAIL hrtimer=poll: $n 'lapic.timer_off', wanted 4"; fail=1; }
check "$L" 'interrupt_driven=false' "hrtimer=poll: not interrupt-driven"
check "$L" 'KTEST skip cp.time.lapic_every_cpu: hrtimer=poll' "hrtimer=poll: the per-CPU timer test skips"
check "$L" 'KTEST ok contract.hrtimer_sleep' "hrtimer=poll: the contract test passes on the polled queue"
no_fail "$L" "poll"

# 3. A value that is not understood.
run badcfg "lapic_timer=fast" "contract.hrtimer" -smp 2
L="$RUN/badcfg.log"
check "$L" 'time config.unknown key=lapic_timer value=fast' "lapic_timer=fast is named in a warning"
check "$L" 'time lapic.timer cpu=0 mode=[a-z_]+ .*config=auto' "and auto is used"
no_fail "$L" "bad value"
exit $fail
