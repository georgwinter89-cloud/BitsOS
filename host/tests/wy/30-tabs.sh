#!/usr/bin/env bash
# WY-0c: tabs like a browser's in the window header (plan/spec/desktop.md §1), headless, driven over the control
# socket: titles on the tabs (umlauts drawn as their own glyphs), tab click shows and focuses, a tab's x closes
# exactly that window, reorder inside the strip, tab drag out of a stack, "+N" chip with many windows, wheel and
# keys still flip, middle click changes nothing, a floating window shows its tab. Opens no window.
set -euo pipefail

ROOT=${BITSOS_ROOT:-$(cd "$(dirname "$0")/../../.." && pwd)}
RUN=${BITSOS_RUN_DIR:-$(mktemp -d)}
mkdir -p "$RUN"
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-$ROOT/target}
BIN=$CARGO_TARGET_DIR/release/bitscomp
SOCK=wayland-wy-tabs-$$
export XDG_RUNTIME_DIR=${XDG_RUNTIME_DIR:-$RUN}
unset DISPLAY

if ! command -v foot >/dev/null; then
    echo "SKIP: foot is not installed"
    exit 0
fi

echo "== build"
(cd "$ROOT/user/services/bitscomp" && cargo build --release -q)

COMP=""
cleanup() {
    [ -n "$COMP" ] && kill "$COMP" 2>/dev/null || true
}
trap cleanup EXIT

ctl() { "$BIN" ctl --socket "$SOCK" "$@"; }
fail() { echo "FAIL: $*"; ctl list || true; ctl hot || true; exit 1; }
wait_for() { # pattern, what
    for _ in $(seq 50); do
        ctl list 2>/dev/null | grep -q -- "$1" && return 0
        sleep 0.1
    done
    fail "$2"
}
wait_gone() { # pattern, what
    for _ in $(seq 50); do
        ctl list 2>/dev/null | grep -q -- "$1" || return 0
        sleep 0.1
    done
    fail "$2"
}
# hot LINE-PATTERN KEY -> "X Y" (physical pixels)
hot() {
    local v
    v=$(ctl hot | grep -- "$1" | head -1 | sed -n "s/.* $2=\([-0-9.]*\),\([-0-9.]*\).*/\1 \2/p")
    [ -n "$v" ] || fail "no '$2' on a hot line matching '$1'"
    echo "$v"
}
field() { ctl hot | grep -- "$1" | head -1 | sed -n "s/.* $2=\([^ ]*\).*/\1/p"; }
term() { # title
    ctl spawn foot -T "$1" sh >/dev/null
    wait_for "visible=true focused=true.*title=\"$1\"" "foot '$1' did not map"
}

echo "== start (headless 1600x1000); tabs show the titles"
"$BIN" --backend headless --size 1600x1000 --socket "$SOCK" >"$RUN/bitscomp-tabs.log" 2>&1 &
COMP=$!
for _ in $(seq 25); do [ -S "$XDG_RUNTIME_DIR/$SOCK.ctl" ] && break; sleep 0.2; done
# WY-5: Claude's sidebar folded, so the tiles of 1600x1000 at 125 % leave the tabs room for their titles.
ctl sidebar | grep -q folded || fail "the sidebar did not fold"
term 'Grüße'
term 'Gru?e'
term A
[ "$(ctl hot | grep -c '^tab tile=0 ')" -eq 1 ] || fail "a single window does not show exactly one tab"
ctl hot | grep -q '^tab tile=0 page=0/1 shown=true .*shows="Grüße" title="Grüße"' || fail "no tab 'Grüße'"
ctl hot | grep -q '^tab tile=2 .*shows="A"' || fail "no tab 'A'"

echo "== umlauts are drawn as their own glyphs (screenshot glyph boxes: G r ü ß e against G r u ? e)"
ctl shot "$RUN/tabs-umlaut.ppm" >/dev/null
python3 "$ROOT/host/tests/wy/tools/cells.py" "$RUN/tabs-umlaut.ppm" \
    "$(field '^tab tile=0 ' text)" "$(field '^tab tile=0 ' glyphs)" "$(field '^tab tile=1 ' text)" \
    "$(field '^tab tile=1 ' glyphs)" "$(field '^tab tile=0 ' cell)" \
    0=same 1=same 2=differ 3=differ 4=same 2=ink 3=ink || fail "umlaut glyphs"

echo "== a stack shows one tab per window; click a tab shows and focuses it"
term B
ctl stack >/dev/null
wait_for 'tile=2 page=1/2 visible=true focused=true.*title="B"' "B is not on A's stack"
[ "$(ctl hot | grep -c '^tab tile=2 ')" -eq 2 ] || fail "the stack does not show two tabs"
ctl click $(hot '^tab tile=2 .*title="A"' at) >/dev/null
wait_for 'tile=2 page=0/2 visible=true focused=true.*title="A"' "click on A's tab did not show and focus A"
ctl hot | grep -q '^tab tile=2 page=0/2 shown=true .*title="A"' || fail "A's tab is not the shown one"

echo "== a tab's x closes exactly that window (B is hidden), the shown one stays"
term C
ctl stack >/dev/null
wait_for 'tile=2 page=2/3 visible=true.*title="C"' "C is not on the stack"
idb=$(field '^tab tile=2 .*title="B"' id)
ctl click $(hot '^tab tile=2 .*title="B"' close) >/dev/null
wait_gone "id=$idb " "B's tab x did not close B"
wait_for 'tile=2 page=1/2 visible=true focused=true.*title="C"' "closing B changed the shown page"
ctl list | grep -q 'title="A"' || fail "A went too"

echo "== drag a tab inside its strip: C before A"
read -r ax ay <<<"$(hot '^tab tile=2 .*title="A"' at)"
lx=$(field '^tab tile=2 .*title="A"' rect | cut -d, -f1)
ctl drag $(hot '^tab tile=2 .*title="C"' at) $((lx + 4)) "$ay" >/dev/null
wait_for 'tile=2 page=0/2 visible=true focused=true.*title="C"' "C did not move before A"
wait_for 'tile=2 page=1/2 visible=false.*title="A"' "A is not behind C"

echo "== drag A's tab out of the stack onto a gap: own tile"
ctl drag $(hot '^tab tile=2 .*title="A"' at) $(hot 'gap=0' at) >/dev/null
wait_for 'tile=1 page=0/1 visible=true focused=true.*title="A"' "A did not get its own tile"
wait_for 'tile=3 page=0/1 visible=true.*title="C"' "C is not alone"

echo "== many windows: tabs shrink, a long title is cut, the rest is on a \"+N\" chip"
term Langer_Fenstertitel
ctl stack >/dev/null
term E
ctl stack >/dev/null
wait_for 'tile=3 page=2/3 visible=true focused=true.*title="E"' "E is not on C's stack"
n=$(field '^more tile=3 ' count)
[ -n "$n" ] && [ "$n" -ge 1 ] || fail "no +N chip on a narrow stack of three"
# WY-5: Exo 2 tabs need more room, so on this narrow stack only the shown page has a tab: show the long one.
ctl flip -1 >/dev/null
wait_for 'tile=3 page=1/3 visible=true focused=true.*title="Langer_Fenstertitel"' "no flip to the long title"
ctl hot | grep -q '^tab tile=3 .*shows="[^"]*…" title="Langer_Fenstertitel"' || fail "the long title is not cut"
ctl pointer $(hot '^tab tile=3 .*title="Langer_Fenstertitel"' close) >/dev/null
sleep 0.3
ctl shot "$RUN/tabs-more.ppm" >/dev/null
ctl flip 1 >/dev/null
wait_for 'tile=3 page=2/3 visible=true focused=true.*title="E"' "no flip back to E"
ctl click $(hot '^more tile=3 ' at) >/dev/null
wait_for 'tile=3 page=0/3 visible=true focused=true.*title="C"' "the chip did not flip forward"

echo "== wheel on the header and Strg+Alt+Bild ab still flip"
ctl pointer $(hot '^win tile=3 ' header) >/dev/null
ctl wheel 1 >/dev/null
wait_for 'tile=3 page=1/3 visible=true.*title="Langer_Fenstertitel"' "the wheel did not flip"
ctl key +29 +56 109 -56 -29 >/dev/null
wait_for 'tile=3 page=2/3 visible=true.*title="E"' "Strg+Alt+Bild ab did not flip"

echo "== middle click on a tab changes nothing"
before=$(ctl list)
ctl click $(hot '^tab tile=3 .*title="C"' at) middle >/dev/null
ctl click $(hot '^tab tile=3 .*title="E"' close) middle >/dev/null
sleep 0.3
[ "$(ctl list)" = "$before" ] || fail "middle click on a tab changed something"

echo "== a floating window shows its title as a tab"
ctl click $(hot '^win tile=0 ' float) >/dev/null
wait_for 'float=0 visible=true focused=true.*title="Grüße"' "Grüße did not float"
ctl hot | grep -q '^tab float=0 page=0/1 shown=true .*title="Grüße"' || fail "the floating window has no tab"
ctl shot "$RUN/tabs-shot.ppm" >/dev/null
ctl list

ctl quit >/dev/null || true
for _ in $(seq 25); do kill -0 "$COMP" 2>/dev/null || break; sleep 0.2; done
kill -0 "$COMP" 2>/dev/null && fail "bitscomp did not quit"
COMP=""
echo "PASS"
