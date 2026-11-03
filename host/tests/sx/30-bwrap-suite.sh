#!/usr/bin/env bash
# slow
# SX-3b: the real bubblewrap (user/ports/bubblewrap, static musl) runs its own test suite on V2 as uid 1000 and
# answers what it answers on CachyOS: test-run.sh, test-specifying-pidns.sh, test-specifying-userns.sh and the
# test-utils program, each TAP line against expected/<suite>.tap (CachyOS 7.3 with the same stage, user/ports/
# bubblewrap/tests/record-host.sh). Lines V2 cannot answer the same way are listed with their reason in
# expected/known.txt; a known line that passes after all is reported so it can go.
# Without the tarballs in the download cache (V2_OFFLINE=1, no download here) it prints "SKIPPED, NOTHING CHECKED"
# and passes; SX_REQUIRE_BWRAP=1 makes that a failure.
# SX3B_KEEP=1 leaves QEMU running for the next try (development).
set -euo pipefail
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/sx-30-bwrap-suite}"
mkdir -p "$RUN_DIR"
RUN_DIR="$(realpath "$RUN_DIR")"
HERE="$(pwd)/host/tests/sx"
PORT_DIR="$(pwd)/user/ports/bubblewrap"
# shellcheck source=host/tests/mcp/lib.bash
. "$(pwd)/host/tests/mcp/lib.bash"
LO="${BITSOS_PORT_LO:-32500}"
FWD=$((LO + 20))
HTTP=$((LO + 21))
TOKEN="sx3b-bwrap-suite-5e1c"
NAME=sx-bwrap
LOG="$RUN_DIR/serial.log"
export BITSOS_LANE="${BITSOS_LANE:-sx}" BITSOS_PORT_LO="$LO" BITSOS_PORT_HI="${BITSOS_PORT_HI:-32767}"

if ! V2_OFFLINE=1 sh "$PORT_DIR/build.sh" > "$RUN_DIR/build.out" 2>&1; then
    echo "SKIPPED, NOTHING CHECKED: bubblewrap port not built ($(tail -1 "$RUN_DIR/build.out")); sh user/ports/bubblewrap/build.sh downloads once"
    # SX_REQUIRE_BWRAP=1 (a machine that must run the suite): a missing port is a failure, not a skip.
    [ -z "${SX_REQUIRE_BWRAP:-}" ] || exit 1
    exit 0
fi
tar -C "$PORT_DIR/out" -cf "$RUN_DIR/bw.tar" bwrap bash getpcaps findmnt df bwrap-lockf bwrap-pdeath-probe tests

if [ -z "${SX3B_REUSE:-}" ]; then
    cargo xtask image --name "$NAME" --config-extra "token=$TOKEN;mcp_port=8765;loglevel=info" > "$RUN_DIR/image.out" 2>&1 || {
        tail -20 "$RUN_DIR/image.out"; exit 1; }
    stop_qemu "$NAME"
    rm -f "$LOG"
    cargo xtask qemu --name "$NAME" --net virtio --smp 4 --mem 2048 --hostfwd "$FWD:8765" --timeout 1500 --serial-log "$LOG" > "$RUN_DIR/qemu.out" 2>&1 &
    QPID=$!
    if [ -z "${SX3B_KEEP:-}" ]; then
        trap 'stop_run "$QPID" "$NAME"' EXIT
    fi
fi

rm -rf "$RUN_DIR/v2"
python3 "$HERE/v2_bwrap.py" "$FWD" "$TOKEN" "$LOG" "$HTTP" "$RUN_DIR/bw.tar" "$RUN_DIR/v2" ${SX3B_SUITES:-}
python3 "$HERE/tap_compare.py" "$HERE/expected" "$RUN_DIR/v2" ${SX3B_SUITES:-}
