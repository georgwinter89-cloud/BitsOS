#!/bin/bash
# BL-0: the read-path measurement of plan/notes/desk-W35-mm-blk.md ("BL-0 procedure"), by the desk against the real
# Mini-PC over MCP (TLS). Usage:
#   bash host/tests/minipc/blk0.sh <host[:port]> --tls auto --token-file <F> [--label idle|x4] [--state BL-0] [blk0.py options]
#   bash host/tests/minipc/blk0.sh --proof     the same script against a QEMU guest of this lane (plain HTTP, a stick
#                                              data.img as /data, small files): proves the script itself, no numbers
# The kernel must have ktests (the cold step's drop_caches 3 is ktest fs.drop_caches.all) and /bin/blkbench. It refuses
# to measure unless /data is the Lexar partition (blk0.py). What it prints: blk0.py; output in target/minipc/blk0/.
set -euo pipefail
cd "$(dirname "$0")/../../.."
if [ "${1:-}" != "--proof" ]; then
    host="${1:?the Mini-PC: <host[:port]> --tls auto --token-file <F>, or --proof}"
    shift
    exec python3 -I host/tests/minipc/blk0.py "$host" target/minipc/blk0 "$@"
fi

# ---- --proof: a QEMU guest on BL's ports (BITSOS_PORT_LO, default 14000), a stick with room for a 96 MiB data.img.
RUN="${BITSOS_RUN_DIR:-target/xtask/runs/minipc-blk0-proof}"
mkdir -p "$RUN"
RUN="$(realpath "$RUN")"
# shellcheck source=host/tests/mcp/lib.bash
. "$(pwd)/host/tests/mcp/lib.bash"
LO="${BITSOS_PORT_LO:-14000}"
FWD=$((LO + 40))
NAME="blk0-proof"
IMG="target/xtask/$NAME.img"
export BITSOS_TOKEN="blk0-proof-token"
fail() { echo "RED: $*"; exit 1; }
python3 -I host/tests/minipc/blk0.py --selftest
cargo xtask image --name "$NAME" --config-extra "token=$BITSOS_TOKEN;mcp_port=8765;loglevel=info;data_img_mb=96"
kib=$(( $(stat -c %s "$IMG") / 1024 + 700 * 1024 ))
rm -f "$IMG"
mkfs.fat -F 32 -n BITSOS -C "$IMG" "$kib" > /dev/null
mcopy -s -i "$IMG" "target/xtask/esp-$NAME/EFI" "target/xtask/esp-$NAME/BITSOS2" ::/
stop_qemu "$NAME"
LOG="$RUN/serial.log"
rm -f "$LOG"
cargo xtask qemu --name "$NAME" --mem 3G --net virtio --hostfwd "$FWD:8765" --timeout 1500 --serial-log "$LOG" -- -smp 4 > "$RUN/qemu.out" 2>&1 &
QPID=$!
trap 'stop_run "$QPID" "$NAME"' EXIT
for _ in $(seq 1 600); do grep -aq "ext4 data.mounted" "$LOG" 2>/dev/null && break; sleep 0.5; done
grep -aq "ext4 data.mounted" "$LOG" || fail "/data was not mounted in time (see $LOG)"
OUT="$RUN/out"
set +e
python3 -I host/tests/minipc/blk0.py "127.0.0.1:$FWD" "$OUT" --label refused > "$RUN/refused.txt" 2>&1
code=$?
set -e
cat "$RUN/refused.txt"
[ "$code" = 2 ] && grep -q "data.img" "$RUN/refused.txt" || fail "blk0.py measured on the stick's data.img (exit $code)"
echo "ok: blk0.py refuses the stick's data.img"
python3 -I host/tests/minipc/blk0.py "127.0.0.1:$FWD" "$OUT" --label qemu --allow-stick-data --prepare --dir /data/home/1000/bench \
    --seq-mb 32 --rand-mb 8 --small 40 --runs 1 --ceiling-mb 8 | tee "$RUN/measured.txt"
grep -q "^GREEN" "$RUN/measured.txt" || fail "blk0.py did not measure"
for k in "test=seq" "test=rand" "test=small" "test=mmap" "resident_pages=0" "block.copy_bytes" "row:"; do
    grep -q -- "$k" "$RUN/measured.txt" || fail "no '$k' in the measurement"
done
echo "ok: blk0.py measures the whole sequence"
# The ABI half on a block file system (abi-fadvise dir): V2's ext4 /data against CachyOS's disk file system.
ABI="target/xtask/abi-diff/bin/abi-fadvise"
[ -x "$ABI" ] || fail "$ABI missing: run cargo xtask abi-diff abi-fadvise first"
SCR="$RUN/abi"
rm -rf "$SCR" && mkdir -p "$SCR"
"$ABI" dir "$SCR" > "$RUN/abi-dir.linux.txt"
python3 -I - "127.0.0.1:$FWD" "$RUN/abi-dir.v2.txt" <<'EOF'
import json, os, sys
sys.path.insert(0, "host/tests/minipc")
import mp
o = mp.options([sys.argv[1]], positional=1)
err, text, _ = mp.call(o, "run", {"path": "/bin/abi-fadvise", "args": ["dir", "/data/home/1000"], "timeout_ms": 60000})
body = text.split("--- stdout (", 1)[1].split(") ---\n", 1)[1].split("--- stderr (", 1)[0] if not err and "--- stdout (" in text else text
open(sys.argv[2], "w").write(body.rstrip("\n") + "\n")
EOF
diff "$RUN/abi-dir.linux.txt" "$RUN/abi-dir.v2.txt" || fail "abi-fadvise dir: V2's ext4 differs from CachyOS"
echo "ok: abi-fadvise dir is the same on V2's ext4 /data and CachyOS"
echo "GREEN: blk0.sh --proof"
