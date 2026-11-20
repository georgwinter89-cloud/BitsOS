#!/bin/bash
# slow
# DB2 (QEMU): the access log end to end. /bin/access-probe (user/c/access-probe.c) runs as uid 1000 through MCP:
# it reads itself, makes a folder, makes/renames/removes a file and a folder in it, listens on 127.0.0.1, connects
# to itself and sends 5 bytes, and opens the microphone node (else /dev/ntsync). bitsaccess (init starts it as
# root) must answer on /run/bitsos/access.sock (C5, asked with `bitsaccess query` as uid 1000) and through the MCP
# tool access_log exactly that for the probe; with access.enabled off the kernel records nothing. werhatwas_check.py.
# Ports: lane range + 70 (MCP forward).
set -eu
export LC_ALL=C
for t in python3 mcopy mkfs.fat; do command -v $t >/dev/null || { echo "RED: $t missing"; exit 1; }; done
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/ab-30-werhatwas}"
mkdir -p "$RUN_DIR"
RUN_DIR="$(realpath "$RUN_DIR")"
LO="${BITSOS_PORT_LO:-23500}"
FWD=$((LO + 70))
TOKEN="db2-scenario-token-5e1a"
LOG="$RUN_DIR/serial.log"
NAME=ab-werhatwas

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

cargo xtask image --name "$NAME" --config-extra "token=$TOKEN;mcp_port=8765;loglevel=info;nvme_write=off;data_img_mb=256"
IMG="target/xtask/$NAME.img"
[ -f "$IMG" ] || { echo "RED: no image $IMG"; exit 1; }
# Room for data.img (256 MiB) and the 512 MiB V2 keeps free: the FAT image made anew from xtask's ESP tree, larger
# (as cx/20-ctxd-v2.sh does).
kib=$(( $(stat -c %s "$IMG") / 1024 + 900 * 1024 ))
rm -f "$IMG"
mkfs.fat -F 32 -n BITSOS -C "$IMG" "$kib" > /dev/null
mcopy -s -i "$IMG" "target/xtask/esp-$NAME/EFI" "target/xtask/esp-$NAME/BITSOS2" ::/
rm -f "$LOG"
cargo xtask qemu --name "$NAME" --net virtio --hostfwd "$FWD:8765" --timeout 900 --smp 4 --mem 4G --serial-log "$LOG" \
    > "$RUN_DIR/qemu.out" 2>&1 &
QPID=$!
python3 -I host/tests/ab/werhatwas_check.py "$FWD" "$TOKEN" "$RUN_DIR"
if grep -aq "PANIC" "$LOG"; then echo "RED: the kernel panicked"; tail -20 "$LOG"; exit 1; fi
grep -aq "started the access log /bin/bitsaccess" "$LOG" || { echo "RED: init never said it started bitsaccess"; exit 1; }
echo "PASS: the access log shows what the probe did, and nothing while switched off"
