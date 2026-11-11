# NV-2p: what host/tests/nv/40-window.sh and 42-window-setup.sh share. Sourced, with RUN_DIR and NV_RUN set.
LO="${BITSOS_PORT_LO:-19000}"
MCP_FWD=$((LO + 200))
HTTP=$((LO + 210))
TOKEN="nv2p-window-token-77d1"
SERIAL=LEXARQEMUZY1234
NAME=nv-win
SMP="${NV_SMP:-4}"
BOOT_LINE="BdsDxe: loading"
MKIMG() { python3 -I "$NV/mkimg.py" "$@"; }
sum() { sha256sum "$1" | cut -d' ' -f1; }
fail() { echo "RED: $*"; exit 1; }
GUID3=$(MKIMG guid 3)

QPID=""
stop_own() {
    if [ -n "$QPID" ] && kill -0 "$QPID" 2>/dev/null; then
        local q; q=$(cat "target/xtask/qemu-$NAME.pid" 2>/dev/null || true)
        if [ -n "$q" ]; then kill "$q" 2>/dev/null || true; fi
        kill "$QPID" 2>/dev/null || true
        wait "$QPID" 2>/dev/null || true
    fi
    QPID=""
}
HPID=""
cleanup() {
    stop_own
    if [ -n "$HPID" ]; then kill "$HPID" 2>/dev/null || true; fi
}
trap cleanup EXIT

# start IMAGE LOG [extra QEMU args]: the boot image $NAME with the NVMe image, in the background.
start() {
    local img="$1" log="$2"; shift 2
    rm -f "$log"
    # NV_NO_DISK: a machine with no NVMe controller at all (the keys still set).
    local disk=(-drive "if=none,id=nv0,format=raw,file=$img,discard=unmap" -device "nvme,id=nvq0,serial=$SERIAL,drive=nv0")
    [ -z "${NV_NO_DISK:-}" ] || disk=()
    cargo xtask qemu --name "$NAME" --smp "$SMP" --mem "${NV_MEM:-3G}" --net igb --hostfwd "$MCP_FWD:8765" --timeout "${NV_TIMEOUT:-420}" --serial-log "$log" -- \
        "${disk[@]}" "$@" > "$RUN_DIR/qemu.out" 2>&1 &
    QPID=$!
}

# Room for data.img (64 MiB) and the 512 MiB V2 keeps free on the stick (as host/tests/fs/50-data.sh).
grow_stick() {
    local stick="target/xtask/$NAME.img" kib
    kib=$(( $(stat -c %s "$stick") / 1024 + 700 * 1024 ))
    rm -f "$stick"
    mkfs.fat -F 32 -n BITSOS -C "$stick" "$kib" > /dev/null
    mcopy -s -i "$stick" "target/xtask/esp-$NAME/EFI" "target/xtask/esp-$NAME/BITSOS2" ::/
}

# ktests LOGNAME IMG CONFIG TESTS...: one boot with these ktests (nv.window.<each>); green or the script ends.
ktests() {
    local name="$1" img="$2" cfg="$3"; shift 3
    local log="$RUN_DIR/$name.log" list="" t
    for t in "$@"; do list="$list,nv.window.$t"; done
    [ -n "${NV_EXTRA_TESTS:-}" ] && list="$list,$NV_EXTRA_TESTS"
    cargo xtask image --name "$NAME" --ktest --config-extra "ktest=${list#,};ktest_run=$NV_RUN${NV_EXTRA_RUN:-};loglevel=info;data_img_mb=64;$cfg" > "$RUN_DIR/image.out" 2>&1 || { tail -20 "$RUN_DIR/image.out"; fail "image"; }
    grow_stick
    start "$img" "$log"
    wait "$QPID" || true
    QPID=""
    grep -aq "KTEST done passed=.* failed=0" "$log" || { grep -a "KTEST\|window\|nvme setup\|PANIC" "$log" | tail -30; fail "$name: ktests not green"; }
    for t in "$@"; do grep -aq "KTEST ok nv.window.$t " "$log" || fail "$name: nv.window.$t did not pass"; done
    if grep -aq "PANIC\|lock.violation" "$log"; then fail "$name: a failure line in the log"; fi
    echo "ok: $name"
}

