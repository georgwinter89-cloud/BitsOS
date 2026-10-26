#!/bin/bash
# slow
# CP-4b: CONFIG smp=off and cpus=N (functional, QEMU, -smp 16).
#  1. smp=off: one CPU (no AP prepared or started), one `smp not_started` event, the Reload jump's single-CPU
#     check passes (RB-5a's ktest takes its one-CPU branch).
#  2. cpus=4: CPUs 0-3 online, 12 APs held back, every online AP answers; the full cp.smp set stays green.
#  3. cpus=abc: a value that does not parse never means all CPUs: one CPU and a warning naming the key.
#  4. smp=of;cpus=0: both bad; smp= decides, one CPU, two warnings.
#  5. cpus=64 under -smp 4: a limit above the MADT's count changes nothing (no `smp limited`).
#  6. cpus=1 (smp on): the BSP alone, reason names cpus=1.
#  7. smp=off;cpus=8: smp=off wins, no warning.
# The Reload hand-over of this CONFIG (smp=off reaches the next kernel) is run 1's rb.reload.stage_synthetic: it
# checks the new BootInfo's CONFIG bytes equal the running kernel's. The real jump waits for RB's RootDiffers fix.
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

# 1. smp=off under -smp 16.
run off "smp=off" "cp.smp,cp.percpu,contract.percpu,rb.reload.stage_synthetic" -smp 16
L="$RUN/off.log"
check "$L" 'smp not_started reason="?CONFIG smp=off"? offline=15' "smp=off: one not_started event naming the key, 15 CPUs offline"
n=$(count "$L" 'smp not_started ')
[ "$n" = 1 ] && echo "ok   logged once" || { echo "FAIL $n 'smp not_started' events, wanted 1"; fail=1; }
n=$(count "$L" 'cpu ap.up ')
[ "$n" = 0 ] && echo "ok   no AP started" || { echo "FAIL $n 'cpu ap.up' events with smp=off"; fail=1; }
n=$(count "$L" 'smp prepared ')
[ "$n" = 0 ] && echo "ok   nothing prepared (no trampoline set-up)" || { echo "FAIL 'smp prepared' with smp=off"; fail=1; }
check "$L" 'KTEST ok cp.smp.limit_off' "cp.smp.limit_off: cpu count 1, Reload check passes"
check "$L" 'KTEST skip cp.smp.limit_cpus' "cp.smp.limit_cpus skips"
check "$L" 'KTEST ok rb.reload.stage_synthetic' "RB-5a's staging ktest takes its one-CPU branch"
check "$L" 'KTEST done passed=[0-9]+ failed=0 ' "no ktest failed with smp=off"

# 2. cpus=4 under -smp 16.
run cpus4 "cpus=4" "cp.smp,cp.ist,cp.cpu,cp.percpu,contract.percpu,cp.irq" -smp 16
L="$RUN/cpus4.log"
check "$L" 'smp limited max_cpus=4 held_back=12 ' "cpus=4: 12 APs held back"
check "$L" 'smp done cpus=4 planned=15 up=3 failed=0 .*held_back=12' "4 CPUs online, 3 APs up"
n=$(count "$L" 'cpu ap.up cpu=[0-9]+ ')
[ "$n" = 3 ] && echo "ok   3 'cpu ap.up' events" || { echo "FAIL $n 'cpu ap.up' events, wanted 3"; fail=1; }
check "$L" 'KTEST ok cp.smp.limit_cpus' "cp.smp.limit_cpus: tail held back, every online AP answers"
check "$L" 'KTEST ok cp.smp.all_up' "cp.smp.all_up accepts the held-back APs"
check "$L" 'KTEST ok cp.smp.call_every_cpu' "cross-CPU calls on the 3 APs"
check "$L" 'KTEST skip cp.smp.limit_off' "cp.smp.limit_off skips"
check "$L" 'KTEST done passed=[0-9]+ failed=0 ' "no ktest failed with cpus=4"

# 3. A bad number means one CPU, never all.
run bad "cpus=abc" "cp.smp.limit,cp.smp.all_up" -smp 4
L="$RUN/bad.log"
check "$L" 'smp config.bad problem="?cpus_not_a_number"?' "cpus=abc: warning"
check "$L" 'smp not_started reason="?CONFIG cpus= not usable: one CPU"?' "cpus=abc: one CPU, the reason names cpus="
check "$L" 'KTEST ok cp.smp.limit_off' "cp.smp.limit_off holds for the bad value"
check "$L" 'KTEST done passed=[0-9]+ failed=0 ' "no ktest failed with cpus=abc"

# 4. Both values bad: smp= decides (one CPU), each problem is logged with its hint.
run badsmp "smp=of;cpus=0" "cp.smp.limit,cp.smp.all_up" -smp 4
L="$RUN/badsmp.log"
check "$L" 'smp config.bad problem="?smp_unknown"?.*hint="?smp= takes on or off; use cpus=N' "smp=of: warning with the cpus=N hint"
check "$L" 'smp config.bad problem="?cpus_zero"?' "cpus=0: warning"
check "$L" 'smp not_started reason="?CONFIG smp= not understood: one CPU"? offline=3' "smp=of: one CPU, the reason does not claim smp=off"
n=$(count "$L" 'cpu ap.up ')
[ "$n" = 0 ] && echo "ok   no AP started with smp=of" || { echo "FAIL $n 'cpu ap.up' events with smp=of"; fail=1; }
check "$L" 'KTEST ok cp.smp.limit_off' "cp.smp.limit_off holds for smp=of"
check "$L" 'KTEST done passed=[0-9]+ failed=0 ' "no ktest failed with smp=of;cpus=0"

# 5. A limit above the MADT's count: every AP starts, nothing held back, no `smp limited`.
run big "cpus=64" "cp.smp.limit,cp.smp.all_up,cp.smp.call_every_cpu" -smp 4
L="$RUN/big.log"
check "$L" 'smp done cpus=4 planned=3 up=3 failed=0 .*held_back=0' "cpus=64 under -smp 4: all 4 CPUs"
n=$(count "$L" 'smp limited ')
[ "$n" = 0 ] && echo "ok   no 'smp limited' event" || { echo "FAIL $n 'smp limited' events with cpus=64"; fail=1; }
n=$(count "$L" 'smp config.bad ')
[ "$n" = 0 ] && echo "ok   no warning for cpus=64" || { echo "FAIL 'smp config.bad' with cpus=64"; fail=1; }
check "$L" 'KTEST ok cp.smp.limit_cpus' "cp.smp.limit_cpus: min(64, 4) online"
check "$L" 'KTEST ok cp.smp.all_up' "cp.smp.all_up with cpus=64"
check "$L" 'KTEST done passed=[0-9]+ failed=0 ' "no ktest failed with cpus=64"

# 6. cpus=1 with smp on: the BSP alone, the reason names cpus=1.
run one "smp=on;cpus=1" "cp.smp.limit,cp.smp.all_up" -smp 4
L="$RUN/one.log"
check "$L" 'smp not_started reason="?CONFIG cpus=1"? offline=3' "cpus=1: one CPU"
n=$(count "$L" 'smp config.bad ')
[ "$n" = 0 ] && echo "ok   no warning for cpus=1" || { echo "FAIL 'smp config.bad' with cpus=1"; fail=1; }
check "$L" 'KTEST ok cp.smp.limit_off' "cp.smp.limit_off holds for cpus=1"
check "$L" 'KTEST skip cp.smp.limit_cpus' "cp.smp.limit_cpus skips for cpus=1"
check "$L" 'KTEST done passed=[0-9]+ failed=0 ' "no ktest failed with cpus=1"

# 7. smp=off wins over cpus=8.
run offwins "smp=off;cpus=8" "cp.smp.limit,cp.smp.all_up" -smp 4
L="$RUN/offwins.log"
check "$L" 'smp not_started reason="?CONFIG smp=off"? offline=3' "smp=off;cpus=8: smp=off wins"
n=$(count "$L" 'smp (config.bad|limited) ')
[ "$n" = 0 ] && echo "ok   no warning, no 'smp limited'" || { echo "FAIL $n 'smp config.bad/limited' events"; fail=1; }
check "$L" 'KTEST ok cp.smp.limit_off' "cp.smp.limit_off holds for smp=off;cpus=8"
check "$L" 'KTEST done passed=[0-9]+ failed=0 ' "no ktest failed with smp=off;cpus=8"
exit $fail
