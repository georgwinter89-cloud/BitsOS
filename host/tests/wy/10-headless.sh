#!/usr/bin/env bash
# WY-0: bitscomp without a window (headless back end) on the host, driven over its control socket.
# Proves: foot maps as a tile, German keymap (umlauts typed into foot), two tiles, stack + flip, float,
# clipboard (if wl-clipboard is installed), screenshot, clean quit. Opens nothing on the screen.
set -euo pipefail

ROOT=${BITSOS_ROOT:-$(cd "$(dirname "$0")/../../.." && pwd)}
RUN=${BITSOS_RUN_DIR:-$(mktemp -d)}
mkdir -p "$RUN"
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-$ROOT/target}
BIN=$CARGO_TARGET_DIR/release/bitscomp
SOCK=wayland-wy-test-$$
export XDG_RUNTIME_DIR=${XDG_RUNTIME_DIR:-$RUN}
unset DISPLAY

if ! command -v foot >/dev/null; then
    echo "SKIP: foot is not installed"
    exit 0
fi

echo "== build and unit tests"
(cd "$ROOT/user/services/bitscomp" && cargo build --release -q && cargo test --release -q 2>&1 | grep -E "test result|FAILED")

COMP=""
cleanup() {
    [ -n "$COMP" ] && kill "$COMP" 2>/dev/null || true
}
trap cleanup EXIT

ctl() { "$BIN" ctl --socket "$SOCK" "$@"; }
fail() { echo "FAIL: $*"; ctl list || true; exit 1; }
wait_for() { # pattern, what
    for _ in $(seq 50); do
        ctl list 2>/dev/null | grep -q -- "$1" && return 0
        sleep 0.2
    done
    fail "$2"
}

echo "== start (headless 1280x800)"
"$BIN" --backend headless --size 1280x800 --socket "$SOCK" >"$RUN/bitscomp.log" 2>&1 &
COMP=$!
for _ in $(seq 25); do [ -S "$XDG_RUNTIME_DIR/$SOCK.ctl" ] && break; sleep 0.2; done

echo "== foot as first tile, German umlauts"
TYPED=$RUN/typed.txt
# The control socket splits commands at spaces, so the shell line goes into a script.
printf '#!/bin/sh\nexec cat > %s\n' "$TYPED" >"$RUN/catter.sh"
chmod +x "$RUN/catter.sh"
ctl spawn foot -T A "$RUN/catter.sh" >/dev/null
wait_for 'title="A"' "foot did not map"
sleep 0.5
ctl key 40 39 26 12 +42 40 -42 28 >/dev/null # ä ö ü ß Ä Enter on the German layout
sleep 0.5
[ "$(cat "$TYPED")" = "äöüßÄ" ] || fail "typed '$(cat "$TYPED")', expected äöüßÄ"
echo "typed: $(cat "$TYPED")"

echo "== second tile, stack, flip, float"
ctl spawn foot -T B sh >/dev/null
wait_for 'tile=1 .*title="B"' "second foot is not a second tile"
ctl stack >/dev/null
wait_for 'tile=0 page=1/2 visible=true focused=true.*title="B"' "B is not the shown page of a 2-page stack"
ctl flip >/dev/null
wait_for 'tile=0 page=0/2 visible=true.*title="A"' "flip did not turn to A"
ctl float >/dev/null
wait_for 'float=0 visible=true focused=true.*title="A"' "A does not float"
ctl list

if command -v wl-copy >/dev/null; then
    echo "== clipboard"
    WAYLAND_DISPLAY=$SOCK timeout 5 wl-copy "Grüße aus bitscomp"
    got=$(WAYLAND_DISPLAY=$SOCK timeout 5 wl-paste)
    [ "$got" = "Grüße aus bitscomp" ] || fail "clipboard gave '$got'"
    echo "clipboard: $got"
fi

echo "== screenshot and quit"
ctl shot "$RUN/shot.ppm" >/dev/null
head -c 15 "$RUN/shot.ppm" | grep -q "1280 800" || fail "screenshot header"
ctl stats
ctl quit >/dev/null
for _ in $(seq 15); do kill -0 "$COMP" 2>/dev/null || break; sleep 0.2; done
kill -0 "$COMP" 2>/dev/null && fail "bitscomp did not quit"
COMP=""
echo "PASS"
