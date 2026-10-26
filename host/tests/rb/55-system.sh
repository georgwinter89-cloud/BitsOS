#!/bin/bash
# slow
# RB-9: system generations at run time under QEMU (KVM, igb, the stick on usb-storage), on image files of this run
# only. Driver: system_scenario.py (its docstring lists every check).
#   The stick starts with generations 1 and 2, both confirmed (OK; the image's kernel without its debug info, the
#   image's initramfs). system_list; system_pin 1 -> reset: bitsboot starts 1 (PINNED); system_pin off now ->
#   Reload into 2 (NEWEST_OK) read from the stick; system_pin 1 now -> Reload into 1 (PINNED); off -> reset: 2.
#   install_system refuses an initramfs without the early services; install_system of a broken kernel
#   (`jmp $` at its entry) as 3 -> reset: its one try hangs -> reset: bitsboot falls back to 2 by itself;
#   install_system of the good bundle as 4 (3 replaced; its own KERNEL, four/) -> reset: 4 comes up and confirms
#   itself (OK); a pin still goes back to 1; URL Reloads from there are named by their kernel id; off: 4 runs.
# Afterwards on the host: fsck.fat -n clean, no PIN, SYS holds 1, 2, 4 with 4 OK.
set -eu
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/rb-55-system}"
mkdir -p "$RUN_DIR"
RUN_DIR="$(realpath "$RUN_DIR")"
HERE="$(pwd)/host/tests/rb"
LO="${BITSOS_PORT_LO:-23000}"
MCP_FWD=$((LO + 140))
HTTP=$((LO + 150))
TOKEN="rb9-system-scenario-41c2"
BASE="http://10.0.2.2:$HTTP"
fail() { echo "RED: $*"; exit 1; }

if ! grep -q "kexec::set_boot_info" kernel/src/main.rs || ! grep -q "net::init" kernel/src/main.rs; then
    echo "SKIP: kernel/src/main.rs lacks kexec::set_boot_info or net::init"
    exit 0
fi
command -v objcopy > /dev/null || { echo "SKIP: no objcopy (the generations are stripped kernels)"; exit 0; }

QPID=""
HPID=""
NAME=rb-system
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
cleanup() {
    stop_qemu
    if [ -n "$HPID" ] && kill -0 "$HPID" 2>/dev/null; then kill "$HPID" 2>/dev/null || true; wait "$HPID" 2>/dev/null || true; fi
}
trap cleanup EXIT

# ---- the image: generations 1 and 2 confirmed, stripped kernels ----
cargo xtask image --name "$NAME" --config-extra "token=$TOKEN;mcp_port=8765;loglevel=info;boot_confirm_s=20;sys_keep=3;stick_write=off" | tail -1
ESP="target/xtask/esp-$NAME"
B="$RUN_DIR/bundles"
rm -rf "$B"
mkdir -p "$B/good" "$B/broken" "$B/noservices"
objcopy --strip-debug "$ESP/BITSOS2/SYS/1/KERNEL" "$B/good/KERNEL"
cp "$ESP/BITSOS2/SYS/1/INITRD" "$B/good/"
# RB-11: a kernel no generation has (the good one with one more section that is never loaded) for a URL Reload.
mkdir -p "$B/url"
printf 'rb-11\n' > "$RUN_DIR/rb11.note"
objcopy --add-section .rb11="$RUN_DIR/rb11.note" --set-section-flags .rb11=noload,readonly "$B/good/KERNEL" "$B/url/KERNEL"
cp "$B/good/INITRD" "$B/url/"
# RB-11 (fixer): generation 4 gets its own KERNEL (another never-loaded section), so a URL Reload of it from the
# pinned 1 is named by its kernel id as generation 4's kernel while the record still says 1.
mkdir -p "$B/four"
printf 'rb-11 four\n' > "$RUN_DIR/rb11-four.note"
objcopy --add-section .rb11="$RUN_DIR/rb11-four.note" --set-section-flags .rb11=noload,readonly "$B/good/KERNEL" "$B/four/KERNEL"
cp "$B/good/INITRD" "$B/four/"
cp "$B/good/KERNEL" "$ESP/BITSOS2/SYS/1/KERNEL"
rm -rf "$ESP/BITSOS2/SYS/2"
cp -r "$ESP/BITSOS2/SYS/1" "$ESP/BITSOS2/SYS/2"
touch "$ESP/BITSOS2/SYS/1/OK" "$ESP/BITSOS2/SYS/2/OK"
rm -f "$ESP/BITSOS2/SYS/PIN"
cp "$B/good/INITRD" "$B/broken/"
cp "$B/good/KERNEL" "$B/noservices/"
# The broken kernel: `jmp $` (EB FE) at the ELF entry (every check passes; it spins before it prints anything).
python3 -I - "$B/good/KERNEL" "$B/broken/KERNEL" <<'EOF'
import struct, sys
d = bytearray(open(sys.argv[1], "rb").read())
entry, phoff = struct.unpack_from("<Q", d, 24)[0], struct.unpack_from("<Q", d, 32)[0]
phnum = struct.unpack_from("<H", d, 56)[0]
for i in range(phnum):
    t, fl, off, va, pa, fs, ms, al = struct.unpack_from("<IIQQQQQQ", d, phoff + i * 56)
    if t == 1 and va <= entry < va + fs:
        at = off + entry - va
        d[at:at + 2] = b"\xeb\xfe"
        open(sys.argv[2], "wb").write(d)
        break
else:
    raise SystemExit("entry not in a loaded segment")
EOF
# An initramfs with init only (no settingsd, no bitssnap): install_system refuses it.
python3 -I - "$B/noservices/INITRD" <<'EOF'
import sys
def entry(name, body, mode):
    n = name.encode() + b"\0"
    h = b"070701" + b"".join(b"%08x" % v for v in (1, mode, 0, 0, 1, 0, len(body), 0, 0, 0, 0, len(n), 0))
    out = h + n
    out += b"\0" * (-len(out) % 4)
    out += body
    out += b"\0" * (-len(out) % 4)
    return out
d = entry("bin", b"", 0o040755) + entry("bin/init", b"\x7fELF not really", 0o100755) + entry("TRAILER!!!", b"", 0)
open(sys.argv[1], "wb").write(d)
EOF
IMG="target/xtask/$NAME.img"
CONTENT=$(du -sb "$ESP" | cut -f1)
# Room for two more generations beside the two.
KIB=$(( (CONTENT * 2 + (96 << 20)) / 1024 / 1024 * 1024 ))
rm -f "$IMG"
mkfs.fat -F 32 -n BITSOS -C "$IMG" "$KIB" > /dev/null
mcopy -s -i "$IMG" "$ESP/EFI" "$ESP/BITSOS2" ::/
echo "image: $IMG ($((KIB / 1024)) MiB; generations 1 and 2 marked OK)"

python3 -I "$HERE/install_http.py" "$HTTP" "$B" > "$RUN_DIR/http.log" 2>&1 &
HPID=$!

LOG="$RUN_DIR/serial.log"
SOCK="$RUN_DIR/qmp.sock"
rm -f "$LOG" "$SOCK"
cargo xtask qemu --name "$NAME" --net igb --mem 4096 --hostfwd "$MCP_FWD:8765" --timeout 900 --serial-log "$LOG" \
    -- -action reboot=reset -qmp "unix:$SOCK,server,nowait" > "$RUN_DIR/qemu.out" 2>&1 &
QPID=$!
for _ in $(seq 100); do [ -S "$SOCK" ] && break; sleep 0.1; done
python3 -I "$HERE/system_scenario.py" "$MCP_FWD" "$TOKEN" "$LOG" "$SOCK" "$BASE"
stop_qemu
grep -a "generation\.chosen\|kexec system\.pin \|kexec reload\.generation_switched\|kernel start \|kexec boot\.confirmed" "$LOG" | cut -c1-200
FSCK=$(fsck.fat -n "$IMG" 2>&1) || fail "fsck.fat -n is not clean: $FSCK"
echo "$FSCK" | tail -2
SYS=$(mdir -b -i "$IMG" ::/BITSOS2/SYS | tr -d '\r' | sed 's#/$##; s#.*/##' | grep -v '^$' | sort | tr '\n' ' ')
[ "$SYS" = "1 2 4 " ] || fail "SYS is not 1 2 4: $SYS"
mdir -b -i "$IMG" ::/BITSOS2/SYS/4 | grep -q "/OK$" || fail "SYS\\4\\OK missing"
mtype -i "$IMG" ::/BITSOS2/SYS/4/KERNEL | cmp -s - "$B/four/KERNEL" || fail "SYS\\4\\KERNEL is not the good kernel (four/)"
if grep -a "PANIC\|lock.violation" "$LOG"; then fail "a failure line in the log"; fi
echo "ok: on the host: fsck clean, no PIN, SYS holds 1 2 4, 4 confirmed with the good kernel"
echo "rb-55-system: green"
