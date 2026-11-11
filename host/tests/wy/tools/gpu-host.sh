# WY-14 (sourced by host scenarios after their build step; needs ROOT, RUN, BIN): WY_GPU=1 runs the scenario's
# bitscomp as the rescue build that hands over to the gpu build (`--build gpu --gpu-exe`), linked for a host copy of
# the Mesa tree in $RUN/gpu-tree (tools/gpu-tree.sh; with `nested` like the scenarios' own build, for `ctl phys`).
# Sets START (the command to start bitscomp with) and gpu_host_check LOG PID (with WY_GPU=1: the hand-over line,
# the start line `build=gpu`, and that PID runs the gpu build). Without WY_GPU nothing changes: START is "$BIN".
# WY-15: WY_GPU=gles hands over with `--build gles`: the gpu build draws with GLES (llvmpipe) and the start line must
# say renderer=gles; the scenario's screenshots then compare GLES pictures with its references (its tolerance).
START=("$BIN")
gpu_host_check() { :; }
# WY-15: the screenshot tolerance for comparisons with the Pixman references (GL filters fractional scales a little
# differently: a few hundred pixels by more than 8, a handful by more than 24); empty unless WY_GPU=gles, so the
# scenarios keep their own (`${GPU_TOL:-8 0.0002}`).
GPU_TOL=
[ "${WY_GPU:-0}" = gles ] && GPU_TOL="24 0.003"
if [ "${WY_GPU:-0}" = 1 ] || [ "${WY_GPU:-0}" = gles ]; then
    if [ "$WY_GPU" = gles ]; then _build=gles _r=gles _over="build gpu, renderer gles"; else _build=gpu _r=pixman _over="build gpu"; fi
    _stage=${MESA_STAGE:-$ROOT/user/ports/mesa/out/mesa}
    if [ -z "${MESA_STAGE:-}" ] && [ ! -d "$_stage" ]; then
        # A worktree has no out/: the manifest's `tree mesa`, as dr/20 does; an empty answer keeps the path above.
        _m=$(awk '$1 == "tree" && $2 == "mesa" { sub(/^host:/, "", $3); print $3 }' "$ROOT/user/packaging/manifest.txt")
        [ -z "$_m" ] || _stage=$_m
    fi
    if [ ! -f "$_stage/lib/ld-linux-x86-64.so.2" ]; then
        _stage=$CARGO_TARGET_DIR/wy-mesa-out/mesa
        [ -f "$_stage/lib/ld-linux-x86-64.so.2" ] || MESA_OUT=$CARGO_TARGET_DIR/wy-mesa-out \
            bash "$ROOT/user/ports/mesa/scripts/stage-v2.sh" >"$RUN/gpu-stage.out" 2>&1 \
            || { echo "FAIL: WY_GPU=1: no Mesa stage: $(tail -3 "$RUN/gpu-stage.out")"; exit 1; }
    fi
    _run=$(realpath "$RUN")
    MESA_STAGE=$_stage BITSOS_ROOT=$ROOT WY_GPU_NESTED=1 bash "$ROOT/host/tests/wy/tools/gpu-tree.sh" "$_run/gpu-tree" \
        "$_run/bitscomp-gpu" "$_run/gpu-tree" >"$RUN/gpu-tree.out" 2>&1 \
        || { echo "FAIL: WY_GPU=1: gpu-tree.sh: $(tail -5 "$RUN/gpu-tree.out")"; exit 1; }
    grep "KNOWN-GAP" "$RUN/gpu-tree.out" || true
    START=("$BIN" --build "$_build" --gpu-exe "$_run/bitscomp-gpu")
    echo "== WY_GPU=$WY_GPU: bitscomp hands over to $_run/bitscomp-gpu (renderer $_r)"
    gpu_host_check() { # LOG PID
        local i
        for i in $(seq 100); do grep -q "^WAYLAND_DISPLAY=" "$1" && break; sleep 0.1; done
        grep -q "$_over (--build $_build): handing over to $_run/bitscomp-gpu" "$1" || { echo "FAIL: WY_GPU=$WY_GPU: no hand-over: $(head -5 "$1")"; exit 1; }
        grep -q " build=gpu renderer=$_r egl=loadable$" "$1" || { echo "FAIL: WY_GPU=$WY_GPU: start line: $(grep WAYLAND_DISPLAY "$1")"; exit 1; }
        [ "$(readlink "/proc/$2/exe")" = "$_run/bitscomp-gpu" ] || { echo "FAIL: WY_GPU=$WY_GPU: pid $2 runs $(readlink "/proc/$2/exe")"; exit 1; }
        echo "ok: WY_GPU=$WY_GPU: pid $2 is the gpu build, renderer $_r"
    }
fi
