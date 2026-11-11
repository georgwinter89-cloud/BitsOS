#!/bin/bash
# slow
# time-limit: 2400
# NV-2p, the second half (the first: 40-window.sh): a 10 GiB sparse NVMe image built like Georg's Lexar SSD.
#  3. bitsdisk typed on QEMU's USB keyboard (window_check.py): refused over MCP in every way and with an injected or a
#     wrong code, then done; the next boot has /data on the partition and the stick's data.img read-only at /stickdata;
#     host/tests/minipc/data-move.sh moves its contents once, a second run refuses. `poweroff` typed at the keyboard.
#  4. nv.window.armed + nv.window.big: a 5 GiB file written, read and checked on that /data; the file from before the
#     power-off back; every byte outside the window as it was at the start; e2fsck clean.
# Ports: NV's range (BITSOS_PORT_LO, default 19000).
set -eu
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/nv-42-window-setup}"
mkdir -p "$RUN_DIR"
RUN_DIR="$(realpath "$RUN_DIR")"
NV="$(pwd)/host/tests/nv"
NV_RUN=nv/42-window-setup
. "$NV/window_lib.bash"

# ---- 3. bitsdisk on the big image: 10 GiB sparse, MSR, 2 GiB Windows, 7 GiB raw, 1 GiB free
BIG="$RUN_DIR/big.img"
BIG_BLOCKS=20971520
BIG_FIRST=4229120
BIG_N=14680064
MKIMG real "$BIG" $BIG_BLOCKS 512 6 msr:2048:34815 msdata:34816:4229119:ntfs msdata:$BIG_FIRST:$((BIG_FIRST + BIG_N - 1))
BIG_OUT=$(MKIMG outside "$BIG" $BIG_FIRST $BIG_N 512)
QMP="$RUN_DIR/qmp.sock"
rm -f "$QMP"
LOG="$RUN_DIR/setup.log"
BASE="token=$TOKEN;mcp_port=8765;loglevel=info;data_img_mb=64"
cargo xtask image --name "$NAME" --config-extra "$BASE;nvme_write=on" > "$RUN_DIR/image.out" 2>&1 || { tail -20 "$RUN_DIR/image.out"; fail "image"; }
grow_stick
NV_TIMEOUT=1200 start "$BIG" "$LOG" -machine i8042=off -device qemu-xhci,id=xb -device usb-kbd,bus=xb.0,port=1,id=kbd1 -qmp "unix:$QMP,server=on,wait=off"
check() { python3 -I "$NV/window_check.py" "$1" "$MCP_FWD" "$TOKEN" "$QMP" "$LOG" "$SERIAL" 3 "$BIG_FIRST" "$BIG_N"; }
check setup || { grep -a "nvme \|ext4 data\|window" "$LOG" | tail -40; fail "setup"; }
# The reset ended this QEMU (xtask runs it with -no-reboot): the same stick and NVMe image in a new one, a real boot.
wait "$QPID" || true
QPID=""
rm -f "$QMP"
LOG="$RUN_DIR/moved.log"
NV_TIMEOUT=1200 start "$BIG" "$LOG" -machine i8042=off -device qemu-xhci,id=xb -device usb-kbd,bus=xb.0,port=1,id=kbd1 -qmp "unix:$QMP,server=on,wait=off"
check moved || { grep -a "nvme \|ext4 data\|window" "$LOG" | tail -40; fail "moved"; }
IMG_SUM=$(cat "$RUN_DIR/data-img.sum")
TOKEN_FILE="$RUN_DIR/token"
printf %s "$TOKEN" > "$TOKEN_FILE"
bash host/tests/minipc/data-move.sh "127.0.0.1:$MCP_FWD" --token-file "$TOKEN_FILE" || { grep -a "ext4 data" "$LOG" | tail; fail "data-move"; }
# Once: the marker stands, the second run refuses.
if bash host/tests/minipc/data-move.sh "127.0.0.1:$MCP_FWD" --token-file "$TOKEN_FILE"; then fail "data-move ran twice"; fi
# Off, the machine's way: `poweroff` typed at the keyboard (R8f), a file on /data with no sync before it.
check poweroff || fail "power-off"
wait "$QPID" || true
QPID=""
grep -aq "block shutdown.disk why=[a-z]* disk=nvme0n1 " "$LOG" || fail "power-off: no storage stop of nvme0n1"
[ "$(MKIMG outside "$BIG" $BIG_FIRST $BIG_N 512)" = "$BIG_OUT" ] || fail "setup: a byte outside the window changed"
mcopy -n -i "target/xtask/$NAME.img" ::BITSOS2/data.img "$RUN_DIR/data.img" || fail "the stick has no data.img any more"
[ "$(sum "$RUN_DIR/data.img")" = "$IMG_SUM" ] || fail "data.img changed while it was /stickdata"
rm -f "$RUN_DIR/data.img"

# ---- 4. the 5 GiB file on that /data, and the file from before the power-off
# nv.zero_writes on an armed controller: its refusals hold, its counters move by right (the Mini-PC runs it so).
NV_EXTRA_TESTS=nv.zero_writes NV_EXTRA_RUN=,nv/10-nvme NV_MEM=8G NV_TIMEOUT=1500 ktests big "$BIG" "nvme_write=on;nvme_data=$SERIAL:$GUID3:$BIG_FIRST:$BIG_N" armed big
[ "$(MKIMG outside "$BIG" $BIG_FIRST $BIG_N 512)" = "$BIG_OUT" ] || fail "big: a byte outside the window changed"
# The ext4 inside the partition's pool, by the host's e2fsck (as host/tests/fs/50-data.sh).
dd if="$BIG" of="$RUN_DIR/pool.img" bs=1M iflag=skip_bytes,count_bytes skip=$((BIG_FIRST * 512)) count=$((BIG_N * 512)) conv=sparse status=none
BITSVOL_POOL_IMG="$RUN_DIR/pool.img" BITSVOL_OUT="$RUN_DIR/vol.img" cargo test -q -p nvme --lib tests::window::extract_volume -- --ignored --exact > "$RUN_DIR/extract.log" 2>&1 \
    || { cat "$RUN_DIR/extract.log"; fail "the pool does not open on the host"; }
e2fsck -fn "$RUN_DIR/vol.img" > "$RUN_DIR/fsck.log" 2>&1 || { cat "$RUN_DIR/fsck.log"; fail "e2fsck -fn is not clean"; }
rm -f "$RUN_DIR/pool.img" "$RUN_DIR/vol.img"
echo "GREEN: bitsdisk, /data on the partition, the move, 5 GiB; outside the window unchanged ($BIG_OUT)"
