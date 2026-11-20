#!/bin/bash
# GM-6m: the music player bitsmusik in QEMU, one normal boot without an audio device (the kernel's mixer plays
# into its null output: nothing audible): a FLAC made by the checker goes to the stick's \BITSOS2\MUSIK\ with a
# list, `bitsmusik serve` runs as user 1000 on /run/user/1000/bitsmusik.sock, play of the list, `status` twice
# with the position moving, stop, the player's events on its stderr (musik_check.py). No panic in the serial log.
set -eu
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/ul-70-musik}"
mkdir -p "$RUN_DIR"
RUN_DIR="$(realpath "$RUN_DIR")"
HERE="$(pwd)/host/tests/ul"
LO="${BITSOS_PORT_LO:-28500}"
FWD=$((LO + 90))
TOKEN="gm6m-musik-token-3b91"
LOG="$RUN_DIR/serial.log"
NAME="${BITSOS_LANE:-UL}-musik"
fail() { echo "RED: $*"; exit 1; }

QPID=""
cleanup() {
    if [ -n "$QPID" ] && kill -0 "$QPID" 2>/dev/null; then kill "$QPID" 2>/dev/null || true; fi
    local vm
    vm=$(grep -aoE 'qemu pid [0-9]+' "$RUN_DIR"/qemu.out 2>/dev/null | tail -1 | awk '{print $3}' || true)
    if [ -n "$vm" ] && kill -0 "$vm" 2>/dev/null; then kill "$vm" 2>/dev/null || true; fi
    if [ -n "$QPID" ]; then wait "$QPID" 2>/dev/null || true; fi
}
trap cleanup EXIT

cargo xtask image --name "$NAME" --config-extra "token=$TOKEN;mcp_port=8765;loglevel=info" > "$RUN_DIR/image.txt" 2>&1 \
    || fail "the image did not build (see $RUN_DIR/image.txt)"
rm -f "$LOG"
cargo xtask qemu --name "$NAME" --net virtio --hostfwd "$FWD:8765" --timeout 300 --serial-log "$LOG" > "$RUN_DIR/qemu.out" 2>&1 &
QPID=$!
python3 -I "$HERE/musik_check.py" "$FWD" "$TOKEN" || fail "the check failed (see $RUN_DIR)"
cleanup
QPID=""

grep -aq "PANIC" "$LOG" && fail "a panic"
grep -aq "$TOKEN" "$LOG" && fail "the token appears in the log (R8i)"
echo "ul musik: green (a FLAC from the stick's MUSIK folder plays to the null output, status position moves)"
