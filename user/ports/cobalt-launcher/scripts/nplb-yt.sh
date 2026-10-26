#!/usr/bin/env bash
# usage (in the container): nplb-yt.sh <name> <x11|fb> [extra gtest args]
# Runs Starboard's conformance suite nplb (libnplb.so through elf_loader_sandbox)
# with the same filter as the V1 reference run (3,525 of 3,676 passed: graphics
# and player tests need GL/video output the container lacks; multicast hangs).
. /lane/user/ports/cobalt-launcher/scripts/yt-env.sh
NAME=$1; BACK=$2; shift 2
R=$RUNS/$NAME; rm -rf "$R"; mkdir -p "$R/home/.config/pulse"
yt_pulse
export HOME=$R/home
FILTER='-*Player*:*Egl*:*EGL*:*Gles*:*GLES*:*VideoDecoder*:*DecodeTarget*:*VideoRender*:*Multicast*'
# VerticalVideoTests: all 104 failed in the reference run too ("Invalid media
# time provider", no GL video output); here they abort the whole run when
# nplb's fake GL context finds no EGL config, so they are left out. The
# reference without them: 3,572 tests, 3,525 passed.
FILTER=$FILTER:VerticalVideoTests*
case $BACK in
  # Mesa's software GL, as in the reference run (this container has no GPU;
  # without these Mesa finds no EGL config and nplb's fake graphics context
  # aborts the whole run).
  x11) yt_x; export EGL_PLATFORM=x11 LIBGL_ALWAYS_SOFTWARE=1 ;;
  # The framebuffer backend has no GL at all (the failing stub EGL), so the
  # two audio-write-duration tests, which need nplb's fake GL context, are
  # left out as well (on V2 they cannot run either).
  fb)  mkfifo "$R/input"
       export BITSOS_BACKEND=fb BITSOS_FB=$R/fb0 BITSOS_FB_FAKE=2560x1440 BITSOS_EVDEV=$R/input
       FILTER=$FILTER:*SbMediaSetAudioWriteDuration* ;;
esac
cd "$OUT"
START=$(date +%s)
timeout 3600 ./elf_loader_sandbox --evergreen_library=app/nplb/lib/libnplb.so \
  --evergreen_content=app/nplb/content --gtest_filter="$FILTER" "$@" > "$R/nplb.txt" 2>&1
echo "NPLB_RC=$? SECONDS=$(( $(date +%s) - START ))" >> "$R/nplb.txt"
grep -E '^\[==========\] .* ran|^\[  PASSED  \]|FAILED TESTS|DISABLED|NPLB_RC' "$R/nplb.txt"
grep '^\[  FAILED  \]' "$R/nplb.txt" | sed 's/, where GetParam.*//' | sort -u > "$R/failed.txt"
echo "failed list: $R/failed.txt ($(wc -l < "$R/failed.txt") lines)"
# Gate: the run reached its end and every failure is one the reference run
# (V1 M0, SCHRITT1.md) had too. Exit 0 = PASS, 1 = FAIL.
REF=/lane/host/tests/yt/nplb-reference-fails.txt
grep '^\[  FAILED  \] [A-Za-z]' "$R/nplb.txt" | grep -v 'listed below' |
  sed -e 's/^\[  FAILED  \] //' -e 's/, where GetParam.*//' -e 's/ ([0-9]* ms)$//' |
  sort -u > "$R/failed-names.txt"
grep -v '^#' "$REF" | sort -u > "$R/reference-names.txt"
comm -23 "$R/failed-names.txt" "$R/reference-names.txt" > "$R/new-failures.txt"
PASSED=$(sed -n 's/^\[  PASSED  \] \([0-9]*\) test.*/\1/p' "$R/nplb.txt")
if ! grep -q '^\[==========\] .* ran' "$R/nplb.txt"; then
  echo "NPLB GATE: FAIL (the run did not reach its end; see $R/nplb.txt)"; exit 1
elif [ -s "$R/new-failures.txt" ]; then
  echo "NPLB GATE: FAIL ($(wc -l < "$R/new-failures.txt") failures not in the reference):"
  sed 's/^/  /' "$R/new-failures.txt"; exit 1
fi
echo "NPLB GATE: PASS ($PASSED passed; all $(wc -l < "$R/failed-names.txt") failures are in the reference list)"
