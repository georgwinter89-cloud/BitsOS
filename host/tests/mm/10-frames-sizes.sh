#!/bin/bash
# MM-1: the frame allocator's ktests on other RAM sizes than the gate's 2G: 6G (a NORMAL zone above 4 GiB, so
# `mm.frames.normal_above_4g` must pass instead of skip) and 1G (small machine, DMA32 only; 512M made OVMF
# stall before bitsboot three times in a row under fullrun load).
# MM-2: the same boots check the kernel's own page tables: with 6G the direct map reaches RAM above 4 GiB
# (`mm.paging.direct_map` translates its top page) and uses 1 GiB pages when the CPU has them.
# MM-4: the DMA ktests run on both sizes too (UC/WC buffers split the direct map's 1 GiB pages with 6G).
set -eu
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/mm-10-frames-sizes}"
mkdir -p "$RUN_DIR"
RUN_DIR="$(realpath "$RUN_DIR")"
TESTS="mm.,contract.frame_alloc,contract.global_alloc,contract.dma_pool"

run() { # $1 = name, $2 = memory
    local log="$RUN_DIR/$1.log"
    cargo xtask image --name "$1" --ktest --config-extra "ktest=$TESTS;loglevel=info"
    cargo xtask qemu --name "$1" --mem "$2" --timeout 120 --serial-log "$log" || true
    grep -a "KTEST\|frames.init\|ktest.frames\|paging.switched\|ktest.direct_map" "$log" || true
    grep -aq "KTEST done passed=[0-9]* failed=0" "$log" || { echo "RED: $1 ($2) not green"; return 1; }
    if grep -aq "KTEST FAIL\|KTEST PANIC" "$log"; then echo "RED: $1 ($2)"; return 1; fi
}

run mm-6g 6G
grep -aq "KTEST ok mm.frames.normal_above_4g" "$RUN_DIR/mm-6g.log" || { echo "RED: normal_above_4g did not run with 6G"; exit 1; }
grep -aq "ktest.frames.* normal_mb=[1-9]" "$RUN_DIR/mm-6g.log" || { echo "RED: no NORMAL zone with 6G"; exit 1; }
grep -aq "KTEST ok mm.paging.direct_map" "$RUN_DIR/mm-6g.log" || { echo "RED: direct map test with 6G"; exit 1; }
grep -aq "ktest.direct_map.* above_4g_mb=[1-9]" "$RUN_DIR/mm-6g.log" || { echo "RED: direct map has no RAM above 4 GiB with 6G"; exit 1; }
# MM-4: 64-bit DMA buffers come from above 4 GiB when NORMAL has memory.
grep -aq "ktest.dma_mask.* dma64_above_4g=true" "$RUN_DIR/mm-6g.log" || { echo "RED: 64-bit DMA buffer not above 4 GiB with 6G"; exit 1; }
if grep -q pdpe1gb /proc/cpuinfo; then
    grep -aq "ktest.direct_map.* pages_1g=[1-9]" "$RUN_DIR/mm-6g.log" || { echo "RED: no 1 GiB page in the direct map with 6G"; exit 1; }
fi

run mm-1g 1G
echo "mm frames sizes: green"
