#!/bin/bash
# slow
# MC-4b: host/mcp/install.mjs against a real BitsOS V2 under QEMU (KVM, virtio-net, the stick on usb-storage, an
# image file of this run only; the stick image has generation 1 confirmed and room for more):
#  1. the protocol test of install.mjs against a fake server (install_proto.mjs);
#  2. install.mjs --dry-run: exit 0, "generation 2 would be installed", both hashes equal; SYS unchanged;
#  3. install.mjs --config with nvme_write=on: exit 1 (refused, R8a), SYS and CONFIG.TXT unchanged;
#  4. the echt suite's install checks with --require-install (install.proof_counter: wrong-token requests move
#     mcp.requests, never mcp.answered; install.refusals; install.dry_run with the image's own bundle);
#  5. with kernel_path in install_kernel (MC-4b change request 1 to RB): install.mjs --upload --dry-run (files
#     uploaded with write_file, hash checked, read by install_kernel, deleted again); else a SKIP line;
#  6. install.mjs --reboot: generation 2 installed (NEW, read back), `reboot` names its one try, MCP back from
#     generation 2, reload_info until it confirmed itself (CONFIG boot_confirm_s=20);
#  7. on the host: fsck.fat -n clean, SYS holds 1 and 2 (sys_keep=2), SYS\2 = the bundle with OK and without
#     TRY/NEW, \EFI\BOOT\BOOTX64.EFI unchanged, CONFIG.TXT unchanged; the token never in the serial log.
# Ports: MCP hostfwd LO+140; install.mjs's server on 127.0.0.1 in LO+450..LO+469 (BITSOS_INSTALL_PORTS; the
# guest fetches from 10.0.2.2, QEMU slirp's way to the host).
set -euo pipefail
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/mc-70-install}"
mkdir -p "$RUN_DIR"
RUN_DIR="$(realpath "$RUN_DIR")"
HERE="$(pwd)/host/tests/mcp"
# shellcheck source=host/tests/mcp/lib.bash
. "$HERE/lib.bash"
LO="${BITSOS_PORT_LO:-25000}"
FWD=$((LO + 140))
export BITSOS_INSTALL_PORTS="$((LO + 450))-$((LO + 469))"
export BITSOS_PORT_LO="$LO"
TOKEN="mc4b-install-scenario-31d7"
export BITSOS_TOKEN="$TOKEN"
NAME=mc-install
LOG="$RUN_DIR/serial.log"
SOCK="$RUN_DIR/qmp.sock"
fail() { echo "RED: $*"; exit 1; }

echo "== 1. install.mjs protocol test (fake server)"
node "$HERE/install_proto.mjs" | tail -1

if ! grep -q "kexec::set_boot_info" kernel/src/main.rs || ! grep -q "net::init" kernel/src/main.rs; then
    echo "SKIP: kernel/src/main.rs lacks kexec::set_boot_info or net::init"
    exit 0
fi
if ! grep -q '"install_kernel"' crates/kexec/src/install/tool.rs 2>/dev/null; then
    echo "SKIP: no install_kernel in this tree (RB-6 not merged)"
    exit 0
fi

WPID=""
NPID=""
cleanup() {
    if [ -n "$NPID" ]; then kill "$NPID" 2>/dev/null || true; fi
    if [ -n "$WPID" ]; then stop_run "$WPID" "$NAME"; fi
}
trap cleanup EXIT

# ---- the image: generation 1 confirmed, room for three generations ----
cargo xtask image --name "$NAME" --config-extra "token=$TOKEN;mcp_port=8765;loglevel=info;boot_confirm_s=20;sys_keep=2;nvme_write=off" | tail -1
ESP="target/xtask/esp-$NAME"
B="$RUN_DIR/bundle"
rm -rf "$B"
mkdir -p "$B"
cp "$ESP/BITSOS2/SYS/1/KERNEL" "$ESP/BITSOS2/SYS/1/INITRD" "$B/"
touch "$ESP/BITSOS2/SYS/1/OK"
IMG="target/xtask/$NAME.img"
CONTENT=$(du -sb "$ESP" | cut -f1)
KIB=$(( (CONTENT * 4 + (64 << 20)) / 1024 / 1024 * 1024 ))
rm -f "$IMG"
mkfs.fat -F 32 -n BITSOS -C "$IMG" "$KIB" > /dev/null
mcopy -s -i "$IMG" "$ESP/EFI" "$ESP/BITSOS2" ::/
mtype -i "$IMG" ::/EFI/BOOT/BOOTX64.EFI > "$RUN_DIR/loader.before"
mtype -i "$IMG" ::/BITSOS2/CONFIG.TXT > "$RUN_DIR/config.before"
echo "image: $IMG ($((KIB / 1024)) MiB; generation 1 marked OK)"
# A CONFIG.TXT that turns NVMe writes on (refused) for step 3.
sed 's/^nvme_write=off$/nvme_write=on/' "$RUN_DIR/config.before" > "$RUN_DIR/config.nvme"
grep -q "^nvme_write=on" "$RUN_DIR/config.nvme" || fail "the image's CONFIG.TXT has no nvme_write=off line to flip"

stop_qemu "$NAME"
rm -f "$LOG" "$SOCK"
# INT-A2a: 4 GiB. With every A2 lane merged the debug KERNEL is 160 MB; the upload (tmpfs) plus read_whole's one
# 160 MB heap block did not fit the default 2 GiB any more (ENOMEM). Change request to MC/RB: stage in pages.
cargo xtask qemu --name "$NAME" --net virtio --mem 4096 --hostfwd "$FWD:8765" --timeout 900 --serial-log "$LOG" \
    -- -action reboot=reset -qmp "unix:$SOCK,server,nowait" > "$RUN_DIR/qemu.out" 2>&1 &
WPID=$!
for _ in $(seq 100); do [ -S "$SOCK" ] && break; sleep 0.1; done
[ -S "$SOCK" ] || fail "no QMP socket"
python3 -I "$HERE/ovmf_nudge.py" "$LOG" "$SOCK" > "$RUN_DIR/nudge.log" 2>&1 &
NPID=$!
CALL() { node host/mcp/mcp-call.mjs "127.0.0.1:$FWD" "$@"; }
for i in $(seq 120); do
    CALL version > "$RUN_DIR/version.txt" 2>/dev/null && break
    [ "$i" = 120 ] && fail "MCP does not answer on 127.0.0.1:$FWD within 120 s"
    sleep 1
done
echo "MCP: $(tail -1 "$RUN_DIR/version.txt")"
sys_list() { CALL list_dir '{"path":"/boot/BITSOS2/SYS"}' | tail -n +2 | awk '{print $NF}' | tr '\n' ' '; }
SYS0=$(sys_list)
[ "$SYS0" = "1/ " ] || fail "SYS at the start: '$SYS0', want '1/ '"
INSTALL=(node host/mcp/install.mjs "127.0.0.1:$FWD" --bundle "$B" --bind 127.0.0.1)

echo "== 2. dry run"
"${INSTALL[@]}" --dry-run | tee "$RUN_DIR/dry.out" | grep -v "^| " || fail "install.mjs --dry-run exited $?"
grep -q "install: dry run ok after .*generation 2 would be installed" "$RUN_DIR/dry.out" || fail "dry run line"
[ "$(sys_list)" = "$SYS0" ] || fail "the dry run changed SYS: $(sys_list)"
echo "ok: dry run, SYS unchanged"

echo "== 3. a CONFIG.TXT that turns NVMe writes on is refused"
set +e
"${INSTALL[@]}" --config "$RUN_DIR/config.nvme" > "$RUN_DIR/nvme.out" 2>&1
rc=$?
set -e
[ "$rc" = 1 ] || { cat "$RUN_DIR/nvme.out"; fail "install.mjs with nvme_write=on exited $rc, want 1"; }
grep -q "| Refused: .*nvme_write" "$RUN_DIR/nvme.out" && grep -q "nothing on the stick changed" "$RUN_DIR/nvme.out" || { cat "$RUN_DIR/nvme.out"; fail "no nvme_write refusal"; }
[ "$(sys_list)" = "$SYS0" ] || fail "the refused install changed SYS: $(sys_list)"
CALL read_file '{"path":"/boot/BITSOS2/CONFIG.TXT"}' --save "$RUN_DIR/config.now" > /dev/null
# INT-W9 (WL-4 CR 4): read_file shows the secret values as ***; compare with the stick's file shown that way.
sed -E 's/^([[:space:]]*(token|wlan_psk|claude_api_key)[[:space:]]*=).*$/\1***/' "$RUN_DIR/config.before" > "$RUN_DIR/config.shown"
cmp -s "$RUN_DIR/config.now" "$RUN_DIR/config.shown" || fail "CONFIG.TXT changed"
grep -aq "$TOKEN" "$RUN_DIR/config.now" && fail "read_file showed the token"
echo "ok: refused (exit 1), SYS and CONFIG.TXT unchanged"

echo "== 4. echt suite: install checks"
P_ECHT=$((LO + 470))
node "$HERE/echt.mjs" --host "127.0.0.1:$FWD" --only install --http "10.0.2.2:$P_ECHT" --http-bind 127.0.0.1 --install-bundle "$B" --require-install | tee "$RUN_DIR/echt.out"
grep -q "^echt .*: 3 of 3 ok" "$RUN_DIR/echt.out" || fail "echt install checks"
[ "$(sys_list)" = "$SYS0" ] || fail "the echt dry run changed SYS: $(sys_list)"

echo "== 5. --upload --dry-run"
if grep -q '"kernel_path"' crates/kexec/src/install/tool.rs; then
    "${INSTALL[@]}" --upload --dry-run | tee "$RUN_DIR/upload.out" | grep -v "^| " || fail "install.mjs --upload --dry-run"
    grep -q "install: dry run ok .*read from the uploads" "$RUN_DIR/upload.out" || fail "upload dry run line"
    CALL list_dir '{"path":"/tmp"}' | grep -q "install-" && fail "uploads left in /tmp"
    [ "$(sys_list)" = "$SYS0" ] || fail "the upload dry run changed SYS: $(sys_list)"
    echo "ok: upload dry run, uploads removed, SYS unchanged"
else
    echo "SKIP: install_kernel has no kernel_path (MC-4b change request 1 to RB not applied)"
fi

echo "== 6. install, reboot, confirmation"
"${INSTALL[@]}" --reboot --back-s 150 --confirm-s 150 | tee "$RUN_DIR/install.out" || fail "install.mjs --reboot exited non-zero"
grep -q "install: generation 2 installed (NEW)" "$RUN_DIR/install.out" || fail "no install line"
grep -q "| next start: KERNEL: generation 2 (NEWEST_TRY" "$RUN_DIR/install.out" || fail "reboot did not name generation 2's one try"
grep -q "install: generation 2 runs and confirmed itself (OK)" "$RUN_DIR/install.out" || fail "no confirmation"
grep -aq "mcp reboot.synced" "$LOG" || fail "reboot did not write back before the reset"
grep -aq "generation.chosen number=2 reason=NEWEST_TRY" "$LOG" || fail "bitsboot did not start 2 as its one try"
grep -aq "kexec boot.confirmed generation=2" "$LOG" || fail "no kexec boot.confirmed"
L=$(CALL list_dir '{"path":"/boot/BITSOS2/SYS/2"}')
echo "$L" | grep -q " OK$" && ! echo "$L" | grep -q " TRY$\| NEW$" || fail "SYS/2 markers: $L"
cat "$RUN_DIR/nudge.log"

echo "== 7. on the host"
stop_run "$WPID" "$NAME"
WPID=""
FSCK=$(fsck.fat -n "$IMG" 2>&1) || fail "fsck.fat -n is not clean: $FSCK"
[ "$(mdir -b -i "$IMG" ::/BITSOS2/SYS | tr -d '\r' | sed 's#/$##; s#.*/##' | grep -v '^$' | sort | tr '\n' ' ')" = "1 2 " ] || fail "SYS is not 1 and 2: $(mdir -b -i "$IMG" ::/BITSOS2/SYS)"
mtype -i "$IMG" ::/BITSOS2/SYS/2/KERNEL | cmp -s - "$B/KERNEL" || fail "SYS\\2\\KERNEL is not the bundle"
mtype -i "$IMG" ::/BITSOS2/SYS/2/INITRD | cmp -s - "$B/INITRD" || fail "SYS\\2\\INITRD is not the bundle"
mdir -b -i "$IMG" ::/BITSOS2/SYS/2 | grep -q "/OK$" || fail "SYS\\2\\OK missing"
mdir -b -i "$IMG" ::/BITSOS2/SYS/2 | grep -q "/\(TRY\|NEW\)$" && fail "SYS\\2 still has TRY or NEW"
mtype -i "$IMG" ::/EFI/BOOT/BOOTX64.EFI | cmp -s - "$RUN_DIR/loader.before" || fail "\\EFI\\BOOT\\BOOTX64.EFI changed"
mtype -i "$IMG" ::/BITSOS2/CONFIG.TXT | cmp -s - "$RUN_DIR/config.before" || fail "CONFIG.TXT changed"
grep -aq "$TOKEN" "$LOG" && fail "the token is in the serial log"
if grep -a "PANIC\|lock.violation" "$LOG"; then fail "a failure line in the serial log"; fi
echo "ok: fsck clean, SYS = 1 and 2 (2 = the bundle, OK), loader and CONFIG.TXT unchanged, no token in the log"
echo "mc-70-install: green"
