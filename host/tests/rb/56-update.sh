#!/bin/bash
# slow
# UPDATE: a version from the (fake) workshop repository under QEMU (KVM, igb, the stick on usb-storage, a USB
# keyboard and mouse), on files of this run only. Driver: update_scenario.py (its docstring lists every check).
#   A test key (bitsupdate keygen) signs versions 164 and 165 built by `cargo xtask update-release` (dry run).
#   The stick: slot 164 (OK, version 164's files) beside 103 (the old counting, OK: a kernel and initramfs built
#   from int/gp f335626b, before UPDATE: the code of Mini-PC 163 and the Gaming-PC stick's 107), a test CA, the fake GitHub's
#   address in \BITSOS2\UPDATE\, a test token in \BITSOS2\settings\update.tok. fake_github.py serves the API over HTTPS on one port and the
#   assets on another (a redirect across origins; the storage refuses a token, as S3 does).
#   UPD-4: version 165 brings its parts: a changed bitscomp stand-in in \BITSOS2\bin and a small fake tree
#   `fake` (no real program tree in QEMU); the stick's own \BITSOS2\bin and \BITSOS2\fake hold 164's. Version 166
#   (published once 165 runs) changes only the stand-in: its fake tree is 165's and is not downloaded again.
# Afterwards on the host: fsck.fat -n clean, SYS holds 103 164 165 166 STORE with 165 and 166 OK, PIN names 103,
# 165's and 166's FILES name their parts (the same fake tree), STORE holds both bin parts and the one fake tree
# sealed, 166's fake piece never left the storage port, the stick's own folders are 164's still, the token never
# reached the storage port and every API request carried it.
set -euo pipefail
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/rb-56-update}"
mkdir -p "$RUN_DIR"
RUN_DIR="$(realpath "$RUN_DIR")"
HERE="$(pwd)/host/tests/rb"
# shellcheck source=host/tests/mcp/lib.bash
. "$(pwd)/host/tests/mcp/lib.bash"
LO="${BITSOS_PORT_LO:-23000}"
MCP_FWD=$((LO + 156))
API=$((LO + 157))
STORE=$((LO + 158))
TOKEN="rb56-update-scenario-7f31"
GHTOKEN="test_rb56_0123456789abcdef"
NAME=rb-update
fail() { echo "RED: $*"; exit 1; }

command -v objcopy > /dev/null || fail "no objcopy (the release kernel is stripped with it)"
command -v openssl > /dev/null || fail "no openssl (the run's test CA is made with it)"

QPID=""
HPID=""
cleanup() {
    [ -n "$QPID" ] && stop_run "$QPID" "$NAME"
    if [ -n "$HPID" ] && kill -0 "$HPID" 2>/dev/null; then kill "$HPID" 2>/dev/null || true; wait "$HPID" 2>/dev/null || true; fi
}
trap cleanup EXIT

# ---- the test key and two signed versions (dry runs: nothing leaves the host) ----
(cd user && env -u RUSTUP_TOOLCHAIN -u CARGO_TARGET_DIR cargo build -q --offline -p bitsupdate --target-dir ../target/user-host)
BU="$(pwd)/target/user-host/debug/bitsupdate"
rm -rf "$RUN_DIR/key" "$RUN_DIR/gh"
"$BU" keygen "$RUN_DIR/key"
# UPD-4: each version's stand-in for bitscomp (a script; QEMU starts no desktop) and its fake tree: a few files, one in a
# folder, one with a space in its name.
for v in 164 165 166; do
    printf '#!/bin/sh\necho "bitscomp stand-in of version %s"\nexit 1\n' "$v" > "$RUN_DIR/bitscomp-$v"
    chmod +x "$RUN_DIR/bitscomp-$v"
    rm -rf "$RUN_DIR/fake-$v"; mkdir -p "$RUN_DIR/fake-$v/sub/dir"
    printf 'fake tree of version %s\n' "$v" > "$RUN_DIR/fake-$v/hello.txt"
    printf 'deep %s\n' "$v" > "$RUN_DIR/fake-$v/sub/dir/deep file.txt"
    head -c 70000 /dev/urandom > "$RUN_DIR/fake-$v/sub/blob.bin"
done
# 166 takes 165's fake tree; it waits outside the fake GitHub until 165 runs (the driver moves it in).
rm -rf "$RUN_DIR/later"
for v in 164 165 166; do
    tree=$v; out="$RUN_DIR/gh/rel-$v"
    [ "$v" = 166 ] && { tree=165; out="$RUN_DIR/later/rel-$v"; }
    printf 'Was ist neu in Version %s:\n- Aktualisieren per Knopf (Test).\n' "$v" > "$RUN_DIR/notes-$v.txt"
    cargo xtask update-release --version "$v" --notes "$RUN_DIR/notes-$v.txt" --key-dir "$RUN_DIR/key" \
        --pubkey-file "$RUN_DIR/key/release.pub" --out "$out" \
        --trees none --extra-tree "fake=$RUN_DIR/fake-$tree" --bin-file "bitscomp=$RUN_DIR/bitscomp-$v" | tail -6
done
ls "$RUN_DIR/gh/rel-165" | grep -q '^tree\.fake\.[0-9a-f]\{16\}\.1\.tar\.zst$' || fail "release 165 has no fake tree piece"
[ -f "$RUN_DIR/gh/rel-165/bin.bitscomp" ] || fail "release 165 has no bin.bitscomp"
PIECE165=$(cd "$RUN_DIR/gh/rel-165" && ls tree.fake.*)
PIECE166=$(cd "$RUN_DIR/later/rel-166" && ls tree.fake.*)
[ "$PIECE165" = "$PIECE166" ] || fail "166's unchanged fake tree is named $PIECE166, not $PIECE165"

# ---- the test CA: the guest reaches the host as 10.0.2.2 ----
C="$RUN_DIR/ca"
rm -rf "$C"; mkdir -p "$C"
( cd "$C"
  openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes -keyout ca.key -out ca.pem -days 2 -subj "/CN=BitsOS update test CA" 2>/dev/null
  printf 'subjectAltName=IP:10.0.2.2,IP:127.0.0.1\nbasicConstraints=CA:FALSE\nextendedKeyUsage=serverAuth\n' > ext.cnf
  openssl req -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes -keyout srv.key -out srv.csr -subj "/CN=fake github" 2>/dev/null
  openssl x509 -req -in srv.csr -CA ca.pem -CAkey ca.key -CAcreateserial -out srv.pem -days 2 -extfile ext.cnf 2>/dev/null )

# ---- a kernel from before UPDATE, built once per host from its commit (git archive: no worktree, no checkout) ----
OLD_REV=f335626b
OLD="$(pwd)/target/xtask/rb56-old"
if [ ! -f "$OLD/built-$OLD_REV" ]; then
    rm -rf "$OLD"
    mkdir -p "$OLD/src"
    git archive "$OLD_REV" | tar -x -C "$OLD/src"
    # The archive has no musl sysroot (user/sysroot/out is not in git): the old xtask takes this tree's (bwrap).
    SYSROOT="$(pwd)/user/sysroot/out/sysroot"
    [ -f "$SYSROOT/usr/lib/libc.a" ] || fail "no musl sysroot at $SYSROOT for the old kernel's image (cargo xtask abi-diff builds it once)"
    (cd "$OLD/src" && env -u CARGO_TARGET_DIR -u BITSOS_VERSION -u BITSOS_UPDATE_PUBKEY_FILE V2_SYSROOT="$SYSROOT" V2_LEAN=1 cargo xtask image --name rb56old | tail -1)
    OSYS=$(ls -d "$OLD/src/target/xtask/esp-rb56old/BITSOS2/SYS/"*/ | head -1)
    cp "$OSYS/KERNEL" "$OSYS/INITRD" "$OLD/"
    rm -rf "$OLD/src"
    touch "$OLD/built-$OLD_REV"
fi
grep -aq "BITSOS-VERSION=" "$OLD/KERNEL" && fail "the old kernel carries a version mark: not a kernel from before UPDATE"

# ---- the stick: version 164 as slot 164 (OK), the old kernel as 103 beside it, the UPDATE folder ----
BITSOS_VERSION=164 BITSOS_VERSION_DATE="$(date -u +%F)" BITSOS_UPDATE_PUBKEY_FILE="$RUN_DIR/key/release.pub" V2_LEAN=1 \
    cargo xtask image --name "$NAME" --config-extra "token=$TOKEN;mcp_port=8765;loglevel=info;boot_confirm_s=20;sys_keep=4;stick_write=off" | tail -1
ESP="target/xtask/esp-$NAME"
rm -rf "$ESP/BITSOS2/SYS"
mkdir -p "$ESP/BITSOS2/SYS/164" "$ESP/BITSOS2/SYS/103" "$ESP/BITSOS2/UPDATE"
cp "$RUN_DIR/gh/rel-164/KERNEL" "$RUN_DIR/gh/rel-164/INITRD" "$ESP/BITSOS2/SYS/164/"
cp "$OLD/KERNEL" "$OLD/INITRD" "$ESP/BITSOS2/SYS/103/"
touch "$ESP/BITSOS2/SYS/164/OK" "$ESP/BITSOS2/SYS/103/OK"
cp "$RUN_DIR/gh/rel-164/BOOTX64.EFI" "$ESP/EFI/BOOT/BOOTX64.EFI"
# 164 runs with the stick's own folders (no FILES in its slot): its stand-in and its fake tree there.
cp "$RUN_DIR/bitscomp-164" "$ESP/BITSOS2/bin/bitscomp"
rm -rf "$ESP/BITSOS2/fake"; cp -r "$RUN_DIR/fake-164" "$ESP/BITSOS2/fake"
cp "$C/ca.pem" "$ESP/BITSOS2/UPDATE/ca.pem"
printf 'api=https://10.0.2.2:%s\nrepo=o/werkstatt\nca=/boot/BITSOS2/UPDATE/ca.pem\n' "$API" > "$ESP/BITSOS2/UPDATE/SOURCE.TXT"
mkdir -p "$ESP/BITSOS2/settings"
printf '%s\n' "$GHTOKEN" > "$ESP/BITSOS2/settings/update.tok"
IMG="target/xtask/$NAME.img"
CONTENT=$(du -sb "$ESP" | cut -f1)
KIB=$(( (CONTENT * 2 + (96 << 20)) / 1024 / 1024 * 1024 ))
rm -f "$IMG"
mkfs.fat -F 32 -n BITSOS -C "$IMG" "$KIB" > /dev/null
mcopy -s -i "$IMG" "$ESP/EFI" "$ESP/BITSOS2" ::/
echo "image: $IMG ($((KIB / 1024)) MiB; slots 103 and 164 OK)"

: > "$RUN_DIR/github.log"
python3 -I "$HERE/fake_github.py" "$API" "$STORE" "$C/srv.pem" "$C/srv.key" "$GHTOKEN" "$RUN_DIR/gh" "$RUN_DIR/github.log" > "$RUN_DIR/github.out" 2>&1 &
HPID=$!

LOG="$RUN_DIR/serial.log"
SOCK="$RUN_DIR/qmp.sock"
rm -f "$LOG"
stop_qemu "$NAME"
cargo xtask qemu --name "$NAME" --net igb --mem 4096 --hostfwd "$MCP_FWD:8765" --timeout 2700 --serial-log "$LOG" \
    -- -action reboot=reset -qmp "unix:$SOCK,server,nowait" -device usb-kbd,bus=xhci.0 -device usb-mouse,bus=xhci.0 > "$RUN_DIR/qemu.out" 2>&1 &
QPID=$!
for _ in $(seq 100); do [ -S "$SOCK" ] && break; sleep 0.1; done
python3 -I "$HERE/update_scenario.py" "$MCP_FWD" "$TOKEN" "$LOG" "$SOCK" "$GHTOKEN" "$RUN_DIR/later/rel-166" "$RUN_DIR/gh/rel-166"
stop_run "$QPID" "$NAME"
QPID=""
grep -a "generation\.chosen\|kexec update\.\|kexec install\.done\|kexec boot\.confirm\|kernel start \|kexec parts\.\|fat stick\.bind" "$LOG" | cut -c1-220
FSCK=$(fsck.fat -n "$IMG" 2>&1) || fail "fsck.fat -n is not clean: $FSCK"
SYS=$(mdir -b -i "$IMG" ::/BITSOS2/SYS | tr -d '\r' | sed 's#/$##; s#.*/##' | grep -v '^$' | sort | tr '\n' ' ')
[ "$SYS" = "103 164 165 166 PIN STORE " ] || fail "SYS is not 103 164 165 166 PIN STORE: $SYS"
# UPD-4: 165's and 166's FILES name bin (each its own) and the same fake tree; all sealed in STORE; the stick's own
# folders still hold 164's.
FILES=$(mtype -i "$IMG" ::/BITSOS2/SYS/165/FILES | tr -d '\r')
FILES166=$(mtype -i "$IMG" ::/BITSOS2/SYS/166/FILES | tr -d '\r')
echo "$FILES" | grep -q '^part bin [0-9a-f]\{64\}$' || fail "165's FILES does not name bin: $FILES"
FAKE=$(echo "$FILES" | sed -n 's/^part fake \([0-9a-f]\{16\}\).*/\1/p')
[ -n "$FAKE" ] || fail "165's FILES does not name the fake tree: $FILES"
[ "$(echo "$FILES166" | grep '^part fake ')" = "$(echo "$FILES" | grep '^part fake ')" ] || fail "166 does not name 165's fake tree: $FILES166"
[ "$(echo "$FILES166" | grep '^part bin ')" != "$(echo "$FILES" | grep '^part bin ')" ] || fail "166 names 165's bin part: $FILES166"
STORE=$(mdir -b -i "$IMG" ::/BITSOS2/SYS/STORE | tr -d '\r' | sed 's#/$##; s#.*/##' | grep -v '^$' | sort | tr '\n' ' ')
echo "$STORE" | grep -q "fake\.$FAKE fake\.$FAKE\.PART" || fail "STORE does not hold the fake tree sealed: $STORE"
[ "$(echo "$STORE" | tr ' ' '\n' | grep -c '^bin\.[0-9a-f]\{16\}\.PART$')" = 2 ] || fail "STORE does not hold both bin parts sealed: $STORE"
[ "$(echo "$STORE" | tr ' ' '\n' | grep -c '^fake\.')" = 2 ] || fail "STORE holds more than the one fake tree: $STORE"
echo "$STORE" | grep -q "NEW" && fail "an unfinished part stayed in STORE: $STORE"
# 166's fake piece (same name as 165's) was never fetched: no storage request for its asset id.
P166=$(python3 -I -c 'import os,sys; F=["KERNEL","INITRD","BOOTX64.EFI","WASISTNEU.TXT","MANIFEST","MANIFEST.SIG"]; r=sorted(f for f in os.listdir(sys.argv[1]) if f not in F and not f.startswith(".")); print(166000+len(F)+r.index(sys.argv[2]))' "$RUN_DIR/gh/rel-166" "$PIECE166")
grep -q "^storage /blob/$P166?" "$RUN_DIR/github.log" && fail "166's unchanged fake tree was downloaded again (asset $P166)"
grep -q "^storage /blob/166000?" "$RUN_DIR/github.log" || fail "version 166's KERNEL never came from the storage port"
mtype -i "$IMG" "::/BITSOS2/SYS/STORE/fake.$FAKE/sub/dir/deep file.txt" | tr -d '\r' | grep -qx "deep 165" || fail "the fake tree's deep file is not 165's"
mtype -i "$IMG" "::/BITSOS2/SYS/STORE/fake.$FAKE/sub/blob.bin" | cmp -s - "$RUN_DIR/fake-165/sub/blob.bin" || fail "the fake tree's blob is not 165's"
mtype -i "$IMG" ::/BITSOS2/fake/hello.txt | tr -d '\r' | grep -qx "fake tree of version 164" || fail "the stick's own fake tree changed"
mtype -i "$IMG" ::/BITSOS2/bin/bitscomp | cmp -s - "$RUN_DIR/bitscomp-164" || fail "the stick's own bitscomp changed"
mdir -b -i "$IMG" ::/BITSOS2/SYS/165 | grep -q "/OK$" || fail "SYS\\165\\OK missing"
mdir -b -i "$IMG" ::/BITSOS2/SYS/166 | grep -q "/OK$" || fail "SYS\\166\\OK missing"
[ "$(mtype -i "$IMG" ::/BITSOS2/SYS/PIN | tr -d ' \r\n')" = "103" ] || fail "PIN does not name 103"
mtype -i "$IMG" ::/BITSOS2/SYS/103/KERNEL | cmp -s - "$OLD/KERNEL" || fail "SYS\\103\\KERNEL is not the old kernel"
[ "$(mtype -i "$IMG" ::/BITSOS2/settings/update.tok | tr -d '\r\n')" = "$GHTOKEN" ] || fail "the token is gone from settings\\update.tok"
mtype -i "$IMG" ::/BITSOS2/SYS/165/KERNEL | cmp -s - "$RUN_DIR/gh/rel-165/KERNEL" || fail "SYS\\165\\KERNEL is not version 165's"
grep -q "auth=yes" <(grep "^storage" "$RUN_DIR/github.log") && fail "the token reached the storage port"
grep -q "^api.*auth=no" "$RUN_DIR/github.log" && fail "an API request without the token"
grep -q "^storage /blob/165000?" "$RUN_DIR/github.log" || fail "version 165's KERNEL never came from the storage port"
if grep -a "PANIC\|lock.violation" "$LOG"; then fail "a failure line in the log"; fi
echo "ok: on the host: fsck clean, SYS holds 103 164 165 166 with 165 and 166 OK and PIN 103 (the old kernel), both versions' parts sealed in STORE with one shared fake tree not downloaded twice, the stick's own folders 164's, the token stayed with the API"
echo "rb-56-update: green"
