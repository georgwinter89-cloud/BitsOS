#!/bin/bash
# slow
# CX-5 on V2 (QEMU): the context service ctxd as the session's service. init starts /bin/ctxd serve --system as
# uid 1000; nothing is indexed until a folder is named in the settings; the German test set (host/cx-eval, plus
# four marked copies for the speed number) and the e5-small model come on the stick image (BITSOS2/CXTEST) and
# are copied by uid 1000 the way the desk will do it on the Mini-PC. ctxd_v2_check.py drives it through MCP.
# Ports: lane range + 60 (MCP forward). Needs the model file: user/services/ctxd/third_party/models/ (as
# 10-ctxd.sh finds it) or CTXD_MODEL=<file>.
set -eu
export LC_ALL=C
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
ROOT="$(pwd)"
for t in python3 mcopy mkfs.fat; do command -v $t >/dev/null || { echo "RED: $t missing"; exit 1; }; done
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/cx-20-ctxd-v2}"
mkdir -p "$RUN_DIR"
RUN_DIR="$(realpath "$RUN_DIR")"
LO="${BITSOS_PORT_LO:-29500}"
FWD=$((LO + 60))
TOKEN="cx5-scenario-token-31c7"
LOG="$RUN_DIR/serial.log"
# xtask takes only sockets below target/ (54 bytes here; a unix socket path must stay under 108).
QMP="$RUN_DIR/qmp.sock"
NAME=cx-ctxd
IMG="target/xtask/$NAME.img"

MODEL="${CTXD_MODEL:-}"
for m in "$MODEL" user/services/ctxd/third_party/models/multilingual-e5-small-q8_0.gguf \
         /var/tmp/claude-v2/cx/user/services/ctxd/third_party/models/multilingual-e5-small-q8_0.gguf; do
    if [ -n "$m" ] && [ -f "$m" ]; then MODEL="$m"; break; fi
done
[ -f "$MODEL" ] || { echo "RED: no e5-small model file (CTXD_MODEL, or user/services/ctxd/third_party/models/)"; exit 1; }

QPID=""
stop() {
    [ -n "$QPID" ] && kill "$QPID" 2>/dev/null || true
    pf="target/xtask/qemu-$NAME.pid"
    if [ -f "$pf" ]; then
        pid="$(head -1 "$pf")"
        if tr '\0' ' ' < "/proc/$pid/cmdline" 2>/dev/null | grep -Eq -- "-name v2-[A-Za-z0-9]+-$NAME( |$)"; then kill "$pid" 2>/dev/null || true; fi
    fi
    [ -n "$QPID" ] && wait "$QPID" 2>/dev/null || true
}
trap stop EXIT

# The test documents: the set once, and four copies with one marked line each (604 more documents).
T="$RUN_DIR/cxtest"
rm -rf "$T"
mkdir -p "$T/docs" "$T/mehr"
cp host/cx-eval/docs/* "$T/docs/"
for k in 1 2 3 4; do
    mkdir -p "$T/mehr/kopie-$k"
    for f in host/cx-eval/docs/*; do
        { cat "$f"; printf '\nKopie %s\n' "$k"; } > "$T/mehr/kopie-$k/$(basename "$f")"
    done
done
cp host/cx-eval/topics.json "$T/topics.json"
cp "$MODEL" "$T/e5.gguf"

cargo xtask image --name "$NAME" --config-extra "token=$TOKEN;mcp_port=8765;loglevel=info;nvme_write=off;data_img_mb=1024"
[ -f "$IMG" ] || { echo "RED: no image $IMG"; exit 1; }
# Room for data.img (1 GiB: the model, its copy, the index), the 512 MiB V2 keeps free and the test tree:
# the FAT image is made anew from xtask's ESP tree, larger (as fs/50-data.sh does).
kib=$(( $(stat -c %s "$IMG") / 1024 + 1900 * 1024 ))
rm -f "$IMG"
mkfs.fat -F 32 -n BITSOS -C "$IMG" "$kib" > /dev/null
mcopy -s -i "$IMG" "target/xtask/esp-$NAME/EFI" "target/xtask/esp-$NAME/BITSOS2" ::/
mmd -i "$IMG" ::BITSOS2/CXTEST
mcopy -s -i "$IMG" "$T/docs" "$T/mehr" "$T/topics.json" "$T/e5.gguf" ::BITSOS2/CXTEST/

rm -f "$LOG" "$QMP"
# The USB keyboard and QMP: root at the console names the folders an AI may see (only Georg's own yes
# may, settingsd leash.rs; as cl/30-assist.sh).
cargo xtask qemu --name "$NAME" --net virtio --hostfwd "$FWD:8765" --timeout 1100 --smp 4 --mem 6G --serial-log "$LOG" \
    -- -machine i8042=off -device qemu-xhci,id=xb -device usb-kbd,bus=xb.0,port=1,id=kbd1 -qmp "unix:$QMP,server=on,wait=off" \
    > "$RUN_DIR/qemu.out" 2>&1 &
QPID=$!
python3 -I host/tests/cx/ctxd_v2_check.py "$FWD" "$TOKEN" "$RUN_DIR" "$QMP"
if grep -aq "PANIC" "$LOG"; then echo "RED: the kernel panicked"; tail -20 "$LOG"; exit 1; fi
grep -aq "started the context service /bin/ctxd as uid 1000" "$LOG" || { echo "RED: init never said it started ctxd"; exit 1; }
grep -a "ctxd \|context service" "$LOG" | tail -12 || true
echo "ctxd on V2 (QEMU): green"
