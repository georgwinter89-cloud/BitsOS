#!/bin/bash
# minipc: host/tests/minipc/vk.sh (this script boots the deqp tree, so no regular run starts it; start it by hand after a change to vk.sh)
# GM-1d: proves host/tests/minipc/vk.sh, the Mini-PC's milestone 1 run, where Mesa may run: V2 in QEMU (-smp 4, 4 GiB,
# 1280x800) with drm=fake and the trees from the stick manifest (V2_STICK_TREES=mesa,deqp, the same path a real stick's
# image takes). vulkaninfo runs on RADV against the fake 780M; the window (a plain bitscomp started here, vkcube on
# wl_shm) and the deqp subset on lavapipe, because the fake draws nothing: nothing of this judges the real chip. Then
# the same script must fail where it should: vulkaninfo on lavapipe (the wrong device), a damaged reference (a heap, a
# queue family, an extension, a deqp result), no compositor, the real profile's steps here (its ktests are class Program;
# RADV's window in a Pixman bitscomp appears but does not present, the fake draws nothing), a key store that does not exist. The token must appear in no output. Parsing:
# tools/test_vkdiff.py in 30-drmtrace-tools.sh.
# Needs: bash user/ports/mesa/scripts/stage-stick.sh (the stage folders the manifest's tree lines read; without them this
# is RED, never left out), node.
set -euo pipefail
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
ROOT=$PWD
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/gm-40-vk-proof}"
mkdir -p "$RUN_DIR"
RUN_DIR="$(realpath "$RUN_DIR")"
LO="${BITSOS_PORT_LO:-32000}"
FWD=$((LO + 80))
TOKEN="gm1d-scenario-token-52e1"
LOG="$RUN_DIR/serial.log"
NAME=gm-vk
SOCK=wayland-3
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-$ROOT/target}
STAGE=${V2_STAGE:-/var/tmp/claude-v2/stage}
[ -f "$STAGE/mesa/run" ] && [ -f "$STAGE/deqp/deqp-vk" ] \
  || { echo "RED: no $STAGE/mesa or $STAGE/deqp: bash user/ports/mesa/scripts/stage-stick.sh"; exit 1; }
cmp -s "$STAGE/mesa/run" user/ports/mesa/bitsos/run || { echo "RED: $STAGE/mesa/run is older than user/ports/mesa/bitsos/run: stage again"; exit 1; }
cmp -s "$STAGE/deqp/subset.txt" host/tests/gm/ref-780m/deqp-subset.txt || { echo "RED: $STAGE/deqp/subset.txt is not ref-780m/deqp-subset.txt: stage again"; exit 1; }
printf '%s\n' "$TOKEN" > "$RUN_DIR/token"

V2_STICK_TREES=mesa,deqp cargo xtask image --name "$NAME" --config-extra "token=$TOKEN;mcp_port=8765;loglevel=warn;screen=1280x800;drm=fake" \
  > "$RUN_DIR/image.txt" 2>&1 || { tail -20 "$RUN_DIR/image.txt"; exit 1; }
ESP=$CARGO_TARGET_DIR/xtask/esp-$NAME
[ -f "$ESP/BITSOS2/mesa/run" ] && [ -f "$ESP/BITSOS2/deqp/deqp-vk" ] && [ -f "$ESP/BITSOS2/bin/mesa-run" ] \
  || { echo "RED: V2_STICK_TREES=mesa,deqp did not put the trees and mesa-run on the image"; exit 1; }
[ ! -e "$ESP/BITSOS2/brave" ] || { echo "RED: a tree nobody named is on the image"; exit 1; }

rm -f "$LOG"
cargo xtask qemu --name "$NAME" --mem 4G --net virtio --hostfwd "$FWD:8765" --timeout 1800 --serial-log "$LOG" -- -smp 4 > "$RUN_DIR/qemu.out" 2>&1 &
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

M=127.0.0.1:$FWD
mc() { node host/mcp/mcp-call.mjs "$M" "$1" "$2" --token-file "$RUN_DIR/token"; }
ENVJ='{"PATH":"/bin:/usr/bin:/boot/BITSOS2/bin","HOME":"/","XDG_RUNTIME_DIR":"/tmp/wy"}'
up=
for _ in $(seq 150); do
  if mc run '{"path":"/bin/sh","args":["-c","mkdir -p /tmp/wy"],"timeout_ms":20000}' > /dev/null 2>&1; then up=1; break; fi
  sleep 2
done
[ -n "$up" ] || { echo "RED: MCP did not answer"; tail -20 "$LOG"; exit 1; }
mc run "{\"path\":\"/bin/bitscomp\",\"args\":[\"--socket\",\"$SOCK\",\"--stats\",\"5\"],\"env\":$ENVJ,\"timeout_ms\":1500}" > "$RUN_DIR/bitscomp-start.txt" 2>&1 || true
up=
for _ in $(seq 30); do
  if mc run "{\"path\":\"/bin/bitscomp\",\"args\":[\"ctl\",\"--socket\",\"$SOCK\",\"stats\"],\"env\":$ENVJ,\"timeout_ms\":20000}" 2>/dev/null | grep -q commits=; then up=1; break; fi
  sleep 1
done
[ -n "$up" ] || { echo "RED: bitscomp did not start"; cat "$RUN_DIR/bitscomp-start.txt"; exit 1; }

VK=(bash host/tests/minipc/vk.sh "$M" --plain --token-file "$RUN_DIR/token" --profile qemu --socket "$SOCK" --xdg /tmp/wy)

echo "== the whole run: every step must pass"
"${VK[@]}" --out "$RUN_DIR/green" 2>&1 | tee "$RUN_DIR/green.txt"
[ "${PIPESTATUS[0]}" = 0 ] || { echo "RED: vk.sh failed on the proof's node"; exit 1; }

# Expects exit 1 and the given FAIL lines.
must_fail() {
  local tag=$1 rc=0
  shift
  local want=$1
  shift
  "${VK[@]}" --out "$RUN_DIR/$tag" "$@" > "$RUN_DIR/$tag.txt" 2>&1 || rc=$?
  [ "$rc" = 1 ] || { echo "RED: $tag: exit $rc, want 1"; cat "$RUN_DIR/$tag.txt"; exit 1; }
  local w
  for w in $want; do
    grep -q "^FAIL $w" "$RUN_DIR/$tag.txt" || { echo "RED: $tag: no 'FAIL $w'"; cat "$RUN_DIR/$tag.txt"; exit 1; }
  done
  echo "ok: $tag: exit 1, FAIL on: $want"
}

echo "== where it must fail"
must_fail wrong-driver "summary" --steps info --info-driver lvp_icd
R=$RUN_DIR/ref-damaged
rm -rf "$R"
cp -r host/tests/gm/ref-780m "$R"
sed -i -E 's/^(heap 1 size=)[0-9]+/\11000/; s/^(queue QUEUE_COMPUTE_BIT,QUEUE_SPARSE_BINDING_BIT,QUEUE_TRANSFER_BIT count=)[0-9]+/\17/; /^ext VK_KHR_swapchain /d' "$R/vulkan.ref"
sed -i -E 's/^(dEQP-VK.api.smoke.triangle) .*/\1 Fail/' "$R/deqp-lavapipe.ref"
must_fail damaged-ref "memory-heaps queue-families extensions deqp-api.smoke" --steps info,deqp --ref-dir "$R"
must_fail no-compositor "window" --steps window --socket no-such-socket
must_fail real-ktest "ktest-ul.pkg.mesa ktest-ul.pkg.deqp" --steps ktest --profile real
must_fail real-window "presenting close" --steps window --profile real
rc=0
bash host/tests/minipc/vk.sh 127.0.0.1:1 --tls "$RUN_DIR/no-such-store" --token-file "$RUN_DIR/token" --out "$RUN_DIR/no-tls" > "$RUN_DIR/no-tls.txt" 2>&1 || rc=$?
[ "$rc" = 1 ] && grep -q '^FAIL mcp' "$RUN_DIR/no-tls.txt" || { echo "RED: a Mini-PC that cannot be reached: exit $rc"; cat "$RUN_DIR/no-tls.txt"; exit 1; }
echo "ok: no key store: exit 1, FAIL mcp"
if grep -lqs "$TOKEN" "$RUN_DIR"/green.txt "$RUN_DIR"/wrong-driver.txt "$RUN_DIR"/damaged-ref.txt "$RUN_DIR"/no-compositor.txt "$RUN_DIR"/real-*.txt "$RUN_DIR"/no-tls.txt "$RUN_DIR"/green/*.txt; then
  echo "RED: the token is in an output"; exit 1
fi
echo "ok: the token is in no output"

mc run "{\"path\":\"/bin/bitscomp\",\"args\":[\"ctl\",\"--socket\",\"$SOCK\",\"quit\"],\"env\":$ENVJ,\"timeout_ms\":20000}" > /dev/null 2>&1 || true
if grep -aq "PANIC" "$LOG"; then echo "RED: the kernel panicked"; tail -20 "$LOG"; exit 1; fi
echo "GREEN: vk.sh passes on the fake node and lavapipe and fails where it should"
