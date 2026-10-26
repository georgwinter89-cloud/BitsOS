#!/usr/bin/env bash
# WY-0b: every window action by mouse (plan/spec/desktop.md §1 "Everything by mouse too"), headless, driven
# over the control socket like a hand would: click focus, header float and close buttons, float resize, drag to
# stack, tab click, drag out of a stack, drag beside, gap drag, circle menu (empty space and window, by
# click and by release, Esc and outside click), middle click untouched, a key still working. Opens no window.
set -euo pipefail

ROOT=${BITSOS_ROOT:-$(cd "$(dirname "$0")/../../.." && pwd)}
RUN=${BITSOS_RUN_DIR:-$(mktemp -d)}
mkdir -p "$RUN"
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-$ROOT/target}
BIN=$CARGO_TARGET_DIR/release/bitscomp
SOCK=wayland-wy-mouse-$$
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
count() { ctl list | grep -c '^tile\|^float' || true; }
# hot LINE-PATTERN KEY -> "X Y" (physical pixels) of that clickable thing
hot() {
    local v
    v=$(ctl hot | grep -- "$1" | head -1 | sed -n "s/.* $2=\([-0-9.]*\),\([-0-9.]*\).*/\1 \2/p")
    [ -n "$v" ] || fail "no '$2' on a hot line matching '$1'"
    echo "$v"
}
width() { ctl list | grep -- "$1" | head -1 | sed -n 's/.* \([0-9]*\)x[0-9]* .*/\1/p'; }
height() { ctl list | grep -- "$1" | head -1 | sed -n 's/.* [0-9]*x\([0-9]*\) .*/\1/p'; }

echo "== start (headless 1280x800) with three terminals A B C"
"$BIN" --backend headless --size 1600x1000 --socket "$SOCK" >"$RUN/bitscomp-mouse.log" 2>&1 &
COMP=$!
for _ in $(seq 25); do [ -S "$XDG_RUNTIME_DIR/$SOCK.ctl" ] && break; sleep 0.2; done
# WY-5: the work area lies inside the chrome; Claude's sidebar folded leaves the stacked tabs room for two tabs.
ctl sidebar | grep -q folded || fail "the sidebar did not fold"
for t in A B C; do
    ctl spawn foot -T $t sh >/dev/null
    wait_for "visible=true.*title=\"$t\"" "foot $t did not map"
done

echo "== click focuses (content, header)"
ctl click $(hot 'title="A"' content) >/dev/null
wait_for 'focused=true.*title="A"' "click into A did not focus it"
before=$(ctl list | sed 's/focused=[a-z]*//')
ctl click $(hot 'title="B"' header) >/dev/null
wait_for 'focused=true.*title="B"' "click on B's header did not focus it"
[ "$(ctl list | sed 's/focused=[a-z]*//')" = "$before" ] || fail "a short header click moved something"

echo "== header float button, float resize at the corner, back to a tile"
ctl click $(hot 'title="B"' float) >/dev/null
wait_for 'float=0 visible=true focused=true.*title="B"' "float button did not float B"
w0=$(width 'title="B"'); h0=$(height 'title="B"')
read -r cx cy <<<"$(hot 'title="B"' corner)"
ctl drag "$cx" "$cy" $((${cx%.*} + 125)) $((${cy%.*} + 75)) >/dev/null
w1=$(width 'title="B"'); h1=$(height 'title="B"')
[ "$w1" -eq $((w0 + 100)) ] && [ "$h1" -eq $((h0 + 60)) ] || fail "corner drag gave ${w1}x${h1} from ${w0}x${h0}"
echo "resized ${w0}x${h0} -> ${w1}x${h1}"
ctl click $(hot 'title="B"' float) >/dev/null
wait_for 'tile=2 page=0/1 .*title="B"' "float button on floating B did not tile it again"

echo "== drag A's header onto C's centre: stack"
read -r ax ay <<<"$(hot 'title="A"' header)"
ctl drag "$ax" "$ay" $(hot 'title="C"' content) >/dev/null
wait_for 'tile=0 page=1/2 visible=true focused=true.*title="A"' "A is not on C's stack"

echo "== tab click"
ctl click $(hot '^tab tile=0 .*title="C"' at) >/dev/null
wait_for 'tile=0 page=0/2 visible=true focused=true.*title="C"' "click on C's tab did not show C"

echo "== drag C out of the stack onto the gap (empty space): own tile"
ctl drag $(hot 'title="C"' header) $(hot 'gap=0' at) >/dev/null
wait_for 'tile=1 page=0/1 visible=true focused=true.*title="C"' "C did not get its own tile"
wait_for 'tile=0 page=0/1 visible=true.*title="A"' "A is not alone in tile 0"

echo "== drag B onto A's left edge: beside"
read -r fx fy <<<"$(hot 'title="A"' content)"
lx=$(ctl list | grep 'title="A"' | sed -n 's/.* frame=\([0-9]*\),.*/\1/p')
ctl drag $(hot 'title="B"' header) $(((lx + 20) * 5 / 4)) "$fy" >/dev/null
wait_for 'tile=0 page=0/1 .*title="B"' "B is not left of A"
wait_for 'tile=1 page=0/1 .*title="A"' "A did not move right"

echo "== gap drag changes the split"
wa=$(width 'title="B"'); wb=$(width 'title="A"')
read -r gx gy <<<"$(hot 'gap=0' at)"
ctl drag "$gx" "$gy" $((${gx%.*} + 150)) "$gy" >/dev/null
wa2=$(width 'title="B"'); wb2=$(width 'title="A"')
d=$((wa2 + wb2 - wa - wb))
[ "$wa2" -gt "$wa" ] && [ "$wb2" -lt "$wb" ] && [ "${d#-}" -le 1 ] ||
    fail "gap drag gave widths $wa2/$wb2 from $wa/$wb"
echo "widths $wa/$wb -> $wa2/$wb2"

echo "== middle click is not bitscomp's"
before=$(ctl list)
ctl click $(hot 'title="C"' header) middle >/dev/null
ctl hot | grep -q '^menu' && fail "middle click opened the menu"
[ "$(ctl list)" = "$before" ] || fail "middle click changed the layout"

echo "== circle menu on empty space: Esc, outside click, Neues Terminal"
ctl click $(hot '^empty' at) right >/dev/null
ctl hot | grep -q '^menu' || fail "right click on empty space opened no menu"
ctl key 1 >/dev/null
ctl hot | grep -q '^menu' && fail "Esc did not close the menu"
ctl click $(hot '^empty' at) right >/dev/null
ctl click 5 795 >/dev/null
ctl hot | grep -q '^menu' && fail "a click outside did not close the menu"
n=$(count)
ctl click $(hot '^empty' at) right >/dev/null
ctl click $(hot 'label="Neues Terminal"' at) >/dev/null
ctl hot | grep -q '^menu' && fail "the menu stayed open after Neues Terminal"
for _ in $(seq 50); do [ "$(count)" -gt "$n" ] && break; sleep 0.1; done
[ "$(count)" -gt "$n" ] || fail "Neues Terminal opened no window"
wait_for 'focused=true.*app_id=foot' "the new terminal is not focused"

echo "== circle menu on a window: Breiter by click, Schweben, Kachel, Schließen by release"
wc0=$(width 'title="C"')
ctl click $(hot 'title="C"' header) right >/dev/null
ctl click $(hot 'label="Breiter"' at) >/dev/null
[ "$(width 'title="C"')" -gt "$wc0" ] || fail "Breiter did not widen C"
ctl click $(hot 'title="C"' header) right >/dev/null
ctl click $(hot 'label="Schweben"' at) >/dev/null
wait_for 'float=0 .*title="C"' "Schweben did not float C"
ctl click $(hot 'title="C"' header) right >/dev/null
ctl click $(hot 'label="Kachel"' at) >/dev/null
wait_for 'tile=.*title="C"' "Kachel did not tile C"
n=$(count)
# The new terminal's header (its title is whatever its shell sets).
head=$(ctl hot | grep '^win' | grep -v -E 'title="[ABC]"' | sed -n 's/.* header=\([-0-9.]*\),\([-0-9.]*\).*/\1 \2/p')
[ -n "$head" ] || fail "no header of the new terminal"
ctl press right $head >/dev/null
ctl pointer $(hot 'label="Schließen"' at) >/dev/null
ctl release right >/dev/null
for _ in $(seq 50); do [ "$(count)" -lt "$n" ] && break; sleep 0.1; done
[ "$(count)" -lt "$n" ] || fail "Schließen (released on it) did not close the window"

echo "== keys still work (Strg+Alt+F floats)"
ctl click $(hot 'title="A"' content) >/dev/null
ctl key +29 +56 33 -56 -29 >/dev/null
wait_for 'float=0 .*title="A"' "Strg+Alt+F did not float A"

echo "== header close button"
ctl click $(hot 'title="A"' close) >/dev/null
wait_gone 'title="A"' "close button did not close A"
ctl list

echo "== screenshot, then quit through the menu"
ctl shot "$RUN/mouse-shot.ppm" >/dev/null
ctl click $(hot '^empty' at) right >/dev/null
ctl click $(hot 'label="bitscomp beenden"' at) >/dev/null || true
for _ in $(seq 25); do kill -0 "$COMP" 2>/dev/null || break; sleep 0.2; done
kill -0 "$COMP" 2>/dev/null && fail "bitscomp beenden did not end bitscomp"
COMP=""
echo "PASS"
