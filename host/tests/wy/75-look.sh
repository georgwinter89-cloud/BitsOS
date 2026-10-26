#!/usr/bin/env bash
# WY-5 on the host (headless back end, nothing on the screen): the approved look against reference images and the
# theme applied live from a real settingsd (built from user/services/settingsd, its own socket and directories).
# Dark, light and an accent change set through settingsd's client reach the screen within 1 s and undo restores
# them; screenshots are compared with host/tests/wy/ref/*.png (tolerance). Workspaces (button, Ctrl+Alt+digit),
# the address bar (notice for a web address, suggestions, BackSpace held repeats), the start page, and references
# for the Details.dc.html parts: suggestions, a ring state, the circle menu with a lit item, the hung veil. Frame costs at 2560x1440 (full redraw
# after a theme change, a hover, an animating client) and idle: no frames, about no CPU.
# WY_UPDATE_REF=1 writes the references instead of comparing (only after a look change that was meant).
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
SOCK=wy75-$$
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
settings() { "$SD" "$@" >/dev/null; }
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
        # Same host, same binary: the pictures are equal; the small tolerance only absorbs rounding. A missing glow
        # line or a wrong border colour differs by far more than 0.02 % of the pixels.
        $IMG diff "$RUN/$1.ppm" "$REF/$1.png" 8 0.0002 || fail "$1 differs from host/tests/wy/ref/$1.png"
    fi
}
start_comp() { # size, extra args...
    local size=$1
    shift
    "$BIN" --backend headless --size "$size" --socket "$SOCK" --mcp none "$@" >"$RUN/bitscomp-$size.log" 2>&1 &
    COMP=$!
    PIDS+=("$COMP")
    for _ in $(seq 50); do [ -S "$XDG_RUNTIME_DIR/$SOCK.ctl" ] && break; sleep 0.1; done
}

echo "== settingsd on the host"
mkdir -p "$RUN/stick" "$RUN/fallback" "$RUN/sys"
"$SD" serve --stick "$RUN/stick" --fallback "$RUN/fallback" --sys "$RUN/sys" --socket "$SETTINGS_SOCKET" --no-mcp \
    >"$RUN/settingsd.log" 2>&1 &
PIDS+=($!)
for _ in $(seq 50); do [ -S "$SETTINGS_SOCKET" ] && break; sleep 0.1; done
[ -S "$SETTINGS_SOCKET" ] || fail "settingsd did not start"

echo "== bitscomp 2000x1125 at 125 % (the mock-up's 1600x900), fixed status values"
start_comp 2000x1125 --settings "$SETTINGS_SOCKET" --fixed-status
wait_for "settingsd=true" ctl theme "bitscomp did not read settingsd"
ctl theme
wait_for "end" ctl list "no control socket"
ctl spawn "$WYC" --title Rechnung_Oktober --pattern 0 >/dev/null
wait_for 'title="Rechnung_Oktober"' ctl list "first client did not map"
ctl spawn "$WYC" --title Kontoauszug.csv --pattern 1 >/dev/null
wait_for 'title="Kontoauszug.csv"' ctl list "second client did not map"
ctl stack >/dev/null
ctl spawn "$WYC" --title Terminal --pattern 1 >/dev/null
wait_for 'tile=1 .*title="Terminal"' ctl list "third client did not map"
sleep 0.3
ctl shot "$RUN/dark.ppm" >/dev/null
compare dark

echo "== light through settingsd: on screen within 1 s, as the reference"
t0=$(date +%s%N)
settings set theme.mode light
wait_for "mode=light" ctl theme "theme.mode light did not arrive"
ms=$(( ($(date +%s%N) - t0) / 1000000 ))
echo "light applied after $ms ms"
[ "$ms" -le 1000 ] || fail "light took $ms ms"
sleep 0.2
ctl shot "$RUN/light.ppm" >/dev/null
compare light

echo "== accent #ffb020 through settingsd"
settings set theme.accent '#ffb020'
wait_for "accent=#ffb020" ctl theme "the accent did not arrive"
sleep 0.2
ctl shot "$RUN/accent.ppm" >/dev/null
compare accent

echo "== undo twice: dark and cyan again, the same picture as before"
settings undo
settings undo
wait_for "mode=dark accent=#00e5ff" ctl theme "undo did not restore the theme"
sleep 0.2
ctl shot "$RUN/undone.ppm" >/dev/null
$IMG diff "$RUN/undone.ppm" "$RUN/dark.ppm" 0 0 || fail "undo did not restore the same picture"

echo "== a malformed accent: settingsd refuses it, bitscomp keeps running and its picture"
"$SD" set theme.accent nonsense >/dev/null 2>&1 && fail "settingsd took the accent 'nonsense'"
sleep 0.3
ctl theme | grep -q "mode=dark accent=#00e5ff" || fail "the theme changed after a refused value: $(ctl theme)"
ctl shot "$RUN/kept.ppm" >/dev/null
$IMG diff "$RUN/kept.ppm" "$RUN/dark.ppm" 0 0 || fail "the picture changed after a refused value"

echo "== layout keys: favourites bar and status bar off, then on again"
settings set theme.layout.favourites_bar false
settings set theme.layout.statusbar false
wait_for "favourites=false sidebar=true statusbar=false" ctl theme "layout keys did not arrive"
ctl hot | grep -q "^fav " && fail "favourites still shown"
settings undo
settings undo
wait_for "favourites=true sidebar=true statusbar=true" ctl theme "layout keys not undone"

echo "== workspaces: a new one shows the start page; Ctrl+Alt+1 goes back"
ctl click $(ctl hot | sed -n 's/^wsnew at=\([0-9]*\),\([0-9]*\).*/\1 \2/p') >/dev/null
wait_for "ws 2 active=true tiles=0" ctl ws "the new workspace is not shown"
ctl hot | grep -q "^start new at=" || fail "no start page on the empty workspace"
sleep 0.2
ctl shot "$RUN/start.ppm" >/dev/null
compare start
ctl key +29 +56 2 -56 -29 >/dev/null
wait_for "ws 1 active=true tiles=2 tabs=3" ctl ws "Ctrl+Alt+1 did not show workspace 1"
ctl list | grep -q 'title="Terminal".* ws=1' || fail "the windows are not on workspace 1"
ctl click $(ctl hot | sed -n 's/^ws 1 at=\([0-9]*\),\([0-9]*\).*/\1 \2/p') >/dev/null
wait_for "ws 2 active=true" ctl ws "a click on workspace 2 did not show it"
ctl ws remove >/dev/null || fail "the empty workspace could not be removed"
wait_for "ws 1 active=true tiles=2" ctl ws "back on workspace 1 after removing 2"

echo "== address bar: web addresses are noted, suggestions come grouped"
ctl address open https://www.elster.de | grep -q "notice Webseiten kann BitsOS noch nicht öffnen" || fail "no notice for a web address"
ctl notice clear | grep -q "notice none" || fail "the notice did not clear"
ctl address abcdefghijkl >/dev/null
ctl key +14 >/dev/null
sleep 0.8
ctl key -14 >/dev/null
left=$(ctl edit | sed -n 's/^edit text="\([^"]*\)".*/\1/p')
echo "BackSpace held 0.8 s: \"$left\" left of abcdefghijkl"
[ "${#left}" -le 8 ] || fail "BackSpace held does not repeat in the address bar: $(ctl edit)"
ctl edit | grep -q "held=false" || fail "the repeat goes on after the release: $(ctl edit)"
ctl key 1 >/dev/null
ctl address term >/dev/null
ctl hot | grep -q "^suggest 0 .*group=FAVORITEN addr=programm/terminal" || fail "no favourite suggestion for 'term'"
sleep 0.2
ctl shot "$RUN/suggest.ppm" >/dev/null
compare suggest
ctl address akzent >/dev/null
ctl hot | grep -q "group=EINSTELLUNGEN addr=einstellungen/theme.accent" || fail "no settings key suggestion for 'akzent'"
ctl key 1 >/dev/null
ctl hot | grep -q "editing=false" || fail "Esc did not end editing"

echo "== the sidebar folds and the ring states draw"
ctl sidebar | grep -q folded || fail "sidebar did not fold"
ctl sidebar | grep -q open || fail "sidebar did not open"
for r in listening thinking working idle waiting; do ctl ring "$r" >/dev/null; done
sleep 0.2
ctl shot "$RUN/ring.ppm" >/dev/null
compare ring
ctl ring idle >/dev/null

echo "== the circle menu on a window, an item lit under the pointer"
ctl click $(hot 'title="Terminal"' header) right >/dev/null
ctl hot | grep -q '^menu' || fail "no circle menu on the window"
ctl pointer $(hot 'label="Breiter"' at) >/dev/null
sleep 0.3
ctl shot "$RUN/menu.ppm" >/dev/null
compare menu
ctl key 1 >/dev/null
ctl hot | grep -q '^menu' && fail "Esc did not close the menu"

echo "== the hung window's veil (Details.dc.html): the picture, Warten hides it, then the program is ended"
ctl spawn "$WYC" --title Haengt --pattern 2 --hang 200 >/dev/null
for _ in $(seq 100); do ctl list | grep -Eq 'title="Haengt" pid=[0-9]+ hung=true' && break; sleep 0.1; done
ctl list | grep -Eq 'title="Haengt" pid=[0-9]+ hung=true' || fail "Haengt did not become hung"
sleep 0.3
ctl hot | grep 'title="Haengt"' | grep -q ' kill=.* wait=' || fail "no Beenden/Warten on the veil"
ctl shot "$RUN/veil.ppm" >/dev/null
# "seit N Sekunden" may differ by a digit: well inside the tolerance.
compare veil
ctl click $(hot 'title="Haengt"' wait) >/dev/null
ctl hot | grep 'title="Haengt"' | grep -q ' kill=' && fail "Warten did not hide the veil"
hid=$(ctl list | grep 'title="Haengt"' | sed -n 's/.* id=\([0-9]*\).*/\1/p')
ctl kill "$hid" >/dev/null || fail "ctl kill $hid"
for _ in $(seq 50); do ctl list | grep -q 'title="Haengt"' || break; sleep 0.1; done
ctl list | grep -q 'title="Haengt"' && fail "the hung window did not go"

echo "== idle with fixed status values: not one frame"
sleep 0.5
ctl stats reset >/dev/null
sleep 3
ctl stats | grep -q "^frames=0 " || fail "an idle desktop drew frames: $(ctl stats)"
kill "$COMP"; wait "$COMP" 2>/dev/null || true

echo "== frame costs at 2560x1440 (125 %)"
start_comp 2560x1440 --settings "$SETTINGS_SOCKET" --fixed-status
wait_for "settingsd=true" ctl theme "bitscomp did not read settingsd"
ctl spawn "$WYC" --title A --pattern 0 >/dev/null
ctl spawn "$WYC" --title B --pattern 1 >/dev/null
wait_for 'title="B"' ctl list "clients did not map"
sleep 1
ctl stats reset >/dev/null
settings set theme.accent '#b388ff'
wait_for "accent=#b388ff" ctl theme "accent did not arrive"
sleep 0.3
full=$(ctl stats)
echo "full redraw (theme change): $full"
ctl stats reset >/dev/null
read -r hx hy <<<"$(ctl hot | sed -n 's/^win tile=0 .* float=\([0-9]*\),\([0-9]*\).*/\1 \2/p' | head -1)"
ctl pointer "$hx" "$hy" >/dev/null
sleep 0.2
echo "hover a header button: $(ctl stats)"
settings undo
ctl spawn "$WYC" --title C --pattern 0 --animate >/dev/null
wait_for 'title="C"' ctl list "animating client did not map"
sleep 0.5
ctl stats reset >/dev/null
sleep 2
echo "one animating client: $(ctl stats)"
kill "$COMP"; wait "$COMP" 2>/dev/null || true

echo "== idle: no frames and about no CPU with real status values"
start_comp 2560x1440 --settings none
ctl spawn "$WYC" --title A --pattern 0 >/dev/null
wait_for 'title="A"' ctl list "client did not map"
sleep 1.5
ctl stats reset >/dev/null
read -r u0 s0 < <(awk '{print $14, $15}' "/proc/$COMP/stat")
sleep 5
read -r u1 s1 < <(awk '{print $14, $15}' "/proc/$COMP/stat")
idle=$(ctl stats)
echo "idle 5 s: $idle; cpu ticks $(( u1 + s1 - u0 - s0 )) of $(( 5 * $(getconf CLK_TCK) ))"
frames=$(echo "$idle" | sed -n 's/.*frames=\([0-9]*\).*/\1/p')
# Real values: a frame only when the CPU value moves by 3 points (sampled every 5 s) or the minute turns.
[ "${frames:-99}" -le 2 ] || fail "idle drew $frames frames in 5 s with real status values"
[ $(( u1 + s1 - u0 - s0 )) -le 5 ] || fail "idle took $(( u1 + s1 - u0 - s0 )) ticks in 5 s"

echo "== session: saved 2 s after a change, offered on the start page after a restart, restored"
ctl spawn "$WYC" --title B --pattern 1 >/dev/null
wait_for 'title="B"' ctl list "B did not map"
ctl stack >/dev/null
ctl spawn "$WYC" --title C --pattern 0 >/dev/null
wait_for 'tile=1 .*title="C"' ctl list "C did not map"
FILE=$XDG_RUNTIME_DIR/bitscomp-session.txt
for _ in $(seq 50); do [ "$(grep -c '^prog ' "$FILE" 2>/dev/null)" = 3 ] && break; sleep 0.1; done
[ "$(grep -c '^prog ' "$FILE" 2>/dev/null)" = 3 ] || fail "no session file with three programs: $(cat "$FILE" 2>/dev/null)"
ctl quit >/dev/null
wait "$COMP" 2>/dev/null || true
start_comp 2560x1440 --settings none
wait_for "start restore at=.* enabled=true" ctl hot "the start page does not offer the last session"
ctl click $(ctl hot | sed -n 's/^start restore at=\([0-9]*\),\([0-9]*\).*/\1 \2/p') >/dev/null
wait_for "ws 1 active=true tiles=2 tabs=3" ctl ws "the session was not restored (tiles and stack)"
[ "$(ctl list | grep -Ec 'page=[01]/2 .*title="(A|B)"')" = 2 ] || fail "A and B are not back on one stack: $(ctl list)"
echo PASS
