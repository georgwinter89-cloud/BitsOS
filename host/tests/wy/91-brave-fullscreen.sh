#!/usr/bin/env bash
# WY-8 with the real browser on the host (headless back end, nothing on the screen): Brave 1.97.59 from
# /opt/brave-bin with the stick launcher's flags and a fresh profile under the run dir. A page whose whole body is a
# "video" that goes fullscreen on a click (requestFullscreen, as YouTube's button does): Brave asks bitscomp,
# covers the whole screen, Esc gives it back; F11 (Brave's own browser fullscreen) the same. Skips without Brave.
set -euo pipefail

ROOT=${BITSOS_ROOT:-$(cd "$(dirname "$0")/../../.." && pwd)}
BRAVE=${BRAVE_BIN:-/opt/brave-bin/brave}
[ -x "$BRAVE" ] || { echo "SKIP (not run, counts as green): no Brave at $BRAVE"; exit 0; }
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-$ROOT/target}
# Short: the compositor's socket path has a length limit.
RUN=${BITSOS_RUN_DIR:-$CARGO_TARGET_DIR/wy91}
rm -rf "$RUN"
mkdir -p "$RUN"
RUN=$(realpath "$RUN")
BIN=$CARGO_TARGET_DIR/release/bitscomp
IMG="python3 -I $ROOT/host/tests/wy/tools/img.py"
export XDG_RUNTIME_DIR=$RUN/xdg
mkdir -m 700 -p "$XDG_RUNTIME_DIR" "$RUN/home"
unset DISPLAY WAYLAND_DISPLAY
SOCK=wy91-$$

fail() { echo "FAIL: $*"; tail -5 "$RUN/brave.log" 2>/dev/null; exit 1; }

echo "== build bitscomp"
(cd "$ROOT/user/services/bitscomp" && cargo build --release -q --offline)

PG="" COMP=""
cleanup() {
    if [ -n "$PG" ]; then
        kill -TERM -- "-$PG" 2>/dev/null || true
        for _ in $(seq 50); do kill -0 -- "-$PG" 2>/dev/null || break; sleep 0.2; done
        kill -KILL -- "-$PG" 2>/dev/null || true
    fi
    [ -n "$COMP" ] && kill "$COMP" 2>/dev/null || true
    wait 2>/dev/null || true
    rm -rf "$RUN/profile" "$RUN/cache" "$RUN/home"
}
trap cleanup EXIT

ctl() { "$BIN" ctl --socket "$SOCK" "$@"; }
wait_for() { # pattern, seconds, command..., what
    local pat=$1 n=$2 what=${!#}
    for _ in $(seq $(( n * 10 ))); do
        "${@:3:$#-3}" 2>/dev/null | grep -q -- "$pat" && return 0
        sleep 0.1
    done
    fail "$what ($(ctl list 2>&1 | head -3 | tr '\n' ' '))"
}
pixel() { ctl shot "$RUN/p.ppm" >/dev/null; $IMG pixel "$RUN/p.ppm" "$1" "$2"; }

cat >"$RUN/page.html" <<'EOF'
<!doctype html><title>WY8 Vollbild</title>
<style>html,body{margin:0;height:100%;background:rgb(0,160,0)}</style>
<body onclick="document.documentElement.requestFullscreen()"></body>
EOF

echo "== bitscomp 1600x900 headless (1280x720 logical at its default scale 1.25), Brave with a fresh profile"
"$BIN" --backend headless --size 1600x900 --socket "$SOCK" --mcp none --fixed-status --settings none >"$RUN/bitscomp.log" 2>&1 &
COMP=$!
for _ in $(seq 50); do [ -S "$XDG_RUNTIME_DIR/$SOCK.ctl" ] && break; sleep 0.1; done
FLAGS="--ozone-platform=wayland --disable-gpu --password-store=basic --no-sandbox --disable-breakpad
--disable-crash-reporter --no-first-run --no-default-browser-check --autoplay-policy=no-user-gesture-required"
# shellcheck disable=SC2086
setsid bash -c 'echo $$ >"$0"; exec "$@"' "$RUN/pgid" env -i PATH=/usr/bin HOME="$RUN/home" LANG=de_DE.UTF-8 \
    XDG_RUNTIME_DIR="$XDG_RUNTIME_DIR" WAYLAND_DISPLAY="$SOCK" GTK_USE_PORTAL=0 NO_AT_BRIDGE=1 \
    DBUS_SESSION_BUS_ADDRESS=disabled: PULSE_SERVER=none \
    "$BRAVE" $FLAGS --user-data-dir="$RUN/profile" --disk-cache-dir="$RUN/cache" "file://$RUN/page.html" >"$RUN/brave.log" 2>&1 &
for _ in $(seq 50); do [ -s "$RUN/pgid" ] && break; sleep 0.1; done
PG=$(cat "$RUN/pgid" 2>/dev/null || true)
[ -n "$PG" ] || fail "no process group id from setsid"
wait_for 'title="WY8 Vollbild' 60 ctl list "Brave's window never showed the page"
wait_for 'focused=true.*title="WY8 Vollbild' 10 ctl list "Brave has no focus"
sleep 2
before=$(ctl list | grep 'title="WY8 Vollbild')
echo "before: $before"
[ "$(pixel 4 450)" != "0 160 0" ] && [ "$(pixel 800 897)" != "0 160 0" ] || fail "the bars are not there before"

echo "== a click on the page: requestFullscreen, Brave covers the whole screen"
ctl click 800 500 >/dev/null
wait_for 'fullscreen visible=true focused=true frame=0,0 1280x720 .*title="WY8 Vollbild' 10 ctl list "Brave did not go fullscreen on the page's request"
grep -q "enters fullscreen on its own request" "$RUN/bitscomp.log" || fail "no log line for Brave's request"
ok=0
# Brave's own --no-sandbox info bar stays at the top in its fullscreen; the workspace list (left), Claude's sidebar
# (right) and the status bar (bottom) must be the page.
for _ in $(seq 30); do [ "$(pixel 4 450)" = "0 160 0" ] && [ "$(pixel 1595 450)" = "0 160 0" ] && [ "$(pixel 800 897)" = "0 160 0" ] && { ok=1; break; }; sleep 0.2; done
[ "$ok" = 1 ] || fail "the page does not cover the bars: $(pixel 4 450) / $(pixel 1595 450) / $(pixel 800 897)"

echo "== Esc: Brave leaves the page's fullscreen, the window comes back"
ctl key 1 >/dev/null
wait_for 'leaves fullscreen on its own request' 10 cat "$RUN/bitscomp.log" "Esc did not end Brave's fullscreen"
wait_for 'tile=0 page=0/1 visible=true focused=true' 10 ctl list "Brave is not back in its tile"
after=$(ctl list | grep 'title="WY8 Vollbild')
[ "${after%% id=*}" = "${before%% id=*}" ] || fail "not back exactly: '$after' vs '$before'"

echo "== F11: Brave's own browser fullscreen, F11 again back"
ctl key 87 >/dev/null
wait_for 'fullscreen visible=true focused=true frame=0,0 1280x720' 10 ctl list "F11 did not make Brave fullscreen"
sleep 1
ctl key 87 >/dev/null
wait_for 'tile=0 page=0/1 visible=true focused=true' 10 ctl list "F11 again did not bring Brave back"
ctl list | grep -q " fullscreen " && fail "still fullscreen after the second F11"
echo "PASS: Brave ($("$BRAVE" --version 2>/dev/null | head -1)) fullscreen in bitscomp: page request (click), Esc, F11 twice"
