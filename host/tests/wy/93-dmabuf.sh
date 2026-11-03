#!/usr/bin/env bash
# WY-16 on the host (headless back end, nothing on the screen): client dma-bufs and explicit sync, with Pixman (the
# rescue build) and GLES (the gpu build on the Mesa stage's llvmpipe, as 98). wyclient --dmabuf makes its buffers
# from /dev/udmabuf (memfd pages) and hands them over with zwp_linux_dmabuf_v1 version 4. Proves, for both
# renderers: the default feedback names the render node (main device 226:128 here) and a table that starts with
# XRGB8888 LINEAR; a dma-buf window shows the very picture of a wl_shm window (Pixman: byte for byte; GLES: within
# img.py's tolerance); a non-linear modifier is answered `failed` and bitscomp stays up; with explicit sync
# (wp_linux_drm_syncobj_v1 on the render node) an animating client gets every buffer back through its release
# points (sync_released grows, nothing starves) and a commit whose acquire point signals 1.5 s late shows its first
# frame only after that; the protocol errors no_release_point, unsupported_buffer and conflicting_points end only
# the client; buffers with rows shorter than their width or no dma-buf at all are `failed`, rows past the end
# out_of_bounds; 500 commits with acquire points that never signal cost bitscomp no fd each and go with their
# client; a waiting commit whose buffer was destroyed applies harmlessly. Then the stage's Mesa clients (lavapipe vkcube --wsi wayland, llvmpipe eglgears_wayland) run against
# bitscomp with the dma-buf global present. Needs /dev/udmabuf (SKIP without it); the GLES half and the Mesa
# clients need the Mesa stage (they SKIP alone without it); explicit sync needs a render node with syncobj
# timelines (else that part says SKIP). Only syncobj objects are made on the render node: no GPU work.
set -euo pipefail

ROOT=${BITSOS_ROOT:-$(cd "$(dirname "$0")/../../.." && pwd)}
cd "$ROOT"
RUN=${BITSOS_RUN_DIR:-$(mktemp -d)}
mkdir -p "$RUN"
RUN=$(realpath "$RUN")
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-$ROOT/target}
BIN=$CARGO_TARGET_DIR/release/bitscomp
WYC=$CARGO_TARGET_DIR/release/wyclient
IMG="python3 -I host/tests/wy/tools/img.py"
export XDG_RUNTIME_DIR=$RUN/xdg
mkdir -m 700 -p "$XDG_RUNTIME_DIR"
unset DISPLAY WAYLAND_DISPLAY

fail() { echo "FAIL: $*"; exit 1; }

[ -r /dev/udmabuf ] && [ -w /dev/udmabuf ] || { echo "SKIP: not run: /dev/udmabuf is not usable here"; exit 0; }

echo "== build the rescue build and wyclient"
(cd user/services/bitscomp && cargo build --release -q --offline && cargo build --release -q --offline -p wyclient)

STAGE=${MESA_STAGE:-$ROOT/user/ports/mesa/out/mesa}
[ -f "$STAGE/lib/ld-linux-x86-64.so.2" ] || STAGE=$CARGO_TARGET_DIR/wy-mesa-out/mesa
GPU=
if [ -f "$STAGE/lib/ld-linux-x86-64.so.2" ]; then
    echo "== the gpu build for a tree at $RUN/tree (Mesa stage $STAGE)"
    GPU=$RUN/bitscomp-gpu
    MESA_STAGE=$STAGE bash host/tests/wy/tools/gpu-tree.sh "$RUN/tree" "$GPU" "$RUN/tree" >"$RUN/tree.out" 2>&1 \
        || fail "gpu-tree.sh: $(tail -5 "$RUN/tree.out")"
else
    echo "SKIP (GLES half and Mesa clients): no Mesa stage"
fi

PIDS=()
cleanup() {
    for p in "${PIDS[@]}"; do kill "$p" 2>/dev/null || true; done
    for p in "${PIDS[@]}"; do wait "$p" 2>/dev/null || true; done
}
trap cleanup EXIT

started() { # log
    for _ in $(seq 300); do grep -q "^WAYLAND_DISPLAY=" "$1" && return 0; sleep 0.1; done
    fail "no start line in $1: $(tail -5 "$1")"
}
ctl() { "$BIN" ctl --socket "$SOCK" "$@"; }
alive() { kill -0 "$CPID" 2>/dev/null || fail "$1: bitscomp ended: $(tail -5 "$LOG")"; }
# compositor NAME PROGRAM ARGS..: one headless bitscomp, its socket in SOCK, pid in CPID, log in LOG.
compositor() {
    local name=$1
    shift
    SOCK=wy93-$name-$$
    LOG=$RUN/$name.log
    "$@" --backend headless --size 1280x800 --socket "$SOCK" --fixed-status --mcp none --settings none --installer none \
        >"$LOG" 2>&1 &
    CPID=$!
    PIDS+=("$CPID")
    started "$LOG"
}
client() { # out-file args..: a wyclient in the background
    local out=$1
    shift
    WAYLAND_DISPLAY=$SOCK "$WYC" "$@" >"$out" 2>&1 &
    PIDS+=("$!")
    CLPID=$!
}
mapped() { # title
    for _ in $(seq 100); do ctl list 2>/dev/null | grep -q "title=\"$1\"" && return 0; sleep 0.1; done
    fail "$1 did not map: $(ctl list 2>&1 | head -5)"
}
stat_of() { ctl stats | tr ' ' '\n' | sed -n "s/^$1=//p"; }
same() { # A B what exact
    if [ "$4" = exact ]; then
        cmp -s "$1" "$2" || fail "$3: the pictures differ ($($IMG diff "$1" "$2" 2>&1 | tail -1))"
        echo "ok: $3: byte for byte"
    else
        $IMG diff "$1" "$2" >"$RUN/diff.out" 2>&1 || fail "$3: $(cat "$RUN/diff.out")"
        echo "ok: $3: $(cat "$RUN/diff.out")"
    fi
}
SYNC=0
for n in /dev/dri/renderD*; do [ -c "$n" ] && [ -r "$n" ] && SYNC=1 && break; done

# One renderer's round: $1 name (pixman|gles), $2 exact|tol, then the compositor command.
round() {
    local r=$1 how=$2
    shift 2
    echo "== $r: a wl_shm window, the reference"
    compositor "$r-shm" "$@"
    client "$RUN/$r-shm.out" --title one --pattern 0
    mapped one
    sleep 3
    ctl shot "$RUN/$r-shm.ppm" >/dev/null
    ctl quit >/dev/null 2>&1 || true

    echo "== $r: the same window from dma-bufs"
    compositor "$r-dma" "$@"
    grep -q "dmabuf: zwp_linux_dmabuf_v1 for renderer $r, main device 226:" "$LOG" || fail "$r: no dma-buf global: $(grep dmabuf "$LOG")"
    client "$RUN/$r-dma.out" --title one --pattern 0 --dmabuf
    mapped one
    sleep 3
    ctl shot "$RUN/$r-dma.ppm" >/dev/null
    local fb
    fb=$(grep "^dmabuf feedback" "$RUN/$r-dma.out") || fail "$r: no feedback line: $(head -5 "$RUN/$r-dma.out")"
    echo "$fb" | grep -Eq "main_device=226:[0-9]+ formats=[1-9][0-9]* first=XR24:LINEAR tranches=1 " || fail "$r: feedback: $fb"
    grep -q "^dmabuf created 0" "$RUN/$r-dma.out" || fail "$r: no buffer created: $(cat "$RUN/$r-dma.out")"
    echo "ok: $r: $fb"
    same "$RUN/$r-shm.ppm" "$RUN/$r-dma.ppm" "$r: the dma-buf window looks like the wl_shm one" "$how"
    [ "$(stat_of dmabuf_imported)" -ge 1 ] || fail "$r: ctl stats counts no import: $(ctl stats)"

    echo "== $r: a modifier the renderer cannot read is answered failed"
    WAYLAND_DISPLAY=$SOCK timeout 10 "$WYC" --title mod --dmabuf --modifier 0x0200000000000001 >"$RUN/$r-mod.out" 2>&1 && rc=0 || rc=$?
    [ "$rc" = 3 ] && grep -q "^dmabuf failed" "$RUN/$r-mod.out" || fail "$r: modifier: exit $rc: $(tail -3 "$RUN/$r-mod.out")"
    # Pixman refuses it in its import; a CPU-reading GLES (llvmpipe) already in the probe.
    grep -Eq "dmabuf: ($r refuses a .* XR24:0x200000000000001 buffer|refused before import: XR24:0x200000000000001)" "$LOG" \
        || fail "$r: no refusal line: $(grep dmabuf "$LOG")"
    alive "$r: modifier"
    echo "ok: $r: $(grep -m1 -E "refuses|refused" "$LOG" | sed 's/^bitscomp: //')"

    echo "== $r: buffers no renderer may read"
    for k in stride:failed memfd:failed oversize:proto; do
        WAYLAND_DISPLAY=$SOCK timeout 10 "$WYC" --title bad --dmabuf --dmabuf-bad "${k%:*}" >"$RUN/$r-bad.out" 2>&1 && rc=0 || rc=$?
        if [ "${k#*:}" = failed ]; then
            [ "$rc" = 3 ] && grep -q "^dmabuf failed" "$RUN/$r-bad.out" || fail "$r: --dmabuf-bad ${k%:*}: exit $rc: $(tail -3 "$RUN/$r-bad.out")"
        else
            grep -q "Protocol error 6 on object zwp_linux_buffer_params_v1" "$RUN/$r-bad.out" \
                || fail "$r: --dmabuf-bad ${k%:*}: exit $rc: $(tail -3 "$RUN/$r-bad.out")"
        fi
        alive "$r: --dmabuf-bad ${k%:*}"
    done
    grep -q "refused before import: plane 0: stride 4 below a row" "$LOG" || fail "$r: no stride refusal: $(grep dmabuf "$LOG")"
    grep -q "refused before import: plane 0: not a dma-buf" "$LOG" || fail "$r: no memfd refusal: $(grep dmabuf "$LOG")"
    echo "ok: $r: a too short stride and a plain memfd are answered failed, rows past the end out_of_bounds"
    ctl quit >/dev/null 2>&1 || true

    if [ "$SYNC" = 0 ]; then
        echo "SKIP ($r explicit sync): no render node here"
        return
    fi
    echo "== $r: explicit sync"
    compositor "$r-sync" "$@"
    grep -q "syncobj: wp_linux_drm_syncobj_manager_v1 on /dev/dri/renderD" "$LOG" || {
        echo "SKIP ($r explicit sync): $(grep 'syncobj:' "$LOG")"
        ctl quit >/dev/null 2>&1 || true
        return
    }
    client "$RUN/$r-anim.out" --title anim --dmabuf --syncobj --animate
    mapped anim
    local t0
    t0=$(date +%s%N)
    client "$RUN/$r-late.out" --title late --dmabuf --syncobj --acquire-delay 1500 --pattern 1
    sleep 0.8
    ! grep -q "of pid $CLPID: first frame" "$LOG" || fail "$r: the late window showed a frame before its acquire point"
    for _ in $(seq 100); do grep -q "of pid $CLPID: first frame" "$LOG" && break; sleep 0.1; done
    grep -q "of pid $CLPID: first frame" "$LOG" || fail "$r: the late window never showed a frame: $(tail -3 "$RUN/$r-late.out")"
    local dt=$((($(date +%s%N) - t0) / 1000000))
    [ "$dt" -ge 1400 ] || fail "$r: the late window's first frame came after $dt ms (acquire point at 1500 ms)"
    grep -q "^syncobj acquire signalled" "$RUN/$r-late.out" || fail "$r: late: $(cat "$RUN/$r-late.out")"
    echo "ok: $r: the late window's first frame waited for its acquire point (seen after $dt ms)"
    sleep 4.5
    local fr
    fr=$(grep "^frames" "$RUN/$r-anim.out" | tail -1)
    [ -n "$fr" ] || fail "$r: anim printed no frames line: $(tail -3 "$RUN/$r-anim.out")"
    local sr st
    sr=$(echo "$fr" | sed -n 's/.* sync_released=\([0-9]*\).*/\1/p')
    st=$(echo "$fr" | sed -n 's/.* starved=\([0-9]*\).*/\1/p')
    [ "${sr:-0}" -ge 100 ] || fail "$r: release points: $fr"
    # starved counts callbacks with both buffers held; a busy host may lose a few, never most.
    [ "${st:-0}" -le $((sr / 10)) ] || fail "$r: buffers held too long: $fr"
    [ "$(stat_of release_points)" -ge 100 ] && [ "$(stat_of acquire_waits)" -ge 1 ] || fail "$r: ctl stats: $(ctl stats)"
    echo "ok: $r: $fr; $(ctl stats | tr ' ' '\n' | grep -E 'acquire_waits|held_buffers|release_points' | tr '\n' ' ')"
    for k in no-release:5 shm:2 conflict:6; do
        WAYLAND_DISPLAY=$SOCK timeout 10 "$WYC" --title brk --dmabuf --syncobj --syncobj-break "${k%:*}" >"$RUN/$r-brk.out" 2>&1 && rc=0 || rc=$?
        grep -q "Protocol error ${k#*:} on object wp_linux_drm_syncobj_surface_v1" "$RUN/$r-brk.out" \
            || fail "$r: --syncobj-break ${k%:*}: exit $rc: $(tail -2 "$RUN/$r-brk.out")"
        alive "$r: --syncobj-break ${k%:*}"
    done
    echo "ok: $r: no_release_point, unsupported_buffer and conflicting_points end only the client"

    echo "== $r: commits that never get their acquire point, a buffer destroyed while it waits"
    local fd0 fd1 fd2
    fd0=$(ls "/proc/$CPID/fd" | wc -l)
    client "$RUN/$r-flood.out" --title flood --dmabuf --syncobj --syncobj-break flood
    local fpid=$CLPID
    for _ in $(seq 100); do grep -q "^flood committed" "$RUN/$r-flood.out" && break; sleep 0.1; done
    grep -q "^flood committed 500" "$RUN/$r-flood.out" || fail "$r: flood: $(tail -3 "$RUN/$r-flood.out")"
    for _ in $(seq 50); do [ "$(stat_of acquire_pending)" -ge 500 ] && break; sleep 0.1; done
    [ "$(stat_of acquire_pending)" -ge 500 ] || fail "$r: flood: $(ctl stats)"
    fd1=$(ls "/proc/$CPID/fd" | wc -l)
    [ $((fd1 - fd0)) -le 30 ] || fail "$r: 500 waiting commits cost $((fd1 - fd0)) fds"
    alive "$r: flood"
    kill -9 "$fpid"
    for _ in $(seq 50); do [ "$(stat_of acquire_pending)" = 0 ] && break; sleep 0.1; done
    [ "$(stat_of acquire_pending)" = 0 ] && [ "$(stat_of held_now)" = 0 ] || fail "$r: after the flood client: $(ctl stats)"
    fd2=$(ls "/proc/$CPID/fd" | wc -l)
    [ $((fd2 - fd0)) -le 5 ] || fail "$r: $((fd2 - fd0)) fds left after the flood client went"
    echo "ok: $r: 500 waiting commits cost $((fd1 - fd0)) fds, all gone with the client (fds $fd0 -> $fd1 -> $fd2)"
    client "$RUN/$r-dw.out" --title dw --dmabuf --syncobj --syncobj-break destroy-waiting
    for _ in $(seq 50); do grep -q "^syncobj acquire signalled" "$RUN/$r-dw.out" && break; sleep 0.1; done
    grep -q "^destroyed the waiting buffer" "$RUN/$r-dw.out" && grep -q "^syncobj acquire signalled" "$RUN/$r-dw.out" \
        || fail "$r: destroy-waiting: $(tail -3 "$RUN/$r-dw.out")"
    sleep 0.5
    alive "$r: destroy-waiting"
    kill "$CLPID" 2>/dev/null || true
    echo "ok: $r: a waiting commit whose buffer was destroyed applies after its point without harm"
    ctl quit >/dev/null 2>&1 || true
}

round pixman exact "$BIN"
grep -q " build=rescue renderer=pixman " "$RUN/pixman-dma.log" || fail "rescue start line: $(grep WAYLAND_DISPLAY "$RUN/pixman-dma.log")"

if [ -n "$GPU" ]; then
    round gles tol "$GPU" --renderer gles
    grep -q " build=gpu renderer=gles " "$RUN/gles-dma.log" || fail "gles start line: $(grep WAYLAND_DISPLAY "$RUN/gles-dma.log")"
    grep -q "drawing with pixman" "$RUN"/gles-*.log && fail "gles fell back to pixman: $(grep -h 'drawing with pixman' "$RUN"/gles-*.log)"

    echo "== the Mesa stage's clients against bitscomp (dma-buf global present)"
    compositor mesa "$BIN"
    WAYLAND_DISPLAY=$SOCK MESA_VK_DRIVERS=lvp_icd timeout 4 "$RUN/tree/run" vkcube --wsi wayland >"$RUN/vkcube.out" 2>&1 && rc=0 || rc=$?
    [ "$rc" = 124 ] || fail "vkcube (lavapipe) ended with $rc: $(tail -5 "$RUN/vkcube.out")"
    alive "vkcube"
    WAYLAND_DISPLAY=$SOCK LIBGL_ALWAYS_SOFTWARE=1 timeout 4 "$RUN/tree/run" eglgears_wayland >"$RUN/eglgears.out" 2>&1 && rc=0 || rc=$?
    [ "$rc" = 124 ] || fail "eglgears_wayland (llvmpipe) ended with $rc: $(tail -5 "$RUN/eglgears.out")"
    alive "eglgears"
    echo "ok: lavapipe vkcube and llvmpipe eglgears_wayland drew for 4 s each (Mesa's software WSI sends wl_shm: $(stat_of commits) commits)"
    ctl quit >/dev/null 2>&1 || true
fi

echo "GREEN: dma-buf windows (udmabuf, LINEAR, feedback 226:x) and explicit sync work with Pixman${GPU:+ and GLES}"
