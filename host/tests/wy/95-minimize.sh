#!/usr/bin/env bash
# WY-12 on the host (headless back end, nothing on the screen): a client's own minimise (xdg set_minimized, as
# Brave's minimise button sends it). wyclient behaves like Chromium: after asking it draws nothing until a
# configure with `activated` comes, then draws in its next colour. The minimised window is hidden (not drawn,
# clicks and keys go to the others), its tile is freed, the other window gets the focus; Alt+Tab restores it with
# a configure `activated` and its next frame is on the screen; the window list (MINIMIERT) and its launcher tile
# (favourites bar) restore too; a minimised float comes back with its frame; a fullscreen window minimised leaves
# its fullscreen; the last window minimised shows the start page, and Alt+Tab brings it back. Fixer: an unfocused
# window minimised keeps the focus where it is, a stack page returns to its stack, fullscreen asked while minimised
# is refused, set_minimized before the first commit is ignored, a hung client gets no configure (B4), and a
# minimised window survives a session restart as a tile.
set -euo pipefail

ROOT=${BITSOS_ROOT:-$(cd "$(dirname "$0")/../../.." && pwd)}
RUN=${BITSOS_RUN_DIR:-$(mktemp -d)}
mkdir -p "$RUN"
RUN=$(realpath "$RUN")
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-$ROOT/target}
BIN=$CARGO_TARGET_DIR/release/bitscomp
WYC=$CARGO_TARGET_DIR/release/wyclient
IMG="python3 -I $ROOT/host/tests/wy/tools/img.py"
export XDG_RUNTIME_DIR=$RUN/xdg
mkdir -m 700 -p "$XDG_RUNTIME_DIR"
unset DISPLAY WAYLAND_DISPLAY
SOCK=wy95-$$

fail() { echo "FAIL: $*"; exit 1; }

echo "== build bitscomp, wyclient"
(cd "$ROOT/user/services/bitscomp" && cargo build --release -q --offline && cargo build --release -q --offline -p wyclient)

PIDS=()
cleanup() {
    for p in "${PIDS[@]}"; do kill "$p" 2>/dev/null || true; done
    for p in "${PIDS[@]}"; do wait "$p" 2>/dev/null || true; done
}
trap cleanup EXIT

ctl() { "$BIN" ctl --socket "$SOCK" "$@"; }
key() { ctl key "$@" >/dev/null; }
alt_tab() { key +56 15 -56; }
hot() { # pattern, field: "X Y" of that field on the first hot line matching the pattern
    local v
    v=$(ctl hot | grep -- "$1" | head -1 | sed -n "s/.* $2=\([-0-9.]*\),\([-0-9.]*\).*/\1 \2/p")
    [ -n "$v" ] || fail "no '$2' on a hot line matching '$1'"
    echo "$v"
}
wait_for() { # pattern, command..., what
    local pat=$1 what=${!#}
    for _ in $(seq 60); do
        "${@:2:$#-2}" 2>/dev/null | grep -q -- "$pat" && return 0
        sleep 0.05
    done
    fail "$what"
}
last_states() { grep '^states' "$RUN/$1.out" | tail -1; }
wait_states() { # client, states pattern, what
    for _ in $(seq 60); do last_states "$1" | grep -q -- "$2" && return 0; sleep 0.05; done
    fail "$3 (last: $(last_states "$1"))"
}
wait_line() { # client, line pattern, count before, what: a new matching line after `before` of them
    for _ in $(seq 60); do [ "$(grep -c -- "$2" "$RUN/$1.out" || true)" -gt "$3" ] && return 0; sleep 0.05; done
    fail "$4"
}
count() { grep -c -- "$2" "$RUN/$1.out" || true; }
client() { # title pattern [args...]: a wyclient whose lines go to $RUN/<title>.out
    local t=$1 p=$2
    shift 2
    WAYLAND_DISPLAY=$SOCK "$WYC" --title "$t" --pattern "$p" "$@" >"$RUN/$t.out" 2>&1 &
    PIDS+=($!)
    wait_for "title=\"$t\"" ctl list "$t did not map"
}
id_of() { ctl list | grep "title=\"$1\"" | sed -n 's/.* id=\([0-9]*\) .*/\1/p'; }
spot() { # title: a point inside the client's part beside its white square (screen pixels)
    local b
    b=$(ctl hot | grep "^win .*title=\"$1\"" | head -1 | sed -n 's/.* body=\([0-9]*\),\([0-9]*\),.*/\1 \2/p')
    [ -n "$b" ] || fail "no body for $1"
    set -- $b
    echo "$(($1 + 30)) $(($2 + 30))"
}
pixel() { ctl shot "$RUN/p.ppm" >/dev/null; $IMG pixel "$RUN/p.ppm" "$1" "$2"; }
wait_pixel() { # x y "r g b" what
    local c
    for _ in $(seq 40); do
        c=$(pixel "$1" "$2")
        [ "$c" = "$3" ] && return 0
        sleep 0.05
    done
    fail "$4 (pixel $1,$2 is '$c', not '$3')"
}
RED="192 48 48"; GREEN="48 160 64"; BLUE="48 80 192"

echo "== bitscomp 2000x1125 (1600x900 logical), two tiles"
start_comp() { # log name, extra args... (--fixed-status: no session is read or written)
    local log=$1
    shift
    "$BIN" --backend headless --size 2000x1125 --socket "$SOCK" --mcp none --settings none "$@" >"$RUN/$log.log" 2>&1 &
    COMP=$!
    PIDS+=($COMP)
    for _ in $(seq 50); do [ -S "$XDG_RUNTIME_DIR/$SOCK.ctl" ] && break; sleep 0.1; done
    wait_for "end" ctl list "no control socket"
    # DS-3: this scenario is about the classic chrome's favourites bar and list (the Bühne has neither).
    ctl set theme.layout.preset classic >/dev/null
}
start_comp bitscomp --fixed-status
client Links 0
client Rechts 1
wait_for 'tile=1 page=0/1 visible=true focused=true.*title="Rechts"' ctl list "Rechts is not focused"
wait_states Rechts "activated=1" "Rechts was never activated"
read -r rx ry <<<"$(spot Rechts)"
wait_pixel "$rx" "$ry" "$GREEN" "Rechts is not on the screen before"

echo "== Rechts minimises itself (F8 = set_minimized, as Brave's button): hidden, its tile freed, Links focused"
key 66
grep -q "^asked minimize" "$RUN/Rechts.out" || fail "F8 did not reach Rechts"
wait_for 'minimized=0 visible=false focused=false.*title="Rechts"' ctl list "Rechts is not listed as minimised"
wait_for 'tile=0 page=0/1 visible=true focused=true.*title="Links"' ctl list "Links did not get the focus"
ctl list | grep -q "^tile=1" && fail "the minimised window's tile was not freed: $(ctl list)"
wait_states Rechts "activated=0" "Rechts was not told it lost activated"
wait_states Links "activated=1" "Links was not activated"
grep -q "window $(id_of Rechts) minimised (its own request)" "$RUN/bitscomp.log" || fail "the log does not say Rechts was minimised"
wait_pixel "$rx" "$ry" "$RED" "where Rechts was, Links (grown over the freed tile) is not shown"

echo "== no input to the hidden window: keys and clicks go to Links"
kr=$(count Rechts "^key 30 pressed"); kl=$(count Links "^key 30 pressed")
key 30
wait_line Links "^key 30 pressed" "$kl" "a key did not reach Links"
[ "$(count Rechts "^key 30 pressed")" = "$kr" ] || fail "a key reached the minimised window"
bl=$(count Links "^button 272 pressed"); br=$(count Rechts "^button 272 pressed")
ctl click "$rx" "$ry" >/dev/null
wait_line Links "^button 272 pressed" "$bl" "a click where Rechts was did not reach Links"
[ "$(count Rechts "^button 272 pressed")" = "$br" ] || fail "a click reached the minimised window"
ctl hot | grep -q "^win .*title=\"Rechts\"" && fail "the minimised window still has a hot spot"
ctl hot | grep -q "^minimized 0 at=.*title=\"Rechts\"" || fail "the window list does not show Rechts"

echo "== Alt+Tab restores it: a configure with activated, its next frame on the screen, Links deactivated"
alt_tab
wait_for 'tile=1 page=0/1 visible=true focused=true.*title="Rechts"' ctl list "Alt+Tab did not restore Rechts into its tile"
wait_states Rechts "activated=1" "the restored Rechts got no configure with activated"
wait_line Rechts "^resumed colour=3050c0" 0 "Rechts did not draw again"
wait_states Links "activated=0" "Links is still activated"
read -r rx ry <<<"$(spot Rechts)"
wait_pixel "$rx" "$ry" "$BLUE" "the restored client's new frame is not on the screen"
ctl list | grep -q "^minimized" && fail "Rechts is still listed as minimised"
alt_tab
wait_for 'focused=true.*title="Links"' ctl list "Alt+Tab did not go on to Links"

echo "== the window list restores (click on its MINIMIERT entry)"
ctl click $(hot 'title="Rechts"' header) >/dev/null
wait_for 'focused=true.*title="Rechts"' ctl list "Rechts did not take the focus"
key 66
wait_for 'minimized=0 .*title="Rechts"' ctl list "Rechts did not minimise a second time"
ctl click $(hot '^minimized 0 ' at) >/dev/null
wait_for 'visible=true focused=true.*title="Rechts"' ctl list "the window list did not restore Rechts"
wait_states Rechts "activated=1" "restored from the list without activated"
wait_line Rechts "^resumed" 1 "Rechts did not draw again after the list"

echo "== its launcher tile (favourites bar) restores it instead of starting another"
ctl set desktop.favourites programm/wyclient >/dev/null
wait_for "^fav 0 at=.*addr=programm/wyclient" ctl hot "the favourite is not in the bar"
ctl minimize "$(id_of Rechts)" >/dev/null
wait_for 'minimized=0 .*title="Rechts"' ctl list "ctl minimize did not minimise Rechts"
n=$(ctl list | grep -c "id=")
ctl click $(hot "^fav 0 " at) >/dev/null
wait_for 'visible=true focused=true.*title="Rechts"' ctl list "the launcher tile did not restore Rechts"
[ "$(ctl list | grep -c "id=")" = "$n" ] || fail "the launcher tile opened another window"
grep -q "restored (launcher tile)" "$RUN/bitscomp.log" || fail "the log does not say the launcher tile restored it"

echo "== a minimised float comes back floating with its frame"
ctl float >/dev/null
wait_for 'float=0 visible=true focused=true.*title="Rechts"' ctl list "Rechts did not float"
before=$(ctl list | grep 'title="Rechts"' | sed 's/.*\(frame=[^ ]* [^ ]*\).*/\1/')
key 66
wait_for 'minimized=0 .*title="Rechts"' ctl list "the float did not minimise"
alt_tab
wait_for 'float=0 visible=true focused=true.*title="Rechts"' ctl list "the float did not come back floating"
[ "$(ctl list | grep 'title="Rechts"' | sed 's/.*\(frame=[^ ]* [^ ]*\).*/\1/')" = "$before" ] || fail "the float's frame changed: $(ctl list)"
ctl float >/dev/null
wait_for 'tile=1 page=0/1 visible=true focused=true.*title="Rechts"' ctl list "Rechts is not a tile again"

echo "== a fullscreen window minimised leaves its fullscreen; restored, it is a tile again"
key 87
wait_states Rechts "fullscreen=1" "Rechts got no fullscreen"
key 66
wait_for 'minimized=0 .*title="Rechts"' ctl list "the fullscreen window did not minimise"
wait_states Rechts "fullscreen=0 maximized=0 activated=0" "the minimised window was not told its fullscreen ended"
ctl list | grep -q " fullscreen " && fail "a fullscreen is left behind"
wait_for 'tile=0 page=0/1 visible=true focused=true.*title="Links"' ctl list "Links is not shown under it"
alt_tab
wait_states Rechts "fullscreen=0 maximized=0 activated=1" "restored fullscreen or not activated"
wait_for 'tile=1 page=0/1 visible=true focused=true.*title="Rechts"' ctl list "Rechts is not back in its tile"

echo "== the last window minimised: the start page shows, Alt+Tab brings the windows back"
ctl minimize "$(id_of Links)" >/dev/null
ctl minimize "$(id_of Rechts)" >/dev/null
[ "$(ctl list | grep -c '^minimized=')" = 2 ] || fail "not both minimised: $(ctl list)"
wait_for "^start new at=" ctl hot "the start page is not shown with every window minimised"
ctl ws | grep -q "^ws 1 active=true tiles=0 tabs=2" || fail "the workspace does not count its minimised windows: $(ctl ws)"
ctl ws new >/dev/null
ctl ws 1 >/dev/null
[ "$(ctl list | grep -c '^minimized=')" = 2 ] || fail "minimised windows lost over a workspace switch: $(ctl list)"
alt_tab
wait_for 'visible=true focused=true.*title="Rechts"' ctl list "Alt+Tab from nothing did not restore the newest"
key +56 +42 15 -42 -56
wait_for 'visible=true focused=true.*title="Links"' ctl list "Shift+Alt+Tab did not restore the other"
[ "$(ctl list | grep -c '^minimized=')" = 0 ] || fail "something is still minimised: $(ctl list)"

echo "== a minimised window that closes is gone from the list"
ctl minimize "$(id_of Rechts)" >/dev/null
ctl kill "$(id_of Rechts)" >/dev/null
for _ in $(seq 60); do ctl list | grep -q 'title="Rechts"' || break; sleep 0.05; done
ctl list | grep -q 'title="Rechts"' && fail "a closed minimised window stays listed"
wait_for "end" ctl list "bitscomp does not answer any more"

echo "== a window without the focus minimised: the focus stays where it is"
client Drei 2
wait_for 'visible=true focused=true.*title="Drei"' ctl list "Drei is not focused"
d0=$(count Drei "activated=0")
ctl minimize "$(id_of Links)" >/dev/null
wait_for 'minimized=0 .*title="Links"' ctl list "Links did not minimise"
ctl list | grep -q 'visible=true focused=true.*title="Drei"' || fail "the focus left Drei: $(ctl list)"
sleep 0.3
[ "$(count Drei "activated=0")" = "$d0" ] || fail "Drei was deactivated by another window's minimise"
ctl unminimize "$(id_of Links)" >/dev/null
wait_for 'visible=true focused=true.*title="Links"' ctl list "ctl unminimize did not restore Links"

echo "== one page of a stack minimised and restored: back on its stack"
ctl click $(hot 'title="Drei"' header) >/dev/null
wait_for 'focused=true.*title="Drei"' ctl list "Drei did not take the focus"
ctl stack >/dev/null
wait_for 'page=[01]/2 visible=true focused=true.*title="Drei"' ctl list "Drei is not on Links' stack"
tile_of() { ctl list | grep "title=\"$1\"" | sed -n 's/^tile=\([0-9]*\) .*/\1/p'; }
st=$(tile_of Links)
[ -n "$st" ] && [ "$(tile_of Drei)" = "$st" ] || fail "Drei and Links are not one tile: $(ctl list)"
key 66
wait_for 'minimized=0 .*title="Drei"' ctl list "the stack page did not minimise"
wait_for "tile=$st page=0/1 visible=true focused=true.*title=\"Links\"" ctl list "Links is not alone and focused on the stack"
alt_tab
wait_for 'page=[01]/2 visible=true focused=true.*title="Drei"' ctl list "Drei did not come back onto the stack"
[ "$(tile_of Drei)" = "$st" ] && [ "$(tile_of Links)" = "$st" ] || fail "the restored page is not on its old stack: $(ctl list)"
ctl kill "$(id_of Drei)" >/dev/null
for _ in $(seq 60); do ctl list | grep -q 'title="Drei"' || break; sleep 0.05; done

echo "== a minimised window asking for fullscreen is refused and told its state as it is"
client Vier 3 --minimize-at 700 --fullscreen-at 1700
wait_for 'minimized=0 .*title="Vier"' ctl list "Vier did not minimise itself"
s0=$(count Vier "^states")
wait_line Vier "^asked fullscreen (timer)" 0 "Vier did not ask for fullscreen"
wait_line Vier "^states" "$s0" "the refused fullscreen got no answer"
last_states Vier | grep -q "fullscreen=0 maximized=0 activated=0" || fail "the answer is not the state as it is: $(last_states Vier)"
ctl list | grep -q 'minimized=0 .*title="Vier"' || fail "Vier left the list: $(ctl list)"
ctl list | grep -q " fullscreen " && fail "a minimised window took the screen"
grep -q "window $(id_of Vier) asks for fullscreen while minimised: refused" "$RUN/bitscomp.log" || fail "the refusal is not in the log"
ctl kill "$(id_of Vier)" >/dev/null
for _ in $(seq 60); do ctl list | grep -q 'title="Vier"' || break; sleep 0.05; done

echo "== set_minimized before the first commit is ignored: the window maps as usual"
client Fuenf 1 --minimized
grep -q "^asked minimize before commit" "$RUN/Fuenf.out" || fail "Fuenf did not ask"
wait_for 'visible=true focused=true.*title="Fuenf"' ctl list "Fuenf is not shown and focused"
ctl list | grep -q '^minimized' && fail "a request before the first configure minimised: $(ctl list)"
ctl kill "$(id_of Fuenf)" >/dev/null
for _ in $(seq 60); do ctl list | grep -q 'title="Fuenf"' || break; sleep 0.05; done

echo "== a hung client is minimised and restored without a configure (B4)"
client Sechs 2 --hang 0
for _ in $(seq 150); do ctl list | grep -q 'title="Sechs" pid=[0-9]* hung=true' && break; sleep 0.1; done
ctl list | grep -q 'title="Sechs" pid=[0-9]* hung=true' || fail "Sechs did not become hung within 15 s"
h0=$(count Sechs "^states")
ctl minimize "$(id_of Sechs)" >/dev/null
wait_for 'minimized=0 .*title="Sechs" pid=[0-9]* hung=true' ctl list "the hung window did not minimise"
sleep 0.4
[ "$(count Sechs "^states")" = "$h0" ] || fail "the hung client got a configure on minimise"
ctl unminimize "$(id_of Sechs)" >/dev/null
wait_for 'visible=true focused=true.*title="Sechs"' ctl list "the hung window did not come back"
sleep 0.4
[ "$(count Sechs "^states")" = "$h0" ] || fail "the hung client got a configure on restore"
ctl kill "$(id_of Sechs)" >/dev/null
for _ in $(seq 60); do ctl list | grep -q 'title="Sechs"' || break; sleep 0.05; done

echo "== session: a minimised window is saved and comes back as a tile after a restart"
ctl quit >/dev/null
wait "$COMP" 2>/dev/null || true
start_comp bitscomp-session
ctl spawn "$WYC" --title Sieben --pattern 2 >/dev/null
wait_for 'title="Sieben"' ctl list "Sieben did not map"
ctl minimize "$(id_of Sieben)" >/dev/null
ctl spawn "$WYC" --title Acht --pattern 0 >/dev/null
wait_for 'visible=true focused=true.*title="Acht"' ctl list "Acht did not map"
ctl list | grep -q 'minimized=0 .*title="Sieben"' || fail "Sieben is not minimised: $(ctl list)"
FILE=$XDG_RUNTIME_DIR/bitscomp-session.txt
saved() { [ "$(grep -c '^prog ' "$FILE" 2>/dev/null)" = 2 ] && grep -q Sieben "$FILE" && grep -q Acht "$FILE"; }
for _ in $(seq 60); do saved && break; sleep 0.1; done
saved || fail "the session does not hold the minimised window: $(cat "$FILE" 2>/dev/null)"
ctl quit >/dev/null
wait "$COMP" 2>/dev/null || true
start_comp bitscomp-restart
wait_for "start restore at=.* enabled=true" ctl hot "the start page does not offer the session"
ctl click $(ctl hot | sed -n 's/^start restore at=\([0-9]*\),\([0-9]*\).*/\1 \2/p') >/dev/null
wait_for "ws 1 active=true tiles=2 tabs=2" ctl ws "the session with the minimised window was not restored as two tiles"
for t in Sieben Acht; do wait_for "visible=true .*title=\"$t\"" ctl list "$t is not shown after the restore"; done

echo "PASS: set_minimized hides the window (no drawing, keys and clicks to the others, tile freed, focus moved, told not activated); Alt+Tab, the window list and its launcher tile restore it with a configure activated and its next frame shown; float and fullscreen handled; last window -> start page; workspace switch keeps the list; a closed one leaves it; focus kept when another minimises, stack page back on its stack, fullscreen refused while minimised, a request before the first commit ignored, a hung client gets no configure, the session keeps a minimised window"
