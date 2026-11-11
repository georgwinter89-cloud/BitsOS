#!/bin/bash
# slow
# NV-2p: RB's Reload scenario (ten reloads over MCP, 16 CPUs, igb) with /data on the window of an NVMe image built
# like Georg's Lexar SSD. Each kernel checks the GPT again, arms the window and mounts the pool; each Reload writes back,
# flushes (`block shutdown.disk disk=nvme0n1`) and quiesces the controller (normal shutdown, RDY=0, bus master off)
# first. A file written before the last Reload, with no sync, is back after it. Every byte outside the window is what
# it was; the pool is clean for the host's e2fsck.
# Ports: NV's range (BITSOS_PORT_LO, default 19000).
set -eu
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/nv-41-window-reload}"
mkdir -p "$RUN_DIR"
RUN_DIR="$(realpath "$RUN_DIR")"
NV="$(pwd)/host/tests/nv"
RB="$(pwd)/host/tests/rb"
LO="${BITSOS_PORT_LO:-19000}"
MCP_FWD=$((LO + 300))
HTTP=$((LO + 310))
TOKEN="nv2p-reload-token-a41c"
LOG="$RUN_DIR/serial.log"
NAME=nv-win-reload
SERIAL=LEXARQEMUZY1234
SMP="${NV_SMP:-16}"
MKIMG() { python3 -I "$NV/mkimg.py" "$@"; }
fail() { echo "RED: $*"; exit 1; }

IMG="$RUN_DIR/real.img"
BLOCKS=4194304 # 2 GiB, MSR, Windows data, 1 GiB raw, a free rest
FIRST=1048576
N=2097152
MKIMG real "$IMG" $BLOCKS 512 8 msr:2048:34815 msdata:34816:$((FIRST - 1)):ntfs msdata:$FIRST:$((FIRST + N - 1))
KEY="$SERIAL:$(MKIMG guid 3):$FIRST:$N"
OUT=$(MKIMG outside "$IMG" $FIRST $N 512)

# The pool first, as the window test makes it (a boot of its own: ktest image).
cargo xtask image --name "$NAME" --ktest --config-extra "ktest=nv.window.pool_first;ktest_run=nv/41-window-reload;loglevel=info;nvme_write=on;nvme_data=$KEY;data_img_mb=64" > "$RUN_DIR/image.out" 2>&1 || { tail -20 "$RUN_DIR/image.out"; fail "image"; }
stick="target/xtask/$NAME.img"
kib=$(( $(stat -c %s "$stick") / 1024 + 700 * 1024 ))
rm -f "$stick"
mkfs.fat -F 32 -n BITSOS -C "$stick" "$kib" > /dev/null
mcopy -s -i "$stick" "target/xtask/esp-$NAME/EFI" "target/xtask/esp-$NAME/BITSOS2" ::/
cargo xtask qemu --name "$NAME" --smp 4 --timeout 300 --serial-log "$RUN_DIR/pool.log" -- \
    -drive "if=none,id=nv0,format=raw,file=$IMG,discard=unmap" -device nvme,id=nvq0,serial=$SERIAL,drive=nv0 > "$RUN_DIR/qemu0.out" 2>&1 || true
grep -aq "KTEST ok nv.window.pool_first " "$RUN_DIR/pool.log" || { grep -a "KTEST\|window" "$RUN_DIR/pool.log" | tail; fail "the pool was not made"; }

# C1 (INT-W27b): squeezed like rb/20-reload.sh.
export RB_SQUEEZE_MB=${RB_SQUEEZE_MB:-384}
export RB18_PSK="rb18-fake-psk-not-real"
cargo xtask image --name "$NAME" --config-extra "token=$TOKEN;wlan_psk=$RB18_PSK;mcp_port=8765;loglevel=info;rb_squeeze_mb=$RB_SQUEEZE_MB;rb_choke_mb=10;nvme_write=on;nvme_data=$KEY;data_img_mb=0"
BUNDLES="$RUN_DIR/bundles"
rm -rf "$BUNDLES"
mkdir -p "$BUNDLES/base"
cp "target/xtask/esp-$NAME/BITSOS2/SYS/1/KERNEL" "target/xtask/esp-$NAME/BITSOS2/SYS/1/INITRD" "$BUNDLES/base/"
python3 -I -m http.server --bind 127.0.0.1 --directory "$BUNDLES" "$HTTP" > "$RUN_DIR/http.log" 2>&1 &
HPID=$!
rm -f "$LOG"
cargo xtask qemu --name "$NAME" --smp "$SMP" --net igb --hostfwd "$MCP_FWD:8765" --timeout 700 --serial-log "$LOG" -- \
    -drive "if=none,id=nv0,format=raw,file=$IMG,discard=unmap" -device nvme,id=nvq0,serial=$SERIAL,drive=nv0 > "$RUN_DIR/qemu.out" 2>&1 &
QPID=$!
stop_qemu() {
    local qemu
    qemu=$(sed -n 's/^xtask: qemu pid \([0-9]*\).*/\1/p' "$RUN_DIR/qemu.out" 2>/dev/null | head -1)
    for p in "$QPID" $qemu; do
        if kill -0 "$p" 2>/dev/null; then kill "$p" 2>/dev/null || true; fi
    done
}
cleanup() {
    stop_qemu
    if kill -0 "$HPID" 2>/dev/null; then kill "$HPID" 2>/dev/null || true; fi
}
trap cleanup EXIT

python3 -I "$RB/reload_scenario.py" "$MCP_FWD" "$TOKEN" "$LOG" "$BUNDLES" "$HTTP" "$SMP"
stop_qemu
wait "$QPID" 2>/dev/null || true
# A file with no sync, a Reload, the file back: write-back and flush before the quiesce (a boot of its own: the
# scenario above leaves its kernel with its memory choked).
LOG2="$RUN_DIR/file.log"
cargo xtask image --name "$NAME" --config-extra "token=$TOKEN;mcp_port=8765;loglevel=info;nvme_write=on;nvme_data=$KEY;data_img_mb=0" > "$RUN_DIR/image.out" 2>&1 || { tail -20 "$RUN_DIR/image.out"; fail "image"; }
cp "target/xtask/esp-$NAME/BITSOS2/SYS/1/KERNEL" "target/xtask/esp-$NAME/BITSOS2/SYS/1/INITRD" "$BUNDLES/base/"
rm -f "$LOG2"
cargo xtask qemu --name "$NAME" --smp 4 --net igb --hostfwd "$MCP_FWD:8765" --timeout 300 --serial-log "$LOG2" -- \
    -drive "if=none,id=nv0,format=raw,file=$IMG,discard=unmap" -device nvme,id=nvq0,serial=$SERIAL,drive=nv0 > "$RUN_DIR/qemu.out" 2>&1 &
QPID=$!
python3 -I - "$MCP_FWD" "$TOKEN" "$HTTP" <<'PY' || fail "the file over the Reload"
import http.client, json, sys, time
port, tok, web = int(sys.argv[1]), sys.argv[2], sys.argv[3]
def conn(wait):
    end = time.time() + wait
    while True:
        try:
            c = http.client.HTTPConnection("127.0.0.1", port, timeout=120)
            rpc(c, "initialize", {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "nv", "version": "1"}})
            return c
        except (OSError, http.client.HTTPException, ValueError, KeyError):
            if time.time() > end:
                raise
            time.sleep(1)
def rpc(c, m, p):
    c.request("POST", "/mcp", json.dumps({"jsonrpc": "2.0", "id": 1, "method": m, "params": p}), {"Authorization": f"Bearer {tok}", "Content-Type": "application/json"})
    return json.loads(c.getresponse().read())["result"]
def tool(c, n, a):
    r = rpc(c, "tools/call", {"name": n, "arguments": a})
    return "\n".join(i.get("text", "") for i in r["content"]), r.get("isError", False)
c = conn(60)
for _ in range(120):
    t, _e = tool(c, "read_file", {"path": "/run/bitsos/data-state", "encoding": "text"})
    if "mounted pool rw" in t:
        break
    time.sleep(0.5)
assert "mounted pool rw" in t, t
t, e = tool(c, "run", {"path": "/bin/sh", "args": ["-c", "echo 'kept over the Reload' > /data/home/1000/nv-reload-file; echo status=$?"], "timeout_ms": 20000})
assert "status=0" in t, t
t, e = tool(c, "reload", {"url": f"http://10.0.2.2:{web}/base/", "timeout_ms": 60000})
assert not e, t
time.sleep(5)
c = conn(60)
for _ in range(120):
    t, _e = tool(c, "read_file", {"path": "/proc/bitsos/data-window", "encoding": "text"})
    if "window=armed" in t:
        break
    time.sleep(0.5)
assert "window=armed" in t, t
for _ in range(120):
    t, e = tool(c, "run", {"path": "/bin/sh", "args": ["-c", "cat /data/home/1000/nv-reload-file"], "timeout_ms": 20000})
    if "kept over the Reload" in t:
        break
    time.sleep(0.5)
assert "kept over the Reload" in t, t
print("ok: the file came back after the Reload")
PY
stop_qemu
wait "$QPID" 2>/dev/null || true
grep -a "nvme ctrl\.\(failed\|recover\)\|nvme window\.\|block window\.\|block shutdown.disk .*disk=nvme0n1\|kexec reload\.jump" "$LOG" | tail -30 | cut -c1-170
ARMED=$(grep -ac "nvme window.valid" "$LOG" || true)
[ "$ARMED" -ge 11 ] || fail "the window was armed $ARMED times, want 11 or more (boot + 10 reloads)"
FILE_FLUSH=$(grep -ac "block shutdown.disk why=reload disk=nvme0n1 .*flush=done" "$LOG2" || true)
[ "$FILE_FLUSH" -ge 1 ] || fail "the Reload with a written file did not flush nvme0n1"
grep -aq "nvme window.valid" "$LOG2" || fail "the file boot has no window"
FLUSHED=$(grep -ac "block shutdown.disk why=reload disk=nvme0n1 " "$LOG" || true)
[ "$FLUSHED" -ge 10 ] || fail "nvme0n1 was stopped $FLUSHED times"
if cat "$LOG" "$LOG2" | grep -aq "nvme ctrl.failed\|nvme ctrl.recover\|PANIC"; then fail "a failure line in the log"; fi
[ "$(MKIMG outside "$IMG" $FIRST $N 512)" = "$OUT" ] || fail "a byte outside the window changed"
dd if="$IMG" of="$RUN_DIR/pool.img" bs=1M iflag=skip_bytes,count_bytes skip=$((FIRST * 512)) count=$((N * 512)) conv=sparse status=none
BITSVOL_POOL_IMG="$RUN_DIR/pool.img" BITSVOL_OUT="$RUN_DIR/vol.img" cargo test -q -p nvme --lib tests::window::extract_volume -- --ignored --exact > "$RUN_DIR/extract.log" 2>&1 \
    || { cat "$RUN_DIR/extract.log"; fail "the pool does not open on the host"; }
e2fsck -fn "$RUN_DIR/vol.img" > "$RUN_DIR/fsck.log" 2>&1 || { cat "$RUN_DIR/fsck.log"; fail "e2fsck -fn is not clean"; }
rm -f "$RUN_DIR/pool.img" "$RUN_DIR/vol.img"
echo "GREEN: $ARMED kernels armed the window, $FLUSHED storage stops of nvme0n1, outside unchanged, e2fsck clean"
