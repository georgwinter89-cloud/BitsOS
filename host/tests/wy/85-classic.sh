#!/usr/bin/env bash
# WY-5c on the host (headless back end, nothing on the screen): floating windows as the classic alternative and
# the side bars that fold away (plan/spec/desktop.md §1, Georg 21.10.). A floating window opens centred over the
# tiles, lies above every bar (the Claude sidebar, the status bar), moves over the whole screen but keeps 48 px of
# its header on it, is maximised by a double click on its header, its button, Super+Up and Ctrl+Alt+M and comes
# back to its old place; a drag gives a maximised window its old size. The workspace list folds by its button,
# comes back by the top bar's button, Ctrl+Alt+W, and over the tiles by pushing the pointer against the edge for
# 300 ms (pinned by its button); Claude's sidebar by the ring, Ctrl+Alt+C and the right edge. Both folds are
# settings in a real settingsd and survive a restart; without settingsd they still work. References
# host/tests/wy/ref/classic-*.png (WY_UPDATE_REF=1 writes them).
set -euo pipefail

ROOT=${BITSOS_ROOT:-$(cd "$(dirname "$0")/../../.." && pwd)}
RUN=${BITSOS_RUN_DIR:-$(mktemp -d)}
mkdir -p "$RUN"
RUN=$(realpath "$RUN")
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-$ROOT/target}
BIN=$CARGO_TARGET_DIR/release/bitscomp
WYC=$CARGO_TARGET_DIR/release/wyclient
SD=$CARGO_TARGET_DIR/release/settingsd
REF=$ROOT/host/tests/wy/ref
IMG="python3 -I $ROOT/host/tests/wy/tools/img.py"
export XDG_RUNTIME_DIR=$RUN/xdg
mkdir -m 700 -p "$XDG_RUNTIME_DIR"
unset DISPLAY WAYLAND_DISPLAY
SOCK=wy85-$$
export SETTINGS_SOCKET=$RUN/settings.sock

fail() { echo "FAIL: $*"; exit 1; }

echo "== build bitscomp, wyclient, settingsd"
(cd "$ROOT/user/services/bitscomp" && cargo build --release -q --offline && cargo build --release -q --offline -p wyclient)
(cd "$ROOT/user" && cargo build --release -q --offline -p settingsd)

PIDS=()
cleanup() {
    for p in "${PIDS[@]}"; do kill "$p" 2>/dev/null || true; done
    for p in "${PIDS[@]}"; do wait "$p" 2>/dev/null || true; done
}
trap cleanup EXIT

ctl() { "$BIN" ctl --socket "$SOCK" "$@"; }
hot() { # pattern, field: "X Y" of that field on the first hot line matching the pattern
    local v
    v=$(ctl hot | grep -- "$1" | head -1 | sed -n "s/.* $2=\([-0-9.]*\),\([-0-9.]*\).*/\1 \2/p")
    [ -n "$v" ] || fail "no '$2' on a hot line matching '$1'"
    echo "$v"
}
frame() { # title: "x y w h" (logical) of that window
    ctl list | grep "title=\"$1\"" | sed -n 's/.* frame=\([-0-9]*\),\([-0-9]*\) \([0-9]*\)x\([0-9]*\) .*/\1 \2 \3 \4/p'
}
key() { "$BIN" ctl --socket "$SOCK" key "$@" >/dev/null; }
wait_for() { # pattern, command..., what
    local pat=$1 what=${!#}
    for _ in $(seq 60); do
        "${@:2:$#-2}" 2>/dev/null | grep -q -- "$pat" && return 0
        sleep 0.05
    done
    fail "$what"
}
compare() { # shot name
    if [ "${WY_UPDATE_REF:-0}" = 1 ]; then
        mkdir -p "$REF"
        $IMG png "$RUN/$1.ppm" "$REF/$1.png"
        echo "reference $1.png written"
    else
        $IMG diff "$RUN/$1.ppm" "$REF/$1.png" 8 0.0002 || fail "$1 differs from host/tests/wy/ref/$1.png"
    fi
}
shot() { sleep 0.2; ctl shot "$RUN/$1.ppm" >/dev/null; compare "$1"; }
start_comp() { # extra args...
    "$BIN" --backend headless --size 2000x1125 --socket "$SOCK" --mcp none --fixed-status "$@" >>"$RUN/bitscomp.log" 2>&1 &
    COMP=$!
    PIDS+=("$COMP")
    for _ in $(seq 50); do [ -S "$XDG_RUNTIME_DIR/$SOCK.ctl" ] && break; sleep 0.1; done
    wait_for "end" ctl list "no control socket"
}
stop_comp() { kill "$COMP"; wait "$COMP" 2>/dev/null || true; rm -f "$XDG_RUNTIME_DIR/$SOCK.ctl"; }

echo "== settingsd on the host"
mkdir -p "$RUN/stick" "$RUN/fallback" "$RUN/sys"
"$SD" serve --stick "$RUN/stick" --fallback "$RUN/fallback" --sys "$RUN/sys" --socket "$SETTINGS_SOCKET" --no-mcp \
    >"$RUN/settingsd.log" 2>&1 &
PIDS+=($!)
for _ in $(seq 50); do [ -S "$SETTINGS_SOCKET" ] && break; sleep 0.1; done
[ -S "$SETTINGS_SOCKET" ] || fail "settingsd did not start"
get() { "$SD" get "$1"; }

echo "== bitscomp 2000x1125 (1600x900 logical), two tiles, a third window floats"
start_comp --settings "$SETTINGS_SOCKET"
wait_for "settingsd=true" ctl theme "bitscomp did not read settingsd"
ctl theme | grep -q "workspaces=true" || fail "theme line lacks the workspace list: $(ctl theme)"
ctl spawn "$WYC" --title Links --pattern 0 >/dev/null
wait_for 'title="Links"' ctl list "first client did not map"
ctl spawn "$WYC" --title Rechts --pattern 1 >/dev/null
wait_for 'title="Rechts"' ctl list "second client did not map"
ctl spawn "$WYC" --title Schwebt --pattern 2 >/dev/null
wait_for 'tile=2 .*title="Schwebt"' ctl list "third client did not map"
ctl click $(hot 'title="Schwebt"' float) >/dev/null
wait_for 'float=0 visible=true focused=true.*title="Schwebt"' ctl list "Schwebt did not float"
read -r wx wy ww wh <<<"$(ctl hot | sed -n 's/^chrome .* work=\([0-9]*\),\([0-9]*\),\([0-9]*\),\([0-9]*\) .*/\1 \2 \3 \4/p')"
read -r fx fy fw fh <<<"$(frame Schwebt)"
echo "work area $wx,$wy ${ww}x$wh; float $fx,$fy ${fw}x$fh"
[ $(( fx + fw / 2 - wx - ww / 2 )) -le 1 ] && [ $(( wx + ww / 2 - fx - fw / 2 )) -le 1 ] || fail "not centred across the tiles"
[ $(( fy + fh / 2 - wy - wh / 2 )) -le 1 ] && [ $(( wy + wh / 2 - fy - fh / 2 )) -le 1 ] || fail "not centred down the tiles"
ctl hot | grep 'title="Schwebt"' | grep -q ' max=.* maximised=false' || fail "no maximise button on the floating header"

echo "== the float over Claude's sidebar and the status bar: drawn and hit above them"
read -r hx hy <<<"$(hot 'title="Schwebt"' header)"
ctl drag "$hx" "$hy" $(( hx + 700 )) $(( hy + 400 )) >/dev/null
read -r fx fy fw fh <<<"$(frame Schwebt)"
echo "moved to $fx,$fy ${fw}x$fh"
[ $(( fx + fw )) -gt 1300 ] && [ $(( fy + fh )) -gt 868 ] || fail "the float did not reach the sidebar and the status bar"
shot classic-float
# Focus a tile, then click the float where it lies over the sidebar (logical 1500,700 = 1875,875 on screen).
ctl click $(hot 'title="Links"' header) >/dev/null
wait_for 'focused=true.*title="Links"' ctl list "Links did not take the focus"
ctl click 1875 875 >/dev/null
wait_for 'float=0 visible=true focused=true.*title="Schwebt"' ctl list "a click on the float over the sidebar did not reach it"

echo "== moved over the whole screen, but 48 px of the header stay on it"
read -r hx hy <<<"$(hot 'title="Schwebt"' header)"
ctl drag "$hx" "$hy" 1999 2 >/dev/null
read -r fx fy fw fh <<<"$(frame Schwebt)"
echo "pushed right and up: $fx,$fy"
[ "$fx" -le $(( 1600 - 48 )) ] || fail "the header left the screen at the right: x=$fx"
[ "$fy" -ge 0 ] || fail "the header left the screen at the top: y=$fy"
ctl drag $(hot 'title="Schwebt"' header) 0 1124 >/dev/null
read -r fx fy fw fh <<<"$(frame Schwebt)"
echo "pushed left and down: $fx,$fy"
[ $(( fx + fw )) -ge 48 ] || fail "the header left the screen at the left: x=$fx w=$fw"
[ $(( fy + 38 )) -le 900 ] || fail "the header left the screen at the bottom: y=$fy"
ctl pointer 1000 500 >/dev/null
# A place to come back to.
ctl drag $(hot 'title="Schwebt"' header) 900 300 >/dev/null
old=$(frame Schwebt)
echo "old place: $old"

echo "== double click on the header maximises over every bar, again gives the old place back"
read -r hx hy <<<"$(hot 'title="Schwebt"' header)"
ctl click "$hx" "$hy" >/dev/null
ctl click "$hx" "$hy" >/dev/null
wait_for 'float=0 max visible=true focused=true frame=0,0 1600x900 .*title="Schwebt"' ctl list "a double click did not maximise"
shot classic-max
ctl hot | grep 'title="Schwebt"' | grep -q 'maximised=true' || fail "hot does not say maximised"
read -r hx hy <<<"$(hot 'title="Schwebt"' header)"
ctl click "$hx" "$hy" >/dev/null
ctl click "$hx" "$hy" >/dev/null
[ "$(frame Schwebt)" = "$old" ] || fail "a second double click did not restore: $(frame Schwebt) instead of $old"
ctl click "$hx" "$hy" >/dev/null
sleep 0.5
ctl click "$hx" "$hy" >/dev/null
[ "$(frame Schwebt)" = "$old" ] || fail "two slow clicks maximised"
echo "== a double click also works on a float that is not on top"
ctl click $(hot 'title="Rechts"' float) >/dev/null
wait_for 'float=1 visible=true focused=true.*title="Rechts"' ctl list "Rechts did not float"
ctl drag $(hot 'title="Rechts"' header) 300 900 >/dev/null
read -r hx hy <<<"$(hot 'title="Schwebt"' header)"
ctl click "$hx" "$hy" >/dev/null
ctl click "$hx" "$hy" >/dev/null
wait_for 'max visible=true focused=true frame=0,0 1600x900 .*title="Schwebt"' ctl list "a double click on the lower float did not maximise it"
ctl max >/dev/null
[ "$(frame Schwebt)" = "$old" ] || fail "ctl max did not restore"
ctl click $(hot 'title="Rechts"' float) >/dev/null
wait_for 'tile=1 .*title="Rechts"' ctl list "Rechts did not go back into a tile"

echo "== the header button, Super+Up and Ctrl+Alt+M"
ctl click $(hot 'title="Schwebt"' max) >/dev/null
wait_for 'float=0 max .*title="Schwebt"' ctl list "the maximise button did not maximise"
ctl click $(hot 'title="Schwebt"' max) >/dev/null
[ "$(frame Schwebt)" = "$old" ] || fail "the restore button did not restore"
key +125 103 -125
wait_for 'float=0 max .*title="Schwebt"' ctl list "Super+Up did not maximise"
key +125 103 -125
[ "$(frame Schwebt)" = "$old" ] || fail "Super+Up did not restore"
key +29 +56 50 -56 -29
wait_for 'float=0 max .*title="Schwebt"' ctl list "Ctrl+Alt+M did not maximise"

echo "== over a maximised float only the circle menu, the address suggestions (and its own veil, below) lie"
over() { # name, what: the shot differs from max-plain in more than 1 % of the screen
    sleep 0.2
    ctl shot "$RUN/$1.ppm" >/dev/null
    $IMG diff "$RUN/max-plain.ppm" "$RUN/$1.ppm" 8 0.01 && fail "$2 is not drawn over the maximised float"
    true
}
read -r hx hy <<<"$(hot 'title="Schwebt"' header)"
ctl pointer "$hx" "$hy" >/dev/null
sleep 0.2
ctl shot "$RUN/max-plain.ppm" >/dev/null
ctl click "$hx" "$hy" right >/dev/null
wait_for "^menu" ctl hot "a right click on the maximised header opened no circle menu"
over max-menu "the circle menu"
key 1
ctl hot | grep -q "^menu" && fail "Esc did not close the circle menu"
ctl address t >/dev/null
over max-suggest "the address suggestions"
key 1
ctl list | grep -q 'float=0 max .*title="Schwebt"' || fail "the menu or the address line changed the maximised float"

echo "== dragging a maximised window's header gives it its old size under the pointer"
ctl drag 1000 20 1000 400 >/dev/null
ctl list | grep 'title="Schwebt"' | grep -q ' max ' && fail "still maximised after a drag"
read -r ox oy ow oh <<<"$old"
read -r fx fy fw fh <<<"$(frame Schwebt)"
[ "$fw" = "$ow" ] && [ "$fh" = "$oh" ] || fail "not its old size after the drag: ${fw}x$fh"
[ "$fx" -le 800 ] && [ $(( fx + fw )) -ge 800 ] || fail "not under the pointer: x=$fx w=$fw"

echo "== a hung float over Claude's sidebar shows its veil over itself, over the bar"
WAYLAND_DISPLAY=$SOCK "$WYC" --title Haengt --pattern 1 --hang 1500 >"$RUN/haengt.log" 2>&1 &
HPID=$!
PIDS+=("$HPID")
wait_for 'title="Haengt"' ctl list "the hanging client did not map"
ctl click $(hot 'title="Haengt"' float) >/dev/null
wait_for 'float=.*focused=true.*title="Haengt"' ctl list "Haengt did not float"
ctl drag $(hot 'title="Haengt"' header) 1700 600 >/dev/null
for _ in $(seq 100); do ctl list | grep -q 'title="Haengt" pid=[0-9]* hung=true' && break; sleep 0.1; done
ctl list | grep -q 'title="Haengt" pid=[0-9]* hung=true' || fail "Haengt did not become hung within 10 s"
read -r fx fy fw fh <<<"$(frame Haengt)"
# A body point over the sidebar (logical, right of the tiles), in screen pixels (125 %).
right=$(( fx + fw )); [ "$right" -gt 1600 ] && right=1600
px=$(( right - 30 )); py=$(( fy + 80 ))
[ "$px" -gt $(( wx + ww )) ] && [ "$py" -lt $(( wy + wh )) ] || fail "Haengt's point $px,$py does not lie over the sidebar"
sx=$(( px * 5 / 4 )); sy=$(( py * 5 / 4 ))
sleep 0.3
ctl shot "$RUN/hung-float.ppm" >/dev/null
veil=$($IMG pixel "$RUN/hung-float.ppm" "$sx" "$sy")
read -r r g b <<<"$veil"
mx=$(printf '%s\n' "$r" "$g" "$b" | sort -n | tail -1); mn=$(printf '%s\n' "$r" "$g" "$b" | sort -n | head -1)
[ "$veil" != "48 160 64" ] && [ $(( mx - mn )) -le 40 ] || fail "no grey veil over the hung float at $sx,$sy: $veil"
ctl hot | grep -E '^win .*kill=.*title="Haengt"' >/dev/null || fail "no Beenden button on the hung float"
kill -9 "$HPID"
for _ in $(seq 60); do ctl list | grep -q 'title="Haengt"' || break; sleep 0.05; done
ctl list | grep -q 'title="Haengt"' && fail "Haengt's window stayed after its end"
sleep 0.2
ctl shot "$RUN/hung-gone.ppm" >/dev/null
bar=$($IMG pixel "$RUN/hung-gone.ppm" "$sx" "$sy")
echo "veil over the sidebar: $veil; the bar alone: $bar"
[ "$veil" != "$bar" ] || fail "the hung float's veil is drawn under the sidebar"

echo "== Ctrl+Alt+F: back into a tile and floating again, centred over the tiles"
key +29 +56 33 -56 -29
wait_for 'tile=2 .*title="Schwebt"' ctl list "Ctrl+Alt+F did not tile it"
key +29 +56 33 -56 -29
wait_for 'float=0 visible=true.*title="Schwebt"' ctl list "Ctrl+Alt+F did not float it"
read -r fx fy fw fh <<<"$(frame Schwebt)"
[ $(( fx + fw / 2 - wx - ww / 2 )) -le 1 ] && [ $(( wx + ww / 2 - fx - fw / 2 )) -le 1 ] || fail "floated again, not centred"
ctl drag $(hot 'title="Schwebt"' header) 1200 700 >/dev/null

echo "== the workspace list folds by its button and the tiles take the width"
ctl click $(hot '^chrome ' wsfold) >/dev/null
wait_for "bars workspaces=folded sidebar=docked" ctl bars "the list did not fold"
ctl hot | grep -q '^chrome .* work=0,' || fail "the tiles did not take the list's width: $(ctl hot | grep '^chrome')"
ctl hot | grep -q '^ws 0 ' && fail "a folded list still has entries"
wait_for false get theme.layout.workspaces_bar "the fold did not reach settingsd"
ctl click $(hot '^chrome ' wsbutton) >/dev/null
wait_for "bars workspaces=docked" ctl bars "the top bar's button did not bring the list back"
wait_for true get theme.layout.workspaces_bar "the list's return did not reach settingsd"

echo "== Ctrl+Alt+W and Ctrl+Alt+C fold both; the ring brings Claude back"
key +29 +56 17 -56 -29
key +29 +56 46 -56 -29
wait_for "bars workspaces=folded sidebar=folded" ctl bars "Ctrl+Alt+W / Ctrl+Alt+C did not fold"
ctl hot | grep -q '^chrome .* work=0,[0-9]*,1600,' || fail "the tiles do not take the whole width"
shot classic-folded
ctl click $(hot '^chrome ' ring) >/dev/null
wait_for "sidebar=docked" ctl bars "the ring did not bring the sidebar back"
key +29 +56 46 -56 -29
wait_for "sidebar=folded" ctl bars "Ctrl+Alt+C did not fold again"

echo "== the edge: 300 ms against it reveals the list over the tiles, leaving folds it again"
ctl pointer 1000 600 >/dev/null
ctl pointer 0 600 >/dev/null
t0=$(date +%s%N)
early=$(ctl bars)
for _ in $(seq 40); do ctl bars | grep -q "workspaces=revealed" && break; sleep 0.05; done
ms=$(( ($(date +%s%N) - t0) / 1000000 ))
echo "before: $early; revealed after about $ms ms"
echo "$early" | grep -q "workspaces=folded" || fail "revealed at once, not after 300 ms"
ctl bars | grep -q "workspaces=revealed" || fail "the left edge did not reveal the list"
ctl hot | grep -q '^chrome .* work=0,' || fail "the revealed list took width from the tiles"
ctl pointer 100 600 >/dev/null
ctl bars | grep -q "workspaces=revealed" || fail "the list folded while the pointer was on it"
shot classic-reveal
ctl pointer 1000 600 >/dev/null
wait_for "workspaces=folded" ctl bars "leaving the list did not fold it"
ctl pointer 0 600 >/dev/null
ctl pointer 1000 600 >/dev/null
sleep 0.5
ctl bars | grep -q "workspaces=folded" || fail "a short touch of the edge revealed the list"
echo "== pinned by its button"
ctl pointer 0 600 >/dev/null
wait_for "workspaces=revealed" ctl bars "the edge did not reveal the list again"
ctl click $(hot '^chrome ' wsfold) >/dev/null
wait_for "workspaces=docked" ctl bars "the list's button did not pin it"
wait_for true get theme.layout.workspaces_bar "the pin did not reach settingsd"
echo "== the right edge reveals Claude's sidebar"
ctl pointer 1999 600 >/dev/null
wait_for "sidebar=revealed" ctl bars "the right edge did not reveal the sidebar"
ctl pointer 1000 600 >/dev/null
wait_for "sidebar=folded" ctl bars "leaving the sidebar did not fold it"

echo "== a restart keeps both folds (settings)"
key +29 +56 17 -56 -29
wait_for "bars workspaces=folded sidebar=folded" ctl bars "Ctrl+Alt+W did not fold"
wait_for false get theme.layout.workspaces_bar "the fold did not reach settingsd"
wait_for false get theme.layout.claude_sidebar "the sidebar's fold did not reach settingsd"
stop_comp
start_comp --settings "$SETTINGS_SOCKET"
wait_for "settingsd=true" ctl theme "bitscomp did not read settingsd after the restart"
wait_for "bars workspaces=folded sidebar=folded" ctl bars "the folds did not survive the restart"
ctl address open einstellungen/aufbau/arbeitsraumliste >/dev/null
wait_for 'page pid=.*switch=theme.layout.workspaces_bar' ctl hot "the settings page has no switch for the workspace list"
ctl hot | grep -q "page pid=.*switch=theme.layout.claude_sidebar" || fail "the settings page has no switch for the sidebar"
ctl click $(hot 'switch=theme.layout.workspaces_bar' at) >/dev/null
wait_for "bars workspaces=docked" ctl bars "the page's switch did not bring the list back"
stop_comp

echo "== without settingsd the folds work, local only"
start_comp --settings none
ctl workspaces | grep -q "workspaces folded" || fail "ctl workspaces did not fold without settingsd"
ctl sidebar | grep -q "sidebar folded" || fail "ctl sidebar did not fold without settingsd"
ctl reveal left | grep -q "workspaces=revealed" || fail "ctl reveal left"
ctl reveal none | grep -q "workspaces=folded" || fail "ctl reveal none"
grep -q "local only" "$RUN/bitscomp.log" || fail "the log does not say the fold stayed local"
stop_comp
echo "PASS: floating windows above every bar, 48 px reach, maximise and restore (double click, button, Super+Up, Ctrl+Alt+M, drag), folding bars (buttons, keys, edge reveal, pin, settings, restart)"
