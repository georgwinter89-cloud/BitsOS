#!/bin/bash
# slow
# RB-6: install_kernel and the kernel's own boot confirmation under QEMU (KVM, igb, the stick on usb-storage), on
# image files of this run only. Driver: install_scenario.py (its docstring lists every check).
#  1. cycle: generation 1 confirmed (OK) on a stick image with room for two generations (CONFIG stick_write=off);
#     refusals change nothing (CONFIG.TXT texts that flip stick_write, add nvme_write or drop the token; junk kernel; cut
#     initramfs; failed downloads: 404, body cut short, Content-Length over the limit; bad URLs and arguments; a
#     second install and a Reload while one install runs); a dry run; a deliberately broken kernel (the image's
#     kernel with `jmp $` at its entry) installed as 2, QMP system_reset: bitsboot gives 2 its one try (it hangs
#     before MCP), QMP system_reset: bitsboot falls back to 1; a generation whose kernel panics before MCP (its
#     initramfs carries the RB-6 test entry) installed as 3 (2 replaced), QMP system_reset: 3 panics,
#     panic=reboot resets the machine and bitsboot falls back to 1 by itself; the good kernel installed as 4 (3
#     replaced); reset: 4's one try, then a Reload: the reloaded kernel never confirms 4 (TRY/NEW stay), reset:
#     fallback to 1; the good kernel as 5 with a new CONFIG.TXT (4 replaced); reset: 5 comes up, confirms itself
#     after MCP answered over boot_confirm_s=20 (OK, TRY/NEW gone, 1 pruned by sys_keep=1); reset: 5 starts as
#     NEWEST_OK. Afterwards on the host: fsck.fat -n clean, SYS holds only 5 with the good kernel's bytes,
#     CONFIG.TXT as written, \EFI\ unchanged.
#  2. refused: a clone of the boot stick on a second USB port: the stick is read-only, install_kernel refuses,
#     neither image changes.
# The bundles come from a host HTTP server in RB's port range (install_http.py; guest: http://10.0.2.2:<port>/...).
set -eu
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/rb-50-install}"
mkdir -p "$RUN_DIR"
RUN_DIR="$(realpath "$RUN_DIR")"
HERE="$(pwd)/host/tests/rb"
LO="${BITSOS_PORT_LO:-23000}"
MCP_FWD=$((LO + 120))
HTTP=$((LO + 130))
TOKEN="rb6-install-scenario-7d1e"
BASE="http://10.0.2.2:$HTTP"
SUM='
import hashlib, sys
print(hashlib.sha256(open(sys.argv[1], "rb").read()).hexdigest())
'
sum() { python3 -I -c "$SUM" "$1"; }
fail() { echo "RED: $*"; exit 1; }

if ! grep -q "kexec::set_boot_info" kernel/src/main.rs || ! grep -q "net::init" kernel/src/main.rs; then
    echo "SKIP: kernel/src/main.rs lacks kexec::set_boot_info or net::init"
    exit 0
fi

QPID=""
HPID=""
NAME=""
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

# ---- the bundles ----
NAME=rb-install
cargo xtask image --name "$NAME" --config-extra "token=$TOKEN;mcp_port=8765;loglevel=info;boot_confirm_s=20;sys_keep=1;panic=reboot;panic_delay=1;stick_write=off" | tail -1
ESP="target/xtask/esp-$NAME"
B="$RUN_DIR/bundles"
rm -rf "$B"
mkdir -p "$B/good" "$B/broken" "$B/junk" "$B/cut" "$B/panic"
cp "$ESP/BITSOS2/SYS/1/KERNEL" "$ESP/BITSOS2/SYS/1/INITRD" "$B/good/"
cp "$ESP/BITSOS2/SYS/1/INITRD" "$B/broken/"
head -c 3000000 /dev/urandom > "$B/junk/KERNEL"
cp "$ESP/BITSOS2/SYS/1/INITRD" "$B/junk/"
python3 -I - "$ESP/BITSOS2/SYS/1/INITRD" "$B/cut/INITRD" <<'EOF'
import sys
d = open(sys.argv[1], "rb").read()
open(sys.argv[2], "wb").write(d[: len(d) // 2])
EOF
# The broken kernel: `jmp $` (EB FE) at the ELF entry, found through the program headers. Every check passes
# (bitsboot's and install_kernel's); the kernel spins before it prints anything.
python3 -I - "$ESP/BITSOS2/SYS/1/KERNEL" "$B/broken/KERNEL" <<'EOF'
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
        print(f"broken kernel: jmp $ at file offset {at:#x} (entry {entry:#x})")
        break
else:
    raise SystemExit("entry not in a loaded segment")
EOF
cmp -s "$B/good/KERNEL" "$B/broken/KERNEL" && fail "the broken kernel equals the good one"
# The panicking generation: the good kernel, its initramfs with one more archive holding the test entry.
cp "$ESP/BITSOS2/SYS/1/KERNEL" "$B/panic/"
python3 -I - "$ESP/BITSOS2/SYS/1/INITRD" "$B/panic/INITRD" <<'EOF'
import sys
def entry(name, body, mode):
    n = name.encode() + b"\0"
    h = b"070701" + b"".join(b"%08x" % v for v in (1, mode, 0, 0, 1, 0, len(body), 0, 0, 0, 0, len(n), 0))
    out = h + n
    out += b"\0" * (-len(out) % 4)
    out += body
    out += b"\0" * (-len(out) % 4)
    return out
d = open(sys.argv[1], "rb").read()
d += b"\0" * (-len(d) % 4)
d += entry("etc", b"", 0o040755) + entry("etc/bitsos", b"", 0o040755)
d += entry("etc/bitsos/test-panic-before-mcp", b"RB-6 fallback test\n", 0o100644) + entry("TRAILER!!!", b"", 0)
open(sys.argv[2], "wb").write(d)
EOF

# ---- the stick image: generation 1 confirmed, room for two generations ----
touch "$ESP/BITSOS2/SYS/1/OK"
IMG="target/xtask/$NAME.img"
CONTENT=$(du -sb "$ESP" | cut -f1)
KIB=$(( (CONTENT * 3 + (64 << 20)) / 1024 / 1024 * 1024 ))
rm -f "$IMG"
mkfs.fat -F 32 -n BITSOS -C "$IMG" "$KIB" > /dev/null
mcopy -s -i "$IMG" "$ESP/EFI" "$ESP/BITSOS2" ::/
mtype -i "$IMG" ::/EFI/BOOT/BOOTX64.EFI > "$RUN_DIR/loader.before"
echo "image: $IMG ($((KIB / 1024)) MiB; generation 1 marked OK)"

python3 -I "$HERE/install_http.py" "$HTTP" "$B" > "$RUN_DIR/http.log" 2>&1 &
HPID=$!

# ---- 1. cycle ----
LOG="$RUN_DIR/serial-cycle.log"
SOCK="$RUN_DIR/qmp.sock"
rm -f "$LOG" "$SOCK"
# INT-W9: 4 GiB as mcp/70-install (INT-A2a): the dev kernel with its debug info is ~240 MB, and install_kernel
# holds the whole KERNEL in one buffer while the page cache holds the written copy (2 GiB: ENOMEM).
cargo xtask qemu --name "$NAME" --net igb --mem 4096 --hostfwd "$MCP_FWD:8765" --timeout 900 --serial-log "$LOG" \
    -- -action reboot=reset -qmp "unix:$SOCK,server,nowait" > "$RUN_DIR/qemu-cycle.out" 2>&1 &
QPID=$!
for _ in $(seq 100); do [ -S "$SOCK" ] && break; sleep 0.1; done
python3 -I "$HERE/install_scenario.py" cycle "$MCP_FWD" "$TOKEN" "$LOG" "$SOCK" "$BASE" "$B"
stop_qemu
grep -a "generation\.chosen\|kexec install\.\(done\|refused\|replaced\)\|kexec boot\.\(confirm\|confirmed\)" "$LOG" | cut -c1-200
FSCK=$(fsck.fat -n "$IMG" 2>&1) || fail "fsck.fat -n is not clean: $FSCK"
echo "$FSCK" | tail -2
mdir -b -i "$IMG" ::/BITSOS2/SYS > "$RUN_DIR/sys.list"
[ "$(tr -d '\r' < "$RUN_DIR/sys.list" | sed 's#/$##; s#.*/##' | grep -v '^$' | sort | tr '\n' ' ')" = "5 " ] || fail "SYS is not just 5: $(cat "$RUN_DIR/sys.list")"
mtype -i "$IMG" ::/BITSOS2/SYS/5/KERNEL | cmp -s - "$B/good/KERNEL" || fail "SYS\\5\\KERNEL is not the good kernel"
mtype -i "$IMG" ::/BITSOS2/SYS/5/INITRD | cmp -s - "$B/good/INITRD" || fail "SYS\\5\\INITRD is not the good initramfs"
mdir -b -i "$IMG" ::/BITSOS2/SYS/5 | grep -q "/OK$" || fail "SYS\\5\\OK missing"
mdir -b -i "$IMG" ::/BITSOS2/SYS/5 | grep -q "/\(TRY\|NEW\)$" && fail "SYS\\5 still has TRY or NEW"
mtype -i "$IMG" ::/BITSOS2/CONFIG.TXT | grep -q "^# rb6 scenario: written by install_kernel" || fail "CONFIG.TXT lacks the line install_kernel wrote"
mtype -i "$IMG" ::/BITSOS2/CONFIG.TXT | grep -q "^stick_write=off" || fail "CONFIG.TXT lost stick_write=off"
mtype -i "$IMG" ::/EFI/BOOT/BOOTX64.EFI | cmp -s - "$RUN_DIR/loader.before" || fail "\\EFI\\BOOT\\BOOTX64.EFI changed"
echo "ok: on the host: fsck clean, SYS holds only 5 (KERNEL, INITRD = the good bundle, OK), CONFIG.TXT as install_kernel wrote it, the loader unchanged"

# ---- 2. refused: a clone of the stick next to it ----
NAME=rb-install2
cargo xtask image --name "$NAME" --config-extra "token=$TOKEN;mcp_port=8765;loglevel=info" | tail -1
IMG2="target/xtask/$NAME.img"
cp "$IMG2" "$RUN_DIR/clone.img"
S1=$(sum "$IMG2")
S2=$(sum "$RUN_DIR/clone.img")
LOG="$RUN_DIR/serial-refused.log"
rm -f "$LOG"
cargo xtask qemu --name "$NAME" --net virtio --hostfwd "$MCP_FWD:8765" --timeout 300 --serial-log "$LOG" \
    -- -drive "if=none,id=dclone,format=raw,file=$RUN_DIR/clone.img" -device qemu-xhci,id=xf \
    -device usb-storage,bus=xf.0,port=1,drive=dclone > "$RUN_DIR/qemu-refused.out" 2>&1 &
QPID=$!
python3 -I "$HERE/install_scenario.py" refused "$MCP_FWD" "$TOKEN" "$LOG" "$BASE"
stop_qemu
[ "$(sum "$IMG2")" = "$S1" ] || fail "the boot image changed with two sticks"
[ "$(sum "$RUN_DIR/clone.img")" = "$S2" ] || fail "the clone changed"
echo "ok: with two sticks both images are unchanged"

# The one panic is the test's own (generation 3); anything else is a failure.
if grep -a "PANIC\|lock.violation" "$RUN_DIR"/serial-*.log | grep -av "KTEST PANIC panicked at crates/kexec/src/install/confirm.rs"; then
    fail "a failure line in a log"
fi
[ "$(grep -ac "^RB-6 fallback test: the initramfs holds" "$RUN_DIR/serial-cycle.log")" = 1 ] || fail "the test panic did not happen exactly once"
grep -aq "crash previous.line .*E kexec boot.test_panic generation=3" "$RUN_DIR/serial-cycle.log" || fail "the next boot's crash record does not hold the test panic event"
echo "rb-50-install: green"
