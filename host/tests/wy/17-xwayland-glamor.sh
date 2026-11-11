#!/usr/bin/env bash
# WY-3b on the host, headless (nothing on the screen), in a user, network and mount namespace of its own: V2's own
# Xwayland tree at its stick path (/boot/BITSOS2 is a private mount here, /dev/dri is hidden, so the host's GPU gets no
# job). bitscomp's choice of glamor and what happens when it fails, none of which needs a chip:
#   no node         XWAYLAND_NO_GLAMOR: xdpyinfo without DRI3, vkcube --wsi xcb on lavapipe gets its X11 window and frames,
#                   glxinfo says it has no GL and ends
#   quick end       a glamor start that ends at once: the waiting client is served by a software start, not counted
#   hung start      a glamor start that never serves: the same, after its deadline
#   no device       glamor asked, but linux-dmabuf names none (Pixman): Xwayland falls back itself, bitscomp says so
#   always ending   an Xwayland that ends in software too: the display still goes off after three (WY-3)
# Glamor itself on a device is 47-v2-xwayland-gpu.sh (fake node, QEMU) and host/tests/minipc/x11gpu.sh (the chip).
set -euo pipefail

ROOT=${BITSOS_ROOT:-$(cd "$(dirname "$0")/../../.." && pwd)}
cd "$ROOT"
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-$ROOT/target}
# Never under /tmp: the namespace mounts a private tmpfs there, which would hide the run directory.
RUN=${BITSOS_RUN_DIR:-$CARGO_TARGET_DIR/xtask/runs/wy17}
mkdir -p "$RUN"
RUN=$(realpath "$RUN")
case $RUN in /tmp|/tmp/*) echo "FAIL: the run directory $RUN lies under /tmp (hidden in the namespace)"; exit 1 ;; esac
BIN=$CARGO_TARGET_DIR/release/bitscomp
STAGE=${MESA_STAGE:-$ROOT/user/ports/mesa/out/mesa}
if [ -z "${MESA_STAGE:-}" ] && [ ! -d "$STAGE" ]; then
    # A worktree has no out/: the manifest's `tree mesa`, as gpu-image.sh does.
    STAGE=$(awk '$1 == "tree" && $2 == "mesa" { sub(/^host:/, "", $3); print $3 }' user/packaging/manifest.txt)
fi
[ -f "$STAGE/lib/ld-linux-x86-64.so.2" ] || { echo "RED: no Mesa stage in $STAGE: bash user/ports/mesa/scripts/stage-stick.sh"; exit 1; }
for t in xdpyinfo unshare; do
    command -v "$t" >/dev/null || { echo "RED: no $t on this host"; exit 1; }
done
if ! unshare -Urnm true 2>/dev/null; then
    echo "RED: no unprivileged user namespaces here (the test needs a private /tmp and /boot)"
    exit 1
fi
(cd user/services/bitscomp && cargo build --release -q --offline)
# XW_TEST: this build replaces the tree, and wy/16 and wy/94 need its test clients.
XW_TEST=1 MESA_STAGE=$STAGE bash user/ports/xwayland/scripts/build-v2.sh >"$RUN/build.txt" 2>&1 || { tail -20 "$RUN/build.txt"; echo "RED: the Xwayland tree does not build"; exit 1; }
TREE=$CARGO_TARGET_DIR/wy-xwayland/xwayland

cat >"$RUN/inner.sh" <<'INNER'
set -u
mount -t tmpfs tmpfs /tmp || { echo "FAIL: no private /tmp"; exit 1; }
mount -t tmpfs tmpfs /boot || { echo "FAIL: no private /boot"; exit 1; }
mkdir -p /boot/BITSOS2/mesa /boot/BITSOS2/xwayland
mount --bind "$STAGE" /boot/BITSOS2/mesa && mount --bind "$TREE" /boot/BITSOS2/xwayland || { echo "FAIL: no bind mounts at /boot/BITSOS2"; exit 1; }
if [ -d /dev/dri ]; then mount -t tmpfs tmpfs /dev/dri || { echo "FAIL: /dev/dri cannot be hidden"; exit 1; }; fi
cd "$RUN"
export XDG_RUNTIME_DIR=$RUN/rt HOME=$RUN
unset WAYLAND_DISPLAY DISPLAY
REAL=/boot/BITSOS2/xwayland/bin/Xwayland
ctl() { "$BIN" ctl --socket wy3b "$@"; }
fail() { echo "FAIL: $*"; ctl xwayland; grep -v '^>' comp.log | tail -25; kill $C 2>/dev/null; exit 1; }
start() { # args of bitscomp
    rm -rf rt; mkdir -m 700 rt; : >comp.log
    "$BIN" --backend headless --size 1280x800 --socket wy3b --mcp none --settings none --installer none "$@" >comp.log 2>&1 &
    C=$!
    for _ in $(seq 100); do [ -S rt/wy3b.ctl ] && grep -q "xwayland on demand" comp.log && break; sleep 0.2; done
    grep -q "xwayland on demand: DISPLAY=:0" comp.log || fail "bitscomp does not hold display :0"
    export WAYLAND_DISPLAY=wy3b DISPLAY=:0
}
stop() { kill $C 2>/dev/null; wait $C 2>/dev/null; rm -f /tmp/.X0-lock /tmp/.X11-unix/X0; unset WAYLAND_DISPLAY DISPLAY; }
wrap() { # name, what it does when asked for glamor; in software it is the real one
    printf '#!/bin/sh\n[ -n "$XWAYLAND_NO_GLAMOR" ] && exec %s "$@"\n%s\n' "$REAL" "$2" >"$1"; chmod +x "$1"
}

# No node: V2's tree in software, at the path bitscomp starts by default.
start
timeout 60 xdpyinfo >x1.txt 2>&1 || fail "xdpyinfo: $(tail -3 x1.txt)"
grep -q "X.Org version: 24.1.14" x1.txt || fail "not V2's Xwayland: $(head -5 x1.txt)"
grep -q "this start in software: " comp.log || fail "no line says why this start is in software"
grep -q "^    DRI3$" x1.txt && fail "DRI3 is offered without a node"
ctl xwayland | grep -q 'dri3=false glamor=off why=' || fail "ctl xwayland: $(ctl xwayland)"
echo "ok: no node: V2's Xwayland in software, no DRI3 ($(ctl xwayland | sed 's/.*glamor=/glamor=/'))"
MESA_VK_DRIVERS=lvp_icd /bin/sh /boot/BITSOS2/mesa/run vkcube --wsi xcb >vk.txt 2>&1 &
V=$!
for _ in $(seq 150); do ctl list | grep -q ' x11=0x' && break; sleep 0.2; done
ctl list | grep -q ' x11=0x' || fail "vkcube --wsi xcb has no X11 window: $(tail -5 vk.txt)"
a=$(ctl stats | sed -n 's/.* commits=\([0-9]*\) .*/\1/p')
for _ in $(seq 150); do
    sleep 0.2
    b=$(ctl stats | sed -n 's/.* commits=\([0-9]*\) .*/\1/p')
    [ "$b" -gt "$((a + 5))" ] && break
done
[ "$b" -gt "$((a + 5))" ] || fail "vkcube's frames do not arrive: commits $a -> $b; $(tail -5 vk.txt)"
kill $V 2>/dev/null; wait $V 2>/dev/null
echo "ok: no node: vkcube --wsi xcb on lavapipe presents, commits $a -> $b"
timeout 60 /bin/sh /boot/BITSOS2/xwayland/run glxinfo -B >gl.txt 2>&1
rc=$?
[ $rc != 124 ] || fail "glxinfo hangs without GL"
echo "ok: no node: glxinfo ends ($rc): $(head -1 gl.txt)"
timeout 30 xdpyinfo >x2.txt 2>&1 || fail "xdpyinfo after the GL client: $(tail -3 x2.txt)"
ctl xwayland | grep -q "starts=1 " || fail "the clients did not share one Xwayland"
stop

# A start with glamor that ends at once: the client waiting at the socket is served, the budget untouched.
: >node
wrap xw-quick 'exit 3'
start --xwayland "$RUN/xw-quick" --xwayland-glamor "force:$RUN/node"
timeout 60 xdpyinfo >x3.txt 2>&1 || fail "xdpyinfo after a failed glamor start: $(tail -3 x3.txt)"
grep -q "X.Org version: 24.1.14" x3.txt || fail "xdpyinfo: $(head -3 x3.txt)"
grep -q "a start with glamor on $RUN/node failed: the next start is in software" comp.log || fail "the failed start is not named"
ctl xwayland | grep -q 'starts=2 ' || fail "not one failure and one software start"
ctl xwayland | grep -q 'dri3=false glamor=off why="a start with glamor' || fail "ctl xwayland: $(ctl xwayland)"
echo "ok: a glamor start that ends at once: the client is served in software"
stop

# A start with glamor that never serves: the client waits through its deadline and is served.
wrap xw-hung 'exec sleep 3600'
start --xwayland "$RUN/xw-hung" --xwayland-glamor "force:$RUN/node"
timeout 90 xdpyinfo >x4.txt 2>&1 || fail "xdpyinfo after a hung glamor start: $(tail -3 x4.txt)"
grep -q "is not ready 30 s after its start" comp.log || fail "the deadline is not named"
grep -q "a start with glamor on $RUN/node failed" comp.log || fail "the hung start is not named"
ctl xwayland | grep -q 'starts=2 ' || fail "not one failure and one software start"
echo "ok: a glamor start that hangs: the client is served in software after its deadline"
stop

# Glamor asked with no device named (Pixman, 0:0): Xwayland falls back itself, bitscomp says so and stops looking.
start --xwayland "$REAL" --xwayland-glamor "force:$RUN/node"
timeout 60 xdpyinfo >x5.txt 2>&1 || fail "xdpyinfo: $(tail -3 x5.txt)"
grep -q "^    DRI3$" x5.txt && fail "DRI3 without a device"
for _ in $(seq 50); do grep -q "fell back to software" comp.log && break; sleep 0.2; done
grep -q "asked for glamor on $RUN/node, but no DRI3 is offered" comp.log || fail "the fallback is not named"
ctl xwayland | grep -q 'glamor=off why="Xwayland fell back' || fail "ctl xwayland: $(ctl xwayland)"
rm node
# Longer than one look (NODE_LOOK, 5 s), and it must not end: load can only make this wait too short, not red.
sleep 6
ctl xwayland | grep -q 'state=ready' || fail "a software Xwayland ended over the node"
echo "ok: glamor asked, no device named: Xwayland falls back itself and stays"
stop

# An Xwayland that ends in software too: one glamor failure, then WY-3's three quick ends.
: >node
printf '#!/bin/sh\nexit 3\n' >xw-dead; chmod +x xw-dead
start --xwayland "$RUN/xw-dead" --xwayland-glamor "force:$RUN/node"
timeout 30 xdpyinfo >x6.txt 2>&1 && fail "xdpyinfo answered without an X server"
for _ in $(seq 50); do ctl xwayland | grep -q 'state=failed' && break; sleep 0.2; done
ctl xwayland | grep -q 'state=failed pid=0 starts=4 ' || fail "not off after one glamor failure and three software ends: $(ctl xwayland)"
grep -q "3 quick ends in a row" comp.log || fail "the end of the display is not named"
echo "ok: an Xwayland that always ends: the display goes off after the glamor start and three more"
stop
exit 0
INNER
RUN=$RUN BIN=$BIN STAGE=$STAGE TREE=$TREE unshare -Urnm bash "$RUN/inner.sh"
rm -rf "$RUN/rt"
echo "GREEN: V2's Xwayland in software without a node; a failed glamor start costs an X11 client nothing"
