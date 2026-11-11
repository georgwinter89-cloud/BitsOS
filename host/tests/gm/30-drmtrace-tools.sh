#!/bin/bash
# GM-1c: the trace tools on the host, no QEMU. drmtrace.py on made-up traces: a good one, one cut in the middle of a
# line, one with bad hex, and an INFO answer that differs from its reference: the damaged lines are listed with
# exit 1 and no traceback, the differing answer is counted. Then, only if the host has a render node, drmhostile
# --host under drmtrace.so (calls that make nothing on the GPU: GET_CAP, INFO into bad pointers, a 'd' ioctl on
# /dev/null, fork while a thread is in a DRM call) and drmreplay of that trace and of a damaged copy. No GEM, no
# context, no CS on the host: its GPU drives the desk's display.
set -euo pipefail
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/gm-30-drmtrace-tools}"
rm -rf "$RUN_DIR"
mkdir -p "$RUN_DIR"
RUN_DIR="$(realpath "$RUN_DIR")"
PY="python3 -I -B host/tests/gm/tools/drmtrace.py"
red() { echo "RED: $*"; exit 1; }

# Expects exit $1 from the rest of the line, its output in $RUN_DIR/out.txt, and no traceback.
expect() {
    local want=$1; shift
    set +e; "$@" > "$RUN_DIR/out.txt" 2>&1; local got=$?; set -e
    grep -q Traceback "$RUN_DIR/out.txt" && { cat "$RUN_DIR/out.txt"; red "a traceback from: $*"; }
    [ "$got" = "$want" ] || { cat "$RUN_DIR/out.txt"; red "exit $got (wanted $want) from: $*"; }
    return 0
}

# Made-up traces: GET_CAP and an INFO DEV_INFO with its answer (fd, numbers and bytes of the record format only).
python3 -I -B - "$RUN_DIR" <<'EOF'
import os, struct, sys
d = sys.argv[1]
cap = "I 1 fd=3 nr=0xc req=0xc010640c size=16 ret=0 errno=0 in=" + "13" + "00" * 15 + " out=" + "13" + "00" * 7 + "01" + "00" * 7
info_in = struct.pack("<QIIIIII", 0x1000, 8, 0x16, 0, 0, 0, 0).hex()
info = (f"I 2 fd=3 nr=0x45 req=0x40206445 size=32 ret=0 errno=0 in={info_in} out={info_in}\n"
        "P 2 info query=0x16 data=0019000001000000")
good = cap + "\n" + info + "\nC fd=3\n"
open(os.path.join(d, "good.txt"), "w").write(good)
open(os.path.join(d, "cut.txt"), "w").write(good[: len(cap) // 2] + "\n" + info + "\n")
open(os.path.join(d, "garbled.txt"), "w").write(good.replace("in=13", "in=1x", 1))
open(os.path.join(d, "same.info"), "w").write("Q 0x16 a=0,0,0,0 sz=8:0019000001000000\n")
open(os.path.join(d, "other.info"), "w").write("Q 0x16 a=0,0,0,0 sz=8:0019000002000000\n")
EOF
expect 0 $PY summary "$RUN_DIR/good.txt"
grep -q "2 ioctls" "$RUN_DIR/out.txt" || red "summary of the good trace: $(cat "$RUN_DIR/out.txt")"
for t in cut garbled; do
    expect 1 $PY summary "$RUN_DIR/$t.txt"
    grep -q "damaged lines: 1" "$RUN_DIR/out.txt" || red "the $t trace: no damaged line listed: $(cat "$RUN_DIR/out.txt")"
done
expect 0 $PY info "$RUN_DIR/good.txt" "$RUN_DIR/same.info"
grep -q "1 queries, 0 differ" "$RUN_DIR/out.txt" || red "info against the same answer: $(cat "$RUN_DIR/out.txt")"
expect 1 $PY info "$RUN_DIR/good.txt" "$RUN_DIR/other.info"
grep -q "1 queries, 1 differ" "$RUN_DIR/out.txt" || red "info against another answer: $(cat "$RUN_DIR/out.txt")"
expect 1 $PY info "$RUN_DIR/garbled.txt" "$RUN_DIR/same.info"
echo "ok: drmtrace.py lists damaged lines and differing INFO answers (exit 1, no traceback)"

W="-O2 -Wall -Wextra -Werror"
gcc $W -shared -fPIC -I/usr/include/libdrm -o "$RUN_DIR/drmtrace.so" host/tests/gm/tools/drmtrace.c -ldl -lpthread
gcc $W -I/usr/include/libdrm -o "$RUN_DIR/drmreplay" host/tests/gm/tools/drmreplay.c
gcc $W -I/usr/include/libdrm -o "$RUN_DIR/drmhostile" host/tests/gm/tools/drmhostile.c -lpthread
echo "ok: drmtrace.so, drmreplay and drmhostile build with -Werror"

NODE=/dev/dri/renderD128
if [ ! -c "$NODE" ] || [ ! -r "$NODE" ]; then
    echo "RED: no readable $NODE on this host: drmhostile --host and drmreplay cannot run"
    exit 1
fi
mkdir "$RUN_DIR/t"
expect 0 env DRMTRACE_FILE="$RUN_DIR/t/trace-%p.txt" LD_PRELOAD="$RUN_DIR/drmtrace.so" timeout 120 "$RUN_DIR/drmhostile" --host --node "$NODE"
cp "$RUN_DIR/out.txt" "$RUN_DIR/hostile.out"
grep -q "drmhostile: ok 5 checks" "$RUN_DIR/hostile.out" || red "drmhostile --host: $(cat "$RUN_DIR/hostile.out")"
MAIN=$(ls -S "$RUN_DIR"/t/trace-*.txt | head -1)
grep -q "^U [0-9]* fd=[0-9]* nr=0x45 " "$MAIN" || red "no U record for INFO with a bad argument pointer"
null=$(sed -n 's/^drmhostile: devnull fd=\([0-9]*\)$/\1/p' "$RUN_DIR/hostile.out")
! grep -q "^[IU] [0-9]* fd=$null " "$MAIN" || red "the 'd' ioctl on /dev/null (fd $null) was traced"
kids=0
for f in "$RUN_DIR"/t/trace-*.txt; do [ "$f" != "$MAIN" ] && [ "$(grep -c '^I ' "$f")" = 1 ] && kids=$((kids + 1)); done
[ "$kids" -ge 32 ] || red "only $kids of 32 fork children wrote their one DRM call to their own trace"
echo "ok: drmhostile --host under drmtrace: U record, /dev/null untraced, $kids fork children with their own trace"

expect 0 "$RUN_DIR/drmreplay" --quiet --node "$NODE" "$MAIN"
grep -q " 0 differ, .* 0 damaged" "$RUN_DIR/out.txt" || red "replay of the hostile trace: $(cat "$RUN_DIR/out.txt")"
head -40 "$MAIN" > "$RUN_DIR/head.txt"
sed '5s/ out=\([0-9a-f]\{8\}\)[0-9a-f]*/ out=\1/' "$RUN_DIR/head.txt" > "$RUN_DIR/head-cut.txt"
expect 1 "$RUN_DIR/drmreplay" --quiet --node "$NODE" "$RUN_DIR/head-cut.txt"
grep -q "not made: damaged" "$RUN_DIR/out.txt" && grep -q ", 1 damaged" "$RUN_DIR/out.txt" \
    || red "replay of a cut trace: $(cat "$RUN_DIR/out.txt")"
echo "ok: drmreplay plays the hostile trace (nothing differs) and calls a cut line damaged (exit 1)"
# GM-1d: the Mini-PC run's parsers (vulkaninfo, deqp-vk, the MCP picture) on made-up input.
python3 -I -B host/tests/gm/tools/test_vkdiff.py > "$RUN_DIR/vkdiff.txt" || { cat "$RUN_DIR/vkdiff.txt"; echo "RED: test_vkdiff.py"; exit 1; }
echo "ok: vkdiff.py and vk_check.py's picture reader"
echo "GREEN: drmtrace tools"
