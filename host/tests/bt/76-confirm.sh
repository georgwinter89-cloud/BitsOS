#!/bin/bash
# slow
# VB-1: "Version N läuft. Bestätigen?" against the real kernel under QEMU (KVM, igb, the stick on usb-storage, a
# USB keyboard and mouse). The image is version 165's twice: slot 164 (OK) and slot 165 (unmarked, its one try).
# Two boots, each from a fresh image (driver: confirm_scenario.py, its docstring lists every check):
#   ja       the kernel refuses an answer without real input; the console's question; MCP's injected J does
#            nothing; the USB keyboard's J confirms (by=georg); on the host SYS\165\OK is there.
#   zurueck  the USB keyboard's Z pins 164 and restarts; bitsboot starts 164 (PINNED); on the host PIN names 164
#            and 165 has no OK.
# QEMU has no desktop (Georg 18.11.): the desktop's card and its rule (only Georg's own hand after the arming) are
# bitscomp's host test confirm_note::only_georgs_own_hand_after_the_arming_answers; the report page's keys are the
# ktest gp.report.ask_real_keys.
set -euo pipefail
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
export MTOOLS_SKIP_CHECK=1
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/bt-76-confirm}"
mkdir -p "$RUN_DIR"
RUN_DIR="$(realpath "$RUN_DIR")"
HERE="$(pwd)/host/tests/bt"
# shellcheck source=host/tests/mcp/lib.bash
. "$(pwd)/host/tests/mcp/lib.bash"
LO="${BITSOS_PORT_LO:-20500}"
MCP_FWD=$((LO + 176))
TOKEN="bt76-confirm-scenario-5e21"
NAME=bt-confirm
fail() { echo "RED: $*"; exit 1; }

QPID=""
cleanup() { [ -n "$QPID" ] && stop_run "$QPID" "$NAME"; return 0; }
trap cleanup EXIT

BITSOS_VERSION=165 BITSOS_VERSION_DATE="$(date -u +%F)" V2_LEAN=1 \
    cargo xtask image --name "$NAME" --config-extra "token=$TOKEN;mcp_port=8765;loglevel=info;boot_confirm_s=600;sys_keep=3;stick_write=off" | tail -1
ESP="target/xtask/esp-$NAME"
IMG="target/xtask/$NAME.img"
SRC=$(ls -d "$ESP/BITSOS2/SYS/"*/ | head -1)
mkdir -p "$RUN_DIR/files"
cp "$SRC/KERNEL" "$SRC/INITRD" "$RUN_DIR/files/"
grep -aq "BITSOS-VERSION=00000165" "$RUN_DIR/files/KERNEL" || fail "the kernel is not marked version 165"

fresh() { # the stick: 164 OK, 165 unmarked
    rm -rf "$ESP/BITSOS2/SYS"
    mkdir -p "$ESP/BITSOS2/SYS/164" "$ESP/BITSOS2/SYS/165"
    cp "$RUN_DIR/files/KERNEL" "$RUN_DIR/files/INITRD" "$ESP/BITSOS2/SYS/164/"
    cp "$RUN_DIR/files/KERNEL" "$RUN_DIR/files/INITRD" "$ESP/BITSOS2/SYS/165/"
    touch "$ESP/BITSOS2/SYS/164/OK"
    local content kib
    content=$(du -sb "$ESP" | cut -f1)
    kib=$(( (content * 2 + (96 << 20)) / 1024 / 1024 * 1024 ))
    rm -f "$IMG"
    mkfs.fat -F 32 -n BITSOS -C "$IMG" "$kib" > /dev/null
    mcopy -s -i "$IMG" "$ESP/EFI" "$ESP/BITSOS2" ::/
}

run() { # mode
    local log="$RUN_DIR/$1.log" sock="$RUN_DIR/qmp-$1.sock"
    fresh
    rm -f "$log" "$sock"
    stop_qemu "$NAME"
    cargo xtask qemu --name "$NAME" --net igb --mem 4096 --hostfwd "$MCP_FWD:8765" --timeout 900 --serial-log "$log" \
        -- -action reboot=reset -qmp "unix:$sock,server,nowait" -device usb-kbd,bus=xhci.0 -device usb-mouse,bus=xhci.0 > "$RUN_DIR/qemu-$1.out" 2>&1 &
    QPID=$!
    for _ in $(seq 100); do [ -S "$sock" ] && break; sleep 0.1; done
    python3 -I "$HERE/confirm_scenario.py" "$1" "$MCP_FWD" "$TOKEN" "$log" "$sock" || { stop_run "$QPID" "$NAME"; QPID=""; fail "mode $1"; }
    stop_run "$QPID" "$NAME"
    QPID=""
    if grep -a "PANIC\|lock.violation" "$log"; then fail "a failure line in the $1 log"; fi
}

run ja
mdir -b -i "$IMG" ::/BITSOS2/SYS/165 | grep -q "/OK$" || fail "SYS\\165\\OK missing after Ja"
FSCK=$(fsck.fat -n "$IMG" 2>&1) || fail "fsck.fat -n is not clean after Ja: $FSCK"
echo "ok: on the host: SYS\\165\\OK written by Georg's Ja, fsck clean"

run zurueck
[ "$(mtype -i "$IMG" ::/BITSOS2/SYS/PIN | tr -d ' \r\n')" = "164" ] || fail "PIN does not name 164 after Zurück"
mdir -b -i "$IMG" ::/BITSOS2/SYS/165 | grep -q "/OK$" && fail "165 has OK after Zurück"
echo "ok: on the host: PIN 164, 165 unconfirmed"

rm -rf "$RUN_DIR/files"
echo "bt-76-confirm: green"
