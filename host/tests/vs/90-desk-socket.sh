#!/bin/bash
# slow
# REWIND on V2 (QEMU, KVM, igb): bitssnap serve's desktop socket (/run/bitsos/bitssnap.sock, user/tools/bitssnap/src/
# local.rs) on a 64 MiB test volume, through `bitssnap ask` started by MCP's `run` (uid 1000, not the compositor) and
# by root's console shell (QMP keys): list, browse and a dry run answer uid 1000; a restore without `confirmed` is
# refused, one with `confirmed` from a program that is not the compositor too, an undo and "everything" likewise,
# nothing changed by any of them; root's confirmed restore brings the file back (a before-restore snapshot first);
# another uid (1001, BusyBox chpst) is refused before anything is looked at; every restore and refusal is in the
# kernel log. desk_socket_check.py has every check. Ports: lane range + 90. Afterwards on the host: fsck.fat -n clean.
set -eu
export LC_ALL=C
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/vs-90-desk-socket}"
mkdir -p "$RUN_DIR"
RUN_DIR="$(realpath "$RUN_DIR")"
HERE="$(pwd)/host/tests/vs"
LO="${BITSOS_PORT_LO:-18000}"
FWD=$((LO + 90))
TOKEN="rewind-desk-token-4f1a"
NAME=vs-desk
IMG="target/xtask/$NAME.img"
fail() { echo "RED: $*"; exit 1; }
for t in python3 mcopy mkfs.fat fsck.fat; do command -v $t >/dev/null || fail "$t missing on this host (this scenario needs it)"; done

QPID=""
stop_qemu() {
    if [ -n "$QPID" ] && kill -0 "$QPID" 2>/dev/null; then
        local q; q=$(head -1 "target/xtask/qemu-$NAME.pid" 2>/dev/null || true)
        # The QEMU of this run only: its pid file, checked against its -name (v2-<lane>-<name>, any lane).
        if [ -n "$q" ] && tr '\0' ' ' < "/proc/$q/cmdline" 2>/dev/null | grep -Eq -- "-name v2-[A-Za-z0-9]+-$NAME( |$)"; then
            kill "$q" 2>/dev/null || true
        fi
        kill "$QPID" 2>/dev/null || true
        wait "$QPID" 2>/dev/null || true
    fi
    QPID=""
}
trap stop_qemu EXIT

cargo xtask image --name "$NAME" --config-extra "token=$TOKEN;mcp_port=8765;loglevel=info;nvme_write=off;data_img_mb=64" > "$RUN_DIR/image.out" 2>&1 \
    || { tail -20 "$RUN_DIR/image.out"; fail "cargo xtask image"; }
ESP="target/xtask/esp-$NAME"
CONTENT=$(du -sb "$ESP" | cut -f1)
# data.img (64 MiB) is made only when 512 MiB of the stick stay free (FS-8c KEEP_FREE).
KIB=$(( (CONTENT + (700 << 20)) / 1024 / 1024 * 1024 ))
rm -f "$IMG"
mkfs.fat -F 32 -n BITSOS -C "$IMG" "$KIB" > /dev/null
mcopy -m -s -i "$IMG" "$ESP/EFI" "$ESP/BITSOS2" ::/

LOG="$RUN_DIR/serial.log"
SOCK="$RUN_DIR/q.qmp"
rm -f "$LOG" "$SOCK"
cargo xtask qemu --name "$NAME" --net igb --mem 4096 --hostfwd "$FWD:8765" --timeout 900 --serial-log "$LOG" \
    -- -machine i8042=off -device qemu-xhci,id=xb -device usb-kbd,bus=xb.0,port=1,id=kbd1 -qmp "unix:$SOCK,server=on,wait=off" \
    > "$RUN_DIR/qemu.out" 2>&1 &
QPID=$!
for _ in $(seq 100); do [ -S "$SOCK" ] && break; sleep 0.1; done
[ -S "$SOCK" ] || { cat "$RUN_DIR/qemu.out"; fail "QEMU did not start"; }
python3 -I "$HERE/desk_socket_check.py" "$FWD" "$TOKEN" "$LOG" "$SOCK"
stop_qemu
if grep -a "PANIC\|lock.violation" "$LOG"; then fail "a failure line in the log"; fi
FSCK=$(fsck.fat -n "$IMG" 2>&1) || fail "fsck.fat -n is not clean: $FSCK"
echo "vs-90-desk-socket: green"
