#!/usr/bin/env bash
# WY-8 on the host (headless back end, nothing on the screen): a client's own fullscreen (xdg set_fullscreen /
# unset_fullscreen, as Brave asks for it on a video's fullscreen button or F11) and its own maximise
# (set_maximized). A fullscreen window covers the whole output above every bar and float, gets the output size,
# takes every click; F11 and Esc stay the client's (wyclient asks like a browser); leaving restores the tile, the
# float and the maximised float exactly; Ctrl+Alt+M leaves it; the focus moving on ends it; on a parked workspace
# it does not cover the shown one; a fullscreen asked for before the first commit is there in the first configure;
# a window opened under it stays behind; a request without the focus or from a hung client is refused; 20 x
# set/unset at once ends where it started; a lone unset_fullscreen changes nothing.
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
SOCK=wy90-$$

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
# The last `states` line of a client: what it was told last.
last_states() { grep '^states' "$RUN/$1.out" | tail -1; }
wait_states() { # client, states pattern, what
    for _ in $(seq 60); do last_states "$1" | grep -q -- "$2" && return 0; sleep 0.05; done
    fail "$3 (last: $(last_states "$1"))"
}
last_configure() { grep '^configure' "$RUN/$1.out" | tail -1; }
client() { # title pattern [args...]: a wyclient whose lines go to $RUN/<title>.out
    local t=$1 p=$2
    shift 2
    WAYLAND_DISPLAY=$SOCK "$WYC" --title "$t" --pattern "$p" "$@" >"$RUN/$t.out" 2>&1 &
    PIDS+=($!)
    wait_for "title=\"$t\"" ctl list "$t did not map"
}
# Frames of every window, without the fullscreen words, for "restored exactly".
frames() { ctl list | grep -v '^end' | sed 's/ fullscreen//; s/ focused=[a-z]*//' | sort; }
pixel() { ctl shot "$RUN/p.ppm" >/dev/null; $IMG pixel "$RUN/p.ppm" "$1" "$2"; }
red() { # x y: the screen pixel shows the fullscreen client's colour (pattern 0), within a second (its frame)
    local c
    for _ in $(seq 20); do
        c=$(pixel "$1" "$2")
        [ "$c" = "192 48 48" ] && return 0
        sleep 0.05
    done
    fail "pixel $1,$2 is '$c', not the fullscreen client's (bars or frames over it?)"
}

echo "== bitscomp 2000x1125 (1600x900 logical), two tiles and a float"
"$BIN" --backend headless --size 2000x1125 --socket "$SOCK" --mcp none --fixed-status --settings none >"$RUN/bitscomp.log" 2>&1 &
PIDS+=($!)
for _ in $(seq 50); do [ -S "$XDG_RUNTIME_DIR/$SOCK.ctl" ] && break; sleep 0.1; done
wait_for "end" ctl list "no control socket"
client Links 0
client Rechts 1
client Schwebt 2
ctl click $(hot 'title="Schwebt"' float) >/dev/null
wait_for 'float=0 visible=true focused=true.*title="Schwebt"' ctl list "Schwebt did not float"
ctl click $(hot 'title="Links"' header) >/dev/null
wait_for 'focused=true.*title="Links"' ctl list "Links did not take the focus"
before=$(frames)
[ "$(pixel 4 4)" != "192 48 48" ] || fail "the top bar is not there before (the test would prove nothing)"

echo "== F11 in the tile: the client asks, covers the whole screen above every bar and the float"
key 87
wait_states Links "fullscreen=1" "Links got no fullscreen configure"
grep -q "^asked fullscreen" "$RUN/Links.out" || fail "F11 did not reach the client"
[ "$(last_configure Links)" = "configure 1600 900" ] || fail "fullscreen size is not the output's: $(last_configure Links)"
wait_for 'tile=0 page=0/1 fullscreen visible=true focused=true frame=0,0 1600x900 .*title="Links"' ctl list "ctl list shows no fullscreen Links"
ctl list | grep 'title="Rechts"' | grep -q "visible=false" || fail "the other tile is not hidden under it"
ctl list | grep 'title="Schwebt"' | grep -q "visible=false" || fail "the float is not hidden under it"
red 4 4         # the top bar's place
red 1995 600    # Claude's sidebar
red 4 600       # the workspace list
red 1000 1120   # the status bar
red 1000 300    # where the float lay

echo "== every click is the client's, no bar comes at the edge"
n=$(grep -c "^button 272 pressed" "$RUN/Links.out" || true)
ctl click 4 4 >/dev/null
for _ in $(seq 40); do [ "$(grep -c "^button 272 pressed" "$RUN/Links.out" || true)" -gt "$n" ] && break; sleep 0.05; done
[ "$(grep -c "^button 272 pressed" "$RUN/Links.out" || true)" -gt "$n" ] || fail "a click at the top bar's place did not reach the client"
ctl pointer 1999 600 >/dev/null
sleep 0.6
ctl pointer 1998 600 >/dev/null
ctl bars | grep -q revealed && fail "a bar was revealed over the fullscreen window: $(ctl bars)"
red 1995 600

echo "== Esc is the client's: it leaves, the tile comes back exactly"
key 1
wait_states Links "fullscreen=0" "Esc did not end the fullscreen"
grep -q "^asked unfullscreen" "$RUN/Links.out" || fail "Esc did not reach the client"
wait_for 'tile=0 page=0/1 visible=true focused=true.*title="Links"' ctl list "Links is not back in its tile"
after=$(frames)
[ "$after" = "$before" ] || fail "not restored exactly:
$before
--- after:
$after"
[ "$(pixel 4 4)" != "192 48 48" ] || fail "the top bar did not come back"

echo "== the float: F11, then Ctrl+Alt+M (the desktop's own way out)"
ctl click $(hot 'title="Schwebt"' header) >/dev/null
wait_for 'float=0 visible=true focused=true.*title="Schwebt"' ctl list "Schwebt did not take the focus"
before=$(frames)
key 87
wait_states Schwebt "fullscreen=1" "the float got no fullscreen"
[ "$(last_configure Schwebt)" = "configure 1600 900" ] || fail "float fullscreen size: $(last_configure Schwebt)"
key +29 +56 50 -56 -29
wait_states Schwebt "fullscreen=0" "Ctrl+Alt+M did not end the fullscreen"
last_states Schwebt | grep -q "maximized=0" || fail "Ctrl+Alt+M on a fullscreen window also maximised it"
wait_for 'float=0 visible=true focused=true.*title="Schwebt"' ctl list "Schwebt is not back"
[ "$(frames)" = "$before" ] || fail "the float did not come back exactly: $(frames)"
grep -q "leaves fullscreen (Maximise)" "$RUN/bitscomp.log" || fail "the log does not say why the fullscreen ended"

echo "== a maximised float goes fullscreen and comes back maximised; Ctrl+Alt+M still maximises"
key +29 +56 50 -56 -29
wait_for 'float=0 max visible=true.*title="Schwebt"' ctl list "Ctrl+Alt+M did not maximise the float"
wait_states Schwebt "maximized=1" "no maximized state"
maxsize=$(last_configure Schwebt)
before=$(frames)
key 87
wait_states Schwebt "fullscreen=1" "the maximised float got no fullscreen"
key 87
wait_states Schwebt "fullscreen=0 maximized=1" "F11 again did not give the maximised float back"
[ "$(frames)" = "$before" ] || fail "the maximised float did not come back exactly"
key +29 +56 50 -56 -29
wait_states Schwebt "maximized=0" "Ctrl+Alt+M did not restore"

echo "== the client's own maximise: a tile floats and is maximised, and back (F10 = set_maximized)"
ctl click $(hot 'title="Rechts"' header) >/dev/null
wait_for 'focused=true.*title="Rechts"' ctl list "Rechts did not take the focus"
key 68
wait_states Rechts "maximized=1" "set_maximized of a tile did nothing"
wait_for 'float=[0-9] max visible=true focused=true.*title="Rechts"' ctl list "Rechts is not a maximised float"
[ "$(last_configure Rechts)" = "$maxsize" ] || fail "maximised size $(last_configure Rechts), Ctrl+Alt+M gives $maxsize"
key 68
wait_states Rechts "maximized=0" "unset_maximized did nothing"
ctl list | grep 'title="Rechts"' | grep -q " max " && fail "Rechts is still maximised"
grep -q "maximised on its own request" "$RUN/bitscomp.log" || fail "no log line for the client's maximise"

echo "== the focus moving on ends the fullscreen (Ctrl+Alt+Right)"
ctl click $(hot 'title="Links"' header) >/dev/null
wait_for 'focused=true.*title="Links"' ctl list "Links did not take the focus"
key 87
wait_states Links "fullscreen=1" "Links got no fullscreen (second time)"
key +29 +56 106 -56 -29
wait_states Links "fullscreen=0" "focus moved on, but Links stayed fullscreen"
grep -q "leaves fullscreen (the focus moved on)" "$RUN/bitscomp.log" || fail "no log line for the focus ending it"
ctl list | grep -q " fullscreen " && fail "a window is still fullscreen: $(ctl list)"

echo "== fullscreen on a parked workspace does not cover the shown one"
ctl click $(hot 'title="Links"' header) >/dev/null
wait_for 'focused=true.*title="Links"' ctl list "Links did not take the focus (third time)"
key 87
wait_states Links "fullscreen=1" "Links got no fullscreen (third time)"
ctl ws new >/dev/null
ctl ws 2 >/dev/null
for _ in $(seq 40); do ctl list | grep -q 'title="Links"' || break; sleep 0.05; done
ctl list | grep -q 'title="Links"' && fail "Links is listed on the new workspace"
# INT-W13: the switch is drawn by the next frame; a real cover stays, a frame not yet drawn does not (1 run in 3 read it).
for _ in $(seq 40); do [ "$(pixel 4 4)" != "192 48 48" ] && break; sleep 0.05; done
[ "$(pixel 4 4)" != "192 48 48" ] || fail "the parked fullscreen window covers the shown workspace"
ctl ws 1 >/dev/null
wait_for 'fullscreen visible=true focused=true frame=0,0 1600x900 .*title="Links"' ctl list "back on workspace 1, Links does not cover it again"
red 4 4
last_states Links | grep -q "fullscreen=1" || fail "the client was told something else meanwhile"
key 1
wait_states Links "fullscreen=0" "Esc after the workspace round trip"

echo "== a window closing in fullscreen leaves no state behind"
client Zu 0
wait_for 'focused=true.*title="Zu"' ctl list "Zu did not take the focus"
key 87
wait_states Zu "fullscreen=1" "Zu got no fullscreen"
key +29 +56 45 -56 -29
wait_for 'title="Links"' ctl list "list gone"
for _ in $(seq 40); do ctl list | grep -q 'title="Zu"' || break; sleep 0.05; done
ctl list | grep -q 'title="Zu"' && fail "Ctrl+Alt+X did not close the fullscreen window"
ctl list | grep -q " fullscreen " && fail "a fullscreen state outlived its window"
for _ in $(seq 40); do [ "$(pixel 4 4)" != "192 48 48" ] && break; sleep 0.05; done
[ "$(pixel 4 4)" != "192 48 48" ] || fail "the bars did not come back after the fullscreen window closed"

echo "== fullscreen asked for before the first commit: the first configure is fullscreen already"
client Gleich 0 --fullscreen
wait_states Gleich "fullscreen=1" "Gleich did not get fullscreen"
first=$(grep -m1 '^configure' "$RUN/Gleich.out")
firsts=$(grep -m1 '^states' "$RUN/Gleich.out")
[ "$first" = "configure 1600 900" ] && echo "$firsts" | grep -q "fullscreen=1" || fail "the first configure was '$first' '$firsts'"
wait_for 'fullscreen visible=true focused=true frame=0,0 1600x900 .*title="Gleich"' ctl list "Gleich does not cover the screen"
red 4 4

echo "== a window opened under the fullscreen one stays behind; its own fullscreen without the focus is refused"
client Hinten 1 --fullscreen-at 1500
wait_for 'fullscreen visible=true focused=true frame=0,0 1600x900 .*title="Gleich"' ctl list "a new window ended Gleich's fullscreen"
ctl list | grep 'title="Hinten"' | grep -q "visible=false focused=false" || fail "Hinten is not behind: $(ctl list | grep Hinten)"
wait_for "asked fullscreen (timer)" cat "$RUN/Hinten.out" "Hinten did not ask"
wait_for "without the focus: refused" cat "$RUN/bitscomp.log" "no log line for the refused request"
wait_states Hinten "fullscreen=0" "Hinten was not answered with its state"
ctl list | grep 'title="Hinten"' | grep -q " fullscreen " && fail "Hinten took the screen without the focus"
wait_for 'fullscreen visible=true focused=true.*title="Gleich"' ctl list "Gleich lost the screen to Hinten"
red 4 4
last_states Gleich | grep -q "fullscreen=1" || fail "Gleich was told to leave"

echo "== 20 times fullscreen and back at once: ends where it started"
key 1
wait_states Gleich "fullscreen=0" "Esc did not end Gleich's fullscreen"
wait_for 'focused=true.*title="Gleich"' ctl list "Gleich lost the focus"
before=$(frames)
n=$(grep -c '^states' "$RUN/Gleich.out")
key 67
wait_for "asked toggle 20" cat "$RUN/Gleich.out" "F9 did not reach Gleich"
for _ in $(seq 60); do [ "$(grep -c '^states' "$RUN/Gleich.out")" -gt "$n" ] && break; sleep 0.05; done
sleep 0.5
last_states Gleich | grep -q "fullscreen=0" || fail "after the toggles: $(last_states Gleich)"
ctl list | grep -q " fullscreen " && fail "a window is fullscreen after 20 x set/unset"
[ "$(frames)" = "$before" ] || fail "not back where it started after the toggles"

echo "== unset_fullscreen with no fullscreen before: nothing changes"
client Nie 1 --unfullscreen
wait_states Nie "fullscreen=0" "Nie got no configure"
grep '^states' "$RUN/Nie.out" | grep -q "fullscreen=1" && fail "Nie was made fullscreen"
ctl list | grep -q " fullscreen " && fail "a window is fullscreen after a lone unset_fullscreen"

echo "== a hung client asking for fullscreen: refused, no configure, bitscomp answers"
WAYLAND_DISPLAY=$SOCK "$WYC" --title Haengt --pattern 2 --hang 0 --fullscreen-at 5000 >"$RUN/Haengt.out" 2>&1 &
PIDS+=($!)
for _ in $(seq 100); do ctl list | grep -q 'title="Haengt" pid=[0-9]* hung=true' && break; sleep 0.1; done
ctl list | grep -q 'title="Haengt" pid=[0-9]* hung=true' || fail "Haengt did not become hung"
c=$(grep -c '^configure' "$RUN/Haengt.out")
for _ in $(seq 100); do grep -q "asked fullscreen (timer)" "$RUN/Haengt.out" && break; sleep 0.1; done
grep -q "asked fullscreen (timer)" "$RUN/Haengt.out" || fail "Haengt did not ask"
wait_for "does not answer: refused" cat "$RUN/bitscomp.log" "no log line for the hung client's request"
sleep 0.5
[ "$(grep -c '^configure' "$RUN/Haengt.out")" = "$c" ] || fail "the hung client got a configure"
ctl list | grep -q " fullscreen " && fail "the hung client took the screen"
wait_for "end" ctl list "bitscomp does not answer any more"

echo "PASS: client fullscreen above every bar and float with the output size, clicks and F11/Esc the client's, no edge reveal, exact restore (tile, float, maximised float), Ctrl+Alt+M out, focus move ends it, parked workspace does not cover, close clears it, fullscreen before the first commit; client maximise of a tile; a new window stays behind, an unfocused or hung window is refused, 20x set/unset, lone unset"
