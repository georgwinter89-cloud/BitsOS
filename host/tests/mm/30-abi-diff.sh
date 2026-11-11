#!/bin/bash
# MM-5a: the memory-call script (crates/mm/abi/mm-ops.txt) on CachyOS must still print what
# crates/mm/abi/linux-expected.txt says; the kernel compares itself with that file (ktest mm.as.abi_diff) and
# the pure model does too (cargo test -p mm). A difference here means the host kernel changed behaviour:
# look at the diff, then update the expected file and the MM code together.
# MM-5b: the same for the second script (madvise, mremap, mincore, shared anonymous memory; ktest mm.as.abi_diff2).
# Regenerate: cc -O1 -o /tmp/mmops host/tests/mm/abi/mmops.c && /tmp/mmops crates/mm/abi/mm-ops.txt > crates/mm/abi/linux-expected.txt
#             /tmp/mmops crates/mm/abi/mm-ops2.txt > crates/mm/abi/linux-expected2.txt
set -eu
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/mm-30-abi-diff}"
mkdir -p "$RUN_DIR"
cc -O1 -Wall -o "$RUN_DIR/mmops" host/tests/mm/abi/mmops.c
"$RUN_DIR/mmops" crates/mm/abi/mm-ops.txt > "$RUN_DIR/linux.txt"
if ! diff -u crates/mm/abi/linux-expected.txt "$RUN_DIR/linux.txt"; then
    echo "RED: this Linux ($(uname -r)) answers the memory script differently from linux-expected.txt"
    exit 1
fi
"$RUN_DIR/mmops" crates/mm/abi/mm-ops2.txt > "$RUN_DIR/linux2.txt"
if ! diff -u crates/mm/abi/linux-expected2.txt "$RUN_DIR/linux2.txt"; then
    echo "RED: this Linux ($(uname -r)) answers the second memory script differently from linux-expected2.txt"
    exit 1
fi
echo "mm abi diff: CachyOS $(uname -r) matches linux-expected.txt ($(wc -l < "$RUN_DIR/linux.txt") lines) and linux-expected2.txt ($(wc -l < "$RUN_DIR/linux2.txt") lines)"

# W35 MM-3 (MM-17): /proc/vmstat, /sys/kernel/mm/transparent_hugepage/defrag and the four compaction sysctls
# against this host's files. BitsOS's vmstat lines (crates/syscall/src/procfs/vmstat.rs LINES) must be Linux names
# in Linux's order; defrag must list the same five modes with one in brackets; each sysctl must have the mode
# BitsOS gives it (crates/syscall/src/procfs/sysctl.rs) and a value inside the range BitsOS accepts.
bits_names=$(sed -n '/^const LINES/,/^];/p' crates/syscall/src/procfs/vmstat.rs | grep -o '("[a-z_]*",' | tr -d '(",')
[ -n "$bits_names" ] || { echo "RED: no vmstat names found in vmstat.rs"; exit 1; }
cut -d' ' -f1 /proc/vmstat > "$RUN_DIR/host-vmstat-names.txt"
at=0
for n in $bits_names; do
    i=$(grep -nx "$n" "$RUN_DIR/host-vmstat-names.txt" | head -1 | cut -d: -f1)
    if [ -z "$i" ]; then echo "RED: /proc/vmstat on $(uname -r) has no line $n"; exit 1; fi
    if [ "$i" -lt "$at" ]; then echo "RED: $n comes before the previous BitsOS line on $(uname -r)"; exit 1; fi
    at=$i
done
defrag=$(cat /sys/kernel/mm/transparent_hugepage/defrag)
if [ "$(echo "$defrag" | tr -d '[]')" != "always defer defer+madvise madvise never" ] || [ "$(echo "$defrag" | grep -o '\[' | wc -l)" != 1 ]; then
    echo "RED: defrag reads '$defrag' on $(uname -r), BitsOS writes the five modes with one in brackets"
    exit 1
fi
for k in compact_memory:200 compact_unevictable_allowed:644 compaction_proactiveness:644 extfrag_threshold:644; do
    name=${k%%:*} mode=${k##*:}
    grep -q "SynthEntry::file(\"$name\", 0o$mode," crates/syscall/src/procfs/sysctl.rs || { echo "RED: sysctl.rs does not give $name mode 0$mode"; exit 1; }
    host=$(stat -c %a "/proc/sys/vm/$name")
    [ "$host" = "$mode" ] || { echo "RED: /proc/sys/vm/$name has mode $host on $(uname -r), BitsOS $mode"; exit 1; }
done
check_range() { v=$(cat "/proc/sys/vm/$1"); [ "$v" -ge 0 ] && [ "$v" -le "$2" ] || { echo "RED: vm.$1 = $v outside 0..=$2"; exit 1; }; }
check_range compaction_proactiveness 100
check_range extfrag_threshold 1000
check_range compact_unevictable_allowed 1
echo "mm abi diff: /proc/vmstat has BitsOS's $(echo $bits_names | wc -w) lines in order, defrag and the compaction sysctls match"
