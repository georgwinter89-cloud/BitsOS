#!/usr/bin/env bash
# WY-3 on the host, headless back end (nothing on the screen): bitscomp's X11 window manager against the host's own
# Xwayland, in a user, network and mount namespace of its own (a private /tmp and abstract socket space, so the
# display bitscomp claims is never the host's). Checks: no Xwayland until the first X11 client; xdpyinfo and xrandr
# answer; with the Xwayland tree's test clients (user/ports/xwayland, XW_TEST=1; they run with the host's
# libraries here) also an X11 window placed and framed, xinput's seat devices, the clipboard both ways and close.
# The QEMU half (our own Xwayland build on V2) is 94-xwayland.sh.
set -euo pipefail

ROOT=${BITSOS_ROOT:-$(cd "$(dirname "$0")/../../.." && pwd)}
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-$ROOT/target}
# Never under /tmp: the namespace mounts a private tmpfs there, which would hide the run directory.
RUN=${BITSOS_RUN_DIR:-$CARGO_TARGET_DIR/xtask/runs/wy16}
mkdir -p "$RUN"
RUN=$(realpath "$RUN")
case $RUN in /tmp|/tmp/*) echo "FAIL: the run directory $RUN lies under /tmp (hidden in the namespace)"; exit 1 ;; esac
BIN=$CARGO_TARGET_DIR/release/bitscomp
XW=${XWAYLAND:-/usr/bin/Xwayland}
TREE=$CARGO_TARGET_DIR/wy-xwayland/xwayland/test

for t in "$XW" xdpyinfo xrandr unshare; do
    command -v "$t" >/dev/null || { echo "SKIP: no $t on this host"; exit 0; }
done
if ! unshare -Urnm true 2>/dev/null; then
    echo "SKIP: no unprivileged user namespaces here (the test needs a private /tmp)"
    exit 0
fi
(cd "$ROOT/user/services/bitscomp" && cargo build --release -q --offline)

# Everything below runs inside the namespace (root there is the caller outside).
cat >"$RUN/inner.sh" <<'INNER'
set -u
# C1: without its private /tmp bitscomp would claim display numbers against the host's /tmp/.X11-unix.
mount -t tmpfs tmpfs /tmp || { echo "FAIL: no private /tmp (mount -t tmpfs in the namespace)"; exit 1; }
cd "$RUN"
export XDG_RUNTIME_DIR=$RUN/rt
rm -rf rt; mkdir -m 700 rt
unset DISPLAY WAYLAND_DISPLAY
fail() { echo "FAIL: $*"; echo "/tmp/.X11-unix: $(ls -a /tmp/.X11-unix 2>&1 | tr '\n' ' ')"; "$BIN" ctl --socket wy3 list; "$BIN" ctl --socket wy3 xwayland; grep -v '^>' comp.log | tail -20; kill $C; exit 1; }
ctl() { "$BIN" ctl --socket wy3 "$@"; }
X=(/usr/lib/ld-linux-x86-64.so.2 --library-path "$TREE/lib")
"$BIN" --backend headless --size 1280x800 --socket wy3 --xwayland "$XW" --mcp none --settings none --installer none >comp.log 2>&1 &
C=$!
# INT-W28: under a loaded host bitscomp may take longer than 5 s to start: wait for its line too (20 s).
for _ in $(seq 100); do [ -S rt/wy3.ctl ] && grep -q "xwayland on demand" comp.log && break; sleep 0.2; done
grep -q "xwayland on demand: DISPLAY=:0 ($XW)" comp.log || fail "bitscomp does not hold display :0"
ctl xwayland | grep -q "state=idle" || fail "Xwayland runs before any X11 client"
export DISPLAY=:0 WAYLAND_DISPLAY=wy3
timeout 20 xdpyinfo >xdpyinfo.txt 2>&1 || fail "xdpyinfo: $(tail -3 xdpyinfo.txt)"
grep -q "XInputExtension" xdpyinfo.txt || fail "xdpyinfo lists no XInputExtension"
grep -q "xwm: window manager ready" comp.log || fail "the window manager did not come up"
echo "ok: the first X11 client started Xwayland; xdpyinfo answers"
timeout 20 xrandr >xrandr.txt 2>&1 && grep -Eq "^BITS-1 connected [0-9]+x[0-9]+\+0\+0" xrandr.txt || fail "xrandr: $(cat xrandr.txt)"
echo "ok: xrandr shows BITS-1"
# A program bitscomp starts (as from the Feld or the launcher) gets the display.
printf 'printf %%s "$DISPLAY" >%s/spawned-display.txt\n' "$RUN" >spawn.sh
ctl spawn /bin/sh "$RUN/spawn.sh" >/dev/null
for _ in $(seq 25); do [ -s spawned-display.txt ] && break; sleep 0.2; done
[ "$(cat spawned-display.txt 2>/dev/null)" = ":0" ] || fail "a program bitscomp started sees DISPLAY='$(cat spawned-display.txt 2>/dev/null)'"
echo "ok: programs bitscomp starts get DISPLAY=:0"
# An Xwayland that ends at once, and one that never gets ready: a second bitscomp (display :1) gives up after
# three quick ends, and the waiting X client is refused instead of hanging.
negative() { # name, fake Xwayland body, seconds an X client may take
    printf '#!/bin/sh\n%s\n' "$2" >"fake-$1.sh"; chmod +x "fake-$1.sh"
    "$BIN" --backend headless --size 800x600 --socket "neg-$1" --xwayland "$RUN/fake-$1.sh" --mcp none --settings none --installer none >"neg-$1.log" 2>&1 &
    local N=$! d
    # C1: the check is on the display value itself (polled up to 30 s), not on a loop that may end without it.
    d=
    for _ in $(seq 150); do
        [ -S "rt/neg-$1.ctl" ] && d=$(sed -n 's/.*xwayland on demand: DISPLAY=\(:[0-9]*\) .*/\1/p' "neg-$1.log")
        [ -n "$d" ] && break
        sleep 0.2
    done
    [ -n "$d" ] && [ "$d" != ":0" ] || { kill $N; fail "the second bitscomp holds no display of its own ($d)"; }
    local t0=$SECONDS
    DISPLAY=$d timeout "$3" xdpyinfo >/dev/null 2>&1; local rc=$?
    for _ in $(seq 25); do "$BIN" ctl --socket "neg-$1" xwayland | grep -q "state=failed" && break; sleep 0.2; done
    local st; st=$("$BIN" ctl --socket "neg-$1" xwayland)
    kill $N; wait $N 2>/dev/null
    [ $rc -ne 124 ] || fail "$1: an X client hung for $3 s (state: $st)"
    echo "$st" | grep -q "state=failed .*starts=3 " || fail "$1: the display did not give up after three quick ends: $st"
    echo "ok: $1 Xwayland: refused after $((SECONDS - t0)) s, $st"
}
negative exits 'exit 1' 10
negative hangs 'exec sleep 120' 45
if [ ! -x "$TREE/bin/xeyes" ]; then
    echo "note: no test clients in $TREE (XW_TEST=1 user/ports/xwayland/scripts/build-v2.sh): windows and clipboard not checked here"
    kill $C; wait $C 2>/dev/null; exit 0
fi
"${X[@]}" "$TREE/bin/xeyes" 2>/dev/null & E=$!
for _ in $(seq 50); do ctl list | grep -q "app_id=XEyes .* x11=0x" && break; sleep 0.2; done
ctl list | grep -q "focused=true.*app_id=XEyes .* x11=0x" || fail "xeyes is not a focused window of the desk"
echo "ok: xeyes is a window: $(ctl list | grep XEyes)"
timeout 20 "${X[@]}" "$TREE/bin/xinput" list >xinput.txt 2>&1 && grep -q "xwayland-pointer" xinput.txt && grep -q "xwayland-keyboard" xinput.txt || fail "xinput: $(cat xinput.txt)"
echo "ok: xinput list shows Xwayland's seat devices"
printf 'von X11 nach Wayland' >in.txt
timeout 5 "${X[@]}" "$TREE/bin/xclip" -selection clipboard -i in.txt
for _ in $(seq 25); do grep -q "xsel: an X11 program owns the clipboard" comp.log && break; sleep 0.2; done
got=$(timeout 5 "${X[@]}" "$TREE/bin/wl-paste" -n 2>&1) || true
[ "$got" = "von X11 nach Wayland" ] || fail "wl-paste got '$got'"
echo "ok: X11 -> Wayland clipboard"
timeout 5 "${X[@]}" "$TREE/bin/wl-copy" "Grüße von Wayland" || true
for _ in $(seq 25); do ctl xwayland | grep -q "clipboard=wayland->x11" && break; sleep 0.2; done
got=$(timeout 5 "${X[@]}" "$TREE/bin/xclip" -selection clipboard -o 2>&1) || true
[ "$got" = "Grüße von Wayland" ] || fail "xclip -o got '$got'"
echo "ok: Wayland -> X11 clipboard"
# Target STRING is Latin-1 (ICCCM 2.6.2): the UTF-8 text arrives converted.
timeout 5 "${X[@]}" "$TREE/bin/xclip" -selection clipboard -o -t STRING >latin1.bin 2>&1 || true
printf 'Gr\374\337e von Wayland' >latin1.want
cmp -s latin1.want latin1.bin || fail "xclip -t STRING got '$(od -c latin1.bin | head -2)'"
echo "ok: STRING answers are Latin-1"
# Several hundred KB: more than one INCR piece towards X11 (64 KiB each), compared by checksum both ways.
head -c 400000 /dev/urandom | base64 -w 0 >big.txt
timeout 10 "${X[@]}" "$TREE/bin/wl-copy" <big.txt || true
for _ in $(seq 25); do ctl xwayland | grep -q "clipboard=wayland->x11" && break; sleep 0.2; done
timeout 20 "${X[@]}" "$TREE/bin/xclip" -selection clipboard -o >big-x.txt 2>&1 || true
cmp -s big.txt big-x.txt || fail "a large Wayland clipboard reached X11 as $(wc -c <big-x.txt) of $(wc -c <big.txt) bytes"
timeout 10 "${X[@]}" "$TREE/bin/xclip" -selection clipboard -i big.txt
for _ in $(seq 25); do ctl xwayland | grep -q "clipboard=x11->wayland" && break; sleep 0.2; done
timeout 20 "${X[@]}" "$TREE/bin/wl-paste" -n >big-w.txt 2>&1 || true
cmp -s big.txt big-w.txt || fail "a large X11 clipboard reached Wayland as $(wc -c <big-w.txt) of $(wc -c <big.txt) bytes"
echo "ok: $(wc -c <big.txt) bytes through the clipboard both ways (INCR)"
ctl close >/dev/null
for _ in $(seq 25); do kill -0 $E 2>/dev/null || break; sleep 0.2; done
kill -0 $E 2>/dev/null && fail "xeyes did not end after ctl close"
ctl list | grep -q "x11=0x" && fail "the closed X11 window is still listed"
echo "ok: ctl close ends xeyes (WM_DELETE_WINDOW)"
ctl xwayland | grep -q "starts=1 " || fail "the X11 clients did not share one Xwayland: $(ctl xwayland)"
echo "ok: every X11 client used the same Xwayland"
# Xwayland killed: its display waits for the next client and starts it again (not a quick end: it ran long).
XP=$(ctl xwayland | sed -n 's/.* pid=\([0-9]*\) .*/\1/p')
[ -n "$XP" ] && [ "$XP" != 0 ] || fail "no Xwayland pid in: $(ctl xwayland)"
sleep 2.2
kill "$XP"
for _ in $(seq 25); do ctl xwayland | grep -q "state=idle" && break; sleep 0.2; done
ctl xwayland | grep -q "state=idle" || fail "after Xwayland was killed: $(ctl xwayland)"
timeout 20 xdpyinfo >xdpyinfo2.txt 2>&1 || fail "xdpyinfo after the restart: $(tail -3 xdpyinfo2.txt)"
ctl xwayland | grep -q "starts=2 " || fail "Xwayland did not start again: $(ctl xwayland)"
echo "ok: a killed Xwayland starts again for the next X11 client"
ctl xwayland
kill $C; wait $C 2>/dev/null
exit 0
INNER
RUN=$RUN BIN=$BIN XW=$XW TREE=$TREE unshare -Urnm bash "$RUN/inner.sh"
echo "GREEN: X11 window manager against the host's Xwayland"
