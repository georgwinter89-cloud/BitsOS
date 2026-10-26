#!/bin/bash
# SN-2: HD Audio in the Reload loop (V2-LANES: not in A1's loop, A3/SN-2 brings it). QEMU with intel-hda +
# hda-duplex (WAV audiodev only), virtio-net for MCP, 16 CPUs: three times a stream is left running
# (sn.hda.leave_running), then Reload over MCP; the old kernel must quiesce hda0 (named in the jump line, no
# bus master left), the new kernel must find no running DMA (no hda.stale_dma), read the codec again and play
# (sn.hda.play over MCP). SN-3: the second reload runs with the mixer playing through hda0
# (sn.mix.leave_running), after it the new kernel's mixer plays (sn.mix.resample).
set -eu
cd "$BITSOS_ROOT"
RUN="$BITSOS_RUN_DIR"
HERE="$BITSOS_ROOT/host/tests/sn"
LO="${BITSOS_PORT_LO:-27500}"
MCP_FWD=$((LO + 100))
HTTP=$((LO + 110))
TOKEN="sn2-reload-token-5c1e"
LOG="$RUN/serial.log"
NAME="$BITSOS_LANE-hda-reload"

if ! grep -q "kexec::set_boot_info" kernel/src/main.rs || ! grep -q "net::init" kernel/src/main.rs; then
    echo "SKIP: kernel/src/main.rs lacks kexec::set_boot_info or net::init"
    exit 0
fi
cargo xtask image --name "$NAME" --config-extra "token=$TOKEN;mcp_port=8765;loglevel=info"
B="$RUN/bundles/base"
mkdir -p "$B"
cp "target/xtask/esp-$NAME/BITSOS2/SYS/1/KERNEL" "target/xtask/esp-$NAME/BITSOS2/SYS/1/INITRD" "$B/"
python3 -I -m http.server --bind 127.0.0.1 --directory "$RUN/bundles" "$HTTP" > "$RUN/http.log" 2>&1 &
HPID=$!
rm -f "$LOG"
cargo xtask qemu --name "$NAME" --smp 16 --net virtio --hostfwd "$MCP_FWD:8765" --timeout 400 --serial-log "$LOG" -- \
    -audiodev "wav,id=a0,path=$RUN/hda.wav,out.frequency=48000" \
    -device intel-hda,id=h0 -device hda-duplex,bus=h0.0,audiodev=a0 > "$RUN/qemu.out" 2>&1 &
QPID=$!
cleanup() {
    local qemu
    qemu=$(sed -n 's/^xtask: qemu pid \([0-9]*\).*/\1/p' "$RUN/qemu.out" 2>/dev/null | head -1)
    for p in "$QPID" "$HPID" $qemu; do
        if kill -0 "$p" 2>/dev/null; then kill "$p" 2>/dev/null || true; wait "$p" 2>/dev/null || true; fi
    done
}
trap cleanup EXIT
python3 -I "$HERE/reload_hda.py" "$MCP_FWD" "$TOKEN" "$LOG" "$HTTP"
