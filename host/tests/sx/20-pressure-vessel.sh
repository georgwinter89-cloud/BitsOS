#!/usr/bin/env bash
# slow
# SX-3c: pressure-vessel (Valve's container tool) with the Steam Linux Runtime 3.0 "sniper" tree starts `true` and
# `steam-runtime-system-info` on V2 as uid 1000, with software graphics from the GM-1a Mesa tree as the graphics
# provider (user/ports/steamrt/build.sh: out/SteamLinuxRuntime_sniper and out/gfx). The tree (~1.1 GB) never goes
# into the image: V2 pulls it over HTTP into /tmp. Checked: both runs end with 0, `true` says nothing on stderr
# that pressure-vessel calls an error (" E: "), system-info's report is JSON whose container section says
# pressure-vessel with V2 as the host, whose x86_64 architecture can run with its libraries, and which finds
# lavapipe (Vulkan) and Mesa's EGL vendor from the provider, and whose sections equal the CachyOS reference run
# (user/ports/steamrt/record-host.sh, made here) apart from the known ones (sx_pv_check.py; each red line of the
# report is explained in plan/notes/sx-SX-3c.md).
# Without the tree (V2_OFFLINE=1 build, no download here) it prints "SKIPPED, NOTHING CHECKED" and passes;
# SX_REQUIRE_PV=1 makes that a failure. Without bwrap on the host there is no reference and the report is checked on
# its own (the last line says "compared with CachyOS: no"); SX_REQUIRE_HOST_REF=1 makes that a failure too.
# SX3C_KEEP=1 leaves QEMU running, SX3C_REUSE=1 uses the running one.
set -euo pipefail
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/sx-20-pressure-vessel}"
mkdir -p "$RUN_DIR"
RUN_DIR="$(realpath "$RUN_DIR")"
HERE="$(pwd)/host/tests/sx"
PORT_DIR="$(pwd)/user/ports/steamrt"
# shellcheck source=host/tests/mcp/lib.bash
. "$(pwd)/host/tests/mcp/lib.bash"
LO="${BITSOS_PORT_LO:-32500}"
FWD=$((LO + 30))
HTTP=$((LO + 31))
TOKEN="sx3c-pressure-vessel-7a2d"
NAME=sx-pv
LOG="$RUN_DIR/serial.log"
export BITSOS_LANE="${BITSOS_LANE:-sx}" BITSOS_PORT_LO="$LO" BITSOS_PORT_HI="${BITSOS_PORT_HI:-32767}"

if ! V2_OFFLINE=1 sh "$PORT_DIR/build.sh" > "$RUN_DIR/build.out" 2>&1; then
    echo "SKIPPED, NOTHING CHECKED: the sniper tree is not there ($(tail -1 "$RUN_DIR/build.out")); sh user/ports/steamrt/build.sh downloads it once (~190 MB)"
    [ -z "${SX_REQUIRE_PV:-}" ] || exit 1
    exit 0
fi

if [ -z "${SX3C_REUSE:-}" ]; then
    cargo xtask image --name "$NAME" --config-extra "token=$TOKEN;mcp_port=8765;loglevel=info" > "$RUN_DIR/image.out" 2>&1 || {
        tail -20 "$RUN_DIR/image.out"; exit 1; }
    stop_qemu "$NAME"
    rm -f "$LOG"
    # 6 GB: the tree lies in /tmp (tmpfs, RAM); --copy-runtime only adds hard links.
    cargo xtask qemu --name "$NAME" --net virtio --smp 4 --mem 6144 --hostfwd "$FWD:8765" --timeout 3000 --serial-log "$LOG" > "$RUN_DIR/qemu.out" 2>&1 &
    QPID=$!
    if [ -z "${SX3C_KEEP:-}" ]; then
        trap 'stop_run "$QPID" "$NAME"' EXIT
    fi
fi

# The CachyOS reference (same tree, environment and provider; a few seconds), when this host can run it.
rm -rf "$RUN_DIR/v2" "$RUN_DIR/host"
HOST_REF=""
if command -v bwrap > /dev/null && sh "$PORT_DIR/record-host.sh" "$RUN_DIR/host" > "$RUN_DIR/host.out" 2>&1; then
    HOST_REF="$RUN_DIR/host"
else
    echo "note: no CachyOS reference run ($(tail -1 "$RUN_DIR/host.out" 2>/dev/null)); the report is checked on its own"
    if [ -n "${SX_REQUIRE_HOST_REF:-}" ]; then
        echo "pressure-vessel: RED (SX_REQUIRE_HOST_REF=1 and no CachyOS reference)"
        exit 1
    fi
fi
python3 "$HERE/v2_pv.py" "$FWD" "$TOKEN" "$LOG" "$HTTP" "$PORT_DIR/out" "$RUN_DIR/v2"
python3 "$HERE/sx_pv_check.py" "$RUN_DIR/v2" $HOST_REF
