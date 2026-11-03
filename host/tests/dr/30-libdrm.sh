#!/bin/bash
# DR-4: real libdrm_amdgpu (2.4.134, static, musl: libdrm/build.sh) against the fake GPU on V2 in QEMU (drm=fake):
# amdgpu_device_initialize, amdgpu_query_gpu_info and every AMDGPU_INFO query Mesa makes at device creation must
# give exactly the answers Linux gave on the real Radeon 780M (fixtures/780m-info.txt, fixtures/780m-gpu-info.txt);
# contexts at NORMAL and LOW work, HIGH is refused (EACCES) for a program without CAP_SYS_NICE... as root it is
# allowed, so only "not an error" is checked for it. The same program runs on CachyOS first (any amdgpu: it must
# run there, its numbers are that GPU's). DR-6: libdrm/bo.c (amdgpu_bo_alloc, cpu_map, metadata, query_info,
# export/import, wait_for_idle, free) prints device-neutral lines that must equal fixtures/libdrm-bo.txt (CachyOS's
# transcript) on both sides. DR-8: libdrm/cs.c (contexts, reset query, fence status, WAIT_FENCES, submit_raw2's
# chunk checks; no job ever reaches a GPU; DR-9: BO lists by handle and FENCE_TO_HANDLE) must equal
# fixtures/libdrm-cs.txt on both sides.
set -euo pipefail
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/dr-30-libdrm}"
mkdir -p "$RUN_DIR"
RUN_DIR="$(realpath "$RUN_DIR")"
LO="${BITSOS_PORT_LO:-16000}"
FWD=$((LO + 30))
TOKEN="dr4-scenario-token-3b9d"
LOG="$RUN_DIR/serial.log"
NAME=dr-libdrm
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-$PWD/target}

set +e
bash host/tests/dr/libdrm/build.sh "$RUN_DIR/libdrm" > "$RUN_DIR/build.log" 2>&1
rc=$?
set -e
if [ $rc = 2 ]; then tail -1 "$RUN_DIR/build.log"; echo "GREEN (skipped: no libdrm source offline)"; exit 0; fi
[ $rc = 0 ] || { tail -20 "$RUN_DIR/build.log"; echo "RED: libdrm did not build"; exit 1; }
PROBE=$RUN_DIR/libdrm/libdrm-info
PROBE_BO=$RUN_DIR/libdrm/libdrm-bo
PROBE_CS=$RUN_DIR/libdrm/libdrm-cs

if [ -e /dev/dri/renderD128 ]; then
    echo "== CachyOS: the static program against the host's amdgpu"
    "$PROBE" > "$RUN_DIR/host.txt" 2>&1 || true
    head -3 "$RUN_DIR/host.txt"
    grep -q "^amdgpu_device_initialize: 0" "$RUN_DIR/host.txt" || { echo "RED: the program fails on CachyOS"; exit 1; }
    "$PROBE_BO" > "$RUN_DIR/host-bo.txt" 2>&1 || true
    cmp -s "$RUN_DIR/host-bo.txt" host/tests/dr/fixtures/libdrm-bo.txt || {
        diff host/tests/dr/fixtures/libdrm-bo.txt "$RUN_DIR/host-bo.txt" | head -20; echo "RED: libdrm-bo on CachyOS differs from its fixture"; exit 1; }
    "$PROBE_CS" > "$RUN_DIR/host-cs.txt" 2>&1 || true
    cmp -s "$RUN_DIR/host-cs.txt" host/tests/dr/fixtures/libdrm-cs.txt || {
        diff host/tests/dr/fixtures/libdrm-cs.txt "$RUN_DIR/host-cs.txt" | head -20; echo "RED: libdrm-cs on CachyOS differs from its fixture"; exit 1; }
fi

cargo xtask image --name "$NAME" --config-extra "token=$TOKEN;mcp_port=8765;loglevel=warn;drm=fake"
ESP=$CARGO_TARGET_DIR/xtask/esp-$NAME
IMG=$CARGO_TARGET_DIR/xtask/$NAME.img
rm -rf "$ESP/BITSOS2/drtest"
mkdir "$ESP/BITSOS2/drtest"
cp "$PROBE" "$ESP/BITSOS2/drtest/libdrm-info"
cp "$PROBE_BO" "$ESP/BITSOS2/drtest/libdrm-bo"
cp "$PROBE_CS" "$ESP/BITSOS2/drtest/libdrm-cs"
bytes=$(du -sb "$ESP" | cut -f1)
kib=$(( (bytes + bytes / 4 + (64 << 20)) / 1048576 * 1024 ))
rm -f "$IMG"
mkfs.fat -F 32 -n BITSOS -C "$IMG" "$kib" > /dev/null
mcopy -s -i "$IMG" "$ESP/EFI" "$ESP/BITSOS2" ::/

rm -f "$LOG"
cargo xtask qemu --name "$NAME" --mem 2G --net virtio --hostfwd "$FWD:8765" --timeout 600 --serial-log "$LOG" \
    -- -smp 2 > "$RUN_DIR/qemu.out" 2>&1 &
QPID=$!
stop() {
    kill "$QPID" 2>/dev/null || true
    pf="$CARGO_TARGET_DIR/xtask/qemu-$NAME.pid"
    if [ -f "$pf" ]; then
        pid="$(head -1 "$pf")"
        if tr '\0' ' ' < "/proc/$pid/cmdline" 2>/dev/null | grep -Eq -- "-name v2-[A-Za-z0-9]+-$NAME( |$)"; then kill "$pid" 2>/dev/null || true; fi
    fi
    wait "$QPID" 2>/dev/null || true
}
trap stop EXIT
python3 -I host/tests/dr/v2_libdrm.py "$FWD" "$TOKEN" none "$RUN_DIR" host/tests/dr/fixtures
if grep -aq "PANIC" "$LOG"; then echo "RED: the kernel panicked"; tail -20 "$LOG"; exit 1; fi
echo "GREEN: static libdrm on V2 gets the 780M's answers from the fake, byte for byte; its buffer calls answer as on CachyOS"
