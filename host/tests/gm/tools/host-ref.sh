#!/bin/bash
# GM-1a: the host's side of 10-mesa.sh: the staged tree's programs on CachyOS as V2 runs them (the tree's own
# loader and libraries, no /dev/dri: a tmpfs over it in a user namespace, so RADV/radeonsi find nothing), the window
# programs in a headless bitscomp at 1280x800 (never a window on the desktop), each with errlog.so preloaded.
#   host-ref.sh OUT STAGE ERRLOG_SO
# Writes OUT/<prog>.out (its output), OUT/err-<prog>.log (its failed calls), OUT/<prog>.ppm (bitscomp's picture
# after 8 s), OUT/bitscomp.log. Exit 0 = every program ran (the window ones still ran when stopped).
set -uo pipefail
OUT=$1 STAGE=$2 SHIM=$3
ROOT="$(cd "$(dirname "$0")/../../../.." && pwd)"
BIN=${BITSCOMP_BIN:-${CARGO_TARGET_DIR:-$ROOT/target}/release/bitscomp}
SOCK=gm-ref
[ -x "$BIN" ] || { echo "FAIL: no bitscomp at $BIN"; exit 1; }
[ ${#OUT} -le 80 ] || { echo "FAIL: $OUT is too long for the compositor's socket path (at most 80 bytes)"; exit 1; }
unshare -rm true 2>/dev/null || { echo "FAIL: no user namespace on this host (unshare -rm)"; exit 1; }
rm -rf "$OUT"
mkdir -m 700 -p "$OUT/xdg" "$OUT/home"
COMP=""
cleanup() { [ -n "$COMP" ] && kill "$COMP" 2>/dev/null; wait 2>/dev/null; rm -rf "$OUT/home"; }
trap cleanup EXIT
XDG_RUNTIME_DIR=$OUT/xdg "$BIN" --backend headless --size 1280x800 --socket "$SOCK" >"$OUT/bitscomp.log" 2>&1 &
COMP=$!
for _ in $(seq 50); do [ -S "$OUT/xdg/$SOCK.ctl" ] && break; sleep 0.2; done
ctl() { XDG_RUNTIME_DIR=$OUT/xdg "$BIN" ctl --socket "$SOCK" "$@"; }

# prog SECONDS ARGS: in the namespace without /dev/dri; SECONDS > 0 stops it then (a window program), after a shot.
rc=0
# TAG names the output files (default: the program); VKD picks the Vulkan drivers (MESA_VK_DRIVERS).
prog() {
  local p=$1 secs=$2 tag=${TAG:-$1}
  shift 2
  env -i PATH=/usr/bin HOME="$OUT/home" XDG_RUNTIME_DIR="$OUT/xdg" WAYLAND_DISPLAY="$SOCK" \
    LD_PRELOAD="$SHIM" ERRLOG_FILE="$OUT/err-$tag.log" ${VKD:+MESA_VK_DRIVERS=$VKD} \
    unshare -rm sh -c 'mount -t tmpfs none /dev/dri && exec "$@"' _ "$STAGE/run" "$p" "$@" >"$OUT/$tag.out" 2>&1 &
  local pid=$! code
  if [ "$secs" -gt 0 ]; then
    sleep "$secs"
    ctl shot "$OUT/$tag.ppm" >/dev/null 2>&1 || echo "note: no shot of $p"
    if kill -0 $pid 2>/dev/null; then kill -TERM $pid; wait $pid; echo "host: $p ran ${secs} s"
    else wait $pid; code=$?; echo "FAIL: host: $p ended by itself (code $code)"; rc=1; fi
  else
    wait $pid; code=$?
    echo "host: $tag ended with code $code"
    # eglinfo's code counts the platforms that failed (GBM, X11 here), vulkaninfo without a device fails; the
    # others must end with 0. A signal never.
    [ $code = 0 ] || { [ "$p" = eglinfo ] || [ -n "${VKD:-}" ]; } && [ $code -lt 128 ] || rc=1
  fi
}
prog vulkaninfo 0 --summary
TAG=vulkaninfo-radv VKD=radeon_icd prog vulkaninfo 0 --summary
prog eglinfo 0 -B
prog vkcube 8 --wsi wayland
prog eglgears_wayland 8
prog es2gears_wayland 8
grep -q "llvmpipe" "$OUT/vulkaninfo.out" || { echo "FAIL: host: vulkaninfo lists no llvmpipe (lavapipe)"; rc=1; }
exit $rc
