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
