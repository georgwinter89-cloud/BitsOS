#!/bin/bash
# slow: YT-4a: Starboard's conformance suite nplb on V2 in QEMU (-smp 4, 4 GB, intel-hda + hda-duplex into a WAV
# file: nothing plays on the host), its pass count against the recorded floor (nplb-floor.txt).
# The image: `cargo xtask image` with V2_STICK_TREES=cobalt (the Cobalt tree in \BITSOS2\cobalt\), then nplb's stage
# (user/ports/cobalt-launcher/scripts/stage-nplb-v2.sh, outside git) copied next to loader_app and the FAT image
# made again from that ESP tree (the manifest leaves nplb out: 300 MB would ride in every gate's ktest image).
# Filter = YT-1's framebuffer reference run (nplb-yt.sh fb) plus PosixSocketResolveTest.SunnyDay* (they resolve
# dns.google: no outside lookups from a scenario). SbTimeZoneGetNameTest.IsIANAFormat runs again since INT-W6 made
# /etc/localtime a link (YT-4a CR 1).
# QEMU's RTC runs in local time as the Mini-PC's does (xtask passes -rtc base=localtime to every run since INT-W6).
# Judgement: nplb reaches gtest's end line; every failing test is listed in nplb-v2-known-fails.txt (a name outside it
# is red: the racy SchedParam is listed there); the pass count is at least nplb-floor.txt (a backstop for tests that
# turn into skips); no ENOSYS other than rseq (334). Missing stage: a SKIP line (NPLB_REQUIRE=1 makes it red).
# Time: MCP wait <= 120 s + nplb <= 300 s + 60 s poll slack + the copy back fit QEMU's --timeout 540; with the image
# build (~450 MB copied) the worst case passes the suite's default 600 s: run it with --script-timeout 900.
# Measured in QEMU: about 60 s in all.
set -eu
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
RUN="${BITSOS_RUN_DIR:-target/xtask/runs/yt-10-nplb}"
mkdir -p "$RUN"
RUN="$(realpath "$RUN")"
HERE="$(pwd)/host/tests/yt"
# shellcheck source=host/tests/mcp/lib.bash
. "$(pwd)/host/tests/mcp/lib.bash"
LANE="${BITSOS_LANE:-YT}"
LO="${BITSOS_PORT_LO:-29000}"
FWD=$((LO + 10))
TOKEN="yt4a-nplb-scenario-7d2e"
NAME="$LANE-nplb"
STAGE="${NPLB_STAGE:-/var/tmp/claude-v2/yt/cobalt/v2-stick/nplb}"
COBALT_STAGE="${COBALT_STAGE:-/var/tmp/claude-v2/yt/cobalt/v2-stick/cobalt}"
FLOOR="$(grep -v '^#' "$HERE/nplb-floor.txt" | head -1)"
FILTER='-*Player*:*Egl*:*EGL*:*Gles*:*GLES*:*VideoDecoder*:*DecodeTarget*:*VideoRender*:*Multicast*:VerticalVideoTests*'
FILTER="$FILTER:*SbMediaSetAudioWriteDuration*:*PosixSocketResolveTest.SunnyDay*"

if [ ! -f "$STAGE/elf_loader_sandbox" ] || [ ! -f "$COBALT_STAGE/loader_app" ]; then
    echo "SKIP: no Cobalt/nplb stage on this host (YT-1's build; user/ports/cobalt-launcher/scripts/stage-v2.sh and stage-nplb-v2.sh make it)"
    [ "${NPLB_REQUIRE:-0}" = 1 ] && { echo "FAIL: NPLB_REQUIRE=1 and no stage"; exit 1; }
    exit 0
fi
V2_STICK_TREES=cobalt cargo xtask image --name "$NAME" --config-extra "token=$TOKEN;mcp_port=8765;loglevel=info"
ESP="target/xtask/esp-$NAME"
IMG="target/xtask/$NAME.img"
cp -r --reflink=auto "$STAGE/." "$ESP/BITSOS2/cobalt/"
# Size as xtask's image_kib: content + 1/4 + 16 MiB, whole MiB, at least 64 MiB.
B=$(du -sb "$ESP" | cut -f1)
KIB=$(( (B + B / 4 + (16 << 20) + (1 << 20) - 1) / (1 << 20) * 1024 ))
[ "$KIB" -ge 65536 ] || KIB=65536
rm -f "$IMG"
mkfs.fat -F 32 -n BITSOS -C "$IMG" "$KIB" > /dev/null
mcopy -s -i "$IMG" "$ESP/EFI" "$ESP/BITSOS2" ::/
echo "image: $IMG with nplb ($((KIB / 1024)) MiB)"

stop_qemu "$NAME"
LOG="$RUN/serial.log"
rm -f "$LOG" "$RUN/nplb.txt" "$RUN/out.wav"
cargo xtask qemu --name "$NAME" --net user --hostfwd "$FWD:8765" --timeout 540 --smp 4 --mem 4G --serial-log "$LOG" -- \
    -audiodev "wav,id=a0,path=$RUN/out.wav,out.frequency=48000,out.channels=2,out.format=s16" \
    -device intel-hda,id=h0 -device hda-duplex,bus=h0.0,audiodev=a0,mixer=off > "$RUN/qemu.out" 2>&1 &
QPID=$!
trap 'stop_run "$QPID" "$NAME"' EXIT
fail=0
python3 -I "$HERE/yt_mcp.py" "$FWD" "$TOKEN" wait 120
python3 -I "$HERE/yt_mcp.py" "$FWD" "$TOKEN" nplb "$RUN" "$FILTER" 300 || fail=1
stop_run "$QPID" "$NAME"
trap - EXIT

T="$RUN/nplb.txt"
[ -s "$T" ] || { echo "FAIL: no nplb output fetched from V2 ($T)"; exit 1; }
grep -E '^\[==========\] .* ran|^\[  PASSED  \]' "$T" || true
{ grep '^\[  FAILED  \] [A-Za-z]' "$T" || true; } | grep -v 'listed below' |
    sed -e 's/^\[  FAILED  \] //' -e 's/, where GetParam.*//' -e 's/ ([0-9]* ms)$//' | sort -u > "$RUN/failed-names.txt"
grep -v '^#' "$HERE/nplb-v2-known-fails.txt" | sort -u > "$RUN/known.txt"
comm -23 "$RUN/failed-names.txt" "$RUN/known.txt" > "$RUN/new-failures.txt"
{ grep -a ' syscall enosys ' "$LOG" || true; } | sed 's/^\[[ 0-9.]*\] //' | tee "$RUN/enosys.txt"
PASSED=$(sed -n 's/^\[  PASSED  \] \([0-9]*\) test.*/\1/p' "$T" | head -1)
if [ "$fail" = 1 ] || ! grep -q '^\[==========\] .* ran' "$T"; then
    echo "FAIL: nplb did not reach its end (last test: $(grep '^\[ RUN' "$T" | tail -1 || true))"; exit 1
fi
red=0
if [ -s "$RUN/new-failures.txt" ]; then
    echo "FAIL: failures not in nplb-v2-known-fails.txt:"; sed 's/^/  /' "$RUN/new-failures.txt"; red=1
fi
if grep -v ' nr=334 ' "$RUN/enosys.txt" | grep -q .; then
    echo "FAIL: ENOSYS other than rseq (334):"; grep -v ' nr=334 ' "$RUN/enosys.txt" | sed 's/^/  /'; red=1
fi
if [ "${PASSED:-0}" -lt "$FLOOR" ]; then
    echo "FAIL: ${PASSED:-0} passed, below the floor $FLOOR (host/tests/yt/nplb-floor.txt)"; red=1
fi
[ "$red" = 0 ] || exit 1
# Measured 3,483 = floor + 2 with SchedParam failing; SchedParam passing adds 1; above floor + 3 a fix landed.
if [ "$PASSED" -gt $((FLOOR + 3)) ]; then
    echo "note: $PASSED passed, more than the floor $FLOOR + 3: raise nplb-floor.txt and drop fixed names from nplb-v2-known-fails.txt"
fi
echo "yt nplb: green ($PASSED passed, floor $FLOOR; $(wc -l < "$RUN/failed-names.txt") failed, all known)"
