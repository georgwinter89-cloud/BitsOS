#!/usr/bin/env bash
# PK-L1b on the host (headless back end at the Mini-PC's 2560x1440, nothing on the screen): the program library's
# overlay over a fake stick bin (the real .desktop files of the stick launchers beside small fake launchers; Writer's
# and Brave's map a wyclient window with the app_id of the real program and count their starts; nothing big runs).
# Win alone opens it, Win+Up does not; "wri" + Enter starts the fake Writer (window libreoffice-writer); a minimised
# fake Brave comes back instead of a second start; the circle menu of a tile has its four entries, pinning Brave puts
# it on the start page; Esc closes. References of the open overlay in dark and light: host/tests/ds/ref/library-*.png
# (DS_UPDATE_REF=1 writes them instead of comparing, only after a look change that was meant).
set -euo pipefail

ROOT=${BITSOS_ROOT:-$(cd "$(dirname "$0")/../../.." && pwd)}
RUN=${BITSOS_RUN_DIR:-$(mktemp -d)}
mkdir -p "$RUN"
RUN=$(realpath "$RUN")
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-$ROOT/target}
BIN=$CARGO_TARGET_DIR/release/bitscomp
WYC=$CARGO_TARGET_DIR/release/wyclient
REF=$ROOT/host/tests/ds/ref
IMG="python3 -I $ROOT/host/tests/ds/ds_img.py"
export XDG_RUNTIME_DIR=$RUN/xdg
rm -rf "$XDG_RUNTIME_DIR"
mkdir -m 700 -p "$XDG_RUNTIME_DIR"
unset DISPLAY WAYLAND_DISPLAY
SOCK=wy52-$$

fail() { echo "FAIL: $*"; exit 1; }

echo "== build bitscomp, wyclient"
(cd "$ROOT/user/services/bitscomp" && cargo build --release -q --offline && cargo build --release -q --offline -p wyclient)

FAKE=$RUN/stick-bin
rm -rf "$FAKE" "$RUN"/starts-*
mkdir -p "$FAKE"
# Writer and Brave: count the start, then a window with the real program's app_id (as LibreOffice and Brave do).
cat > "$FAKE/libreoffice" <<EOF
#!/bin/sh
echo "\$*" >> "$RUN/starts-libreoffice"
case "\$1" in
    writer) exec "$WYC" --title Writer --app-id libreoffice-writer ;;
    *) exec "$WYC" --title "LibreOffice \$1" --app-id "libreoffice-\${1:-startcenter}" ;;
esac
EOF
cat > "$FAKE/brave" <<EOF
#!/bin/sh
echo start >> "$RUN/starts-brave"
exec "$WYC" --title Brave --app-id brave-browser
EOF
# vkQuake's fake gives up at once with an error (the overlay's "ended before its window" line).
printf '#!/bin/sh\necho "fake vkquake gives up"\nexit 3\n' > "$FAKE/vkquake"
for l in steam cobalt; do
    printf '#!/bin/sh\necho "fake %s $*"\n' "$l" > "$FAKE/$l"
done
chmod 755 "$FAKE"/*
cp "$ROOT"/user/ports/{brave-launcher,libreoffice-launcher,steam,vkquake,cobalt-launcher}/bitsos/*.desktop "$FAKE/"
# No brave.png: every tile shows the fallback icon, so the references do not hang on the host's icons.

PIDS=()
cleanup() {
    for p in "${PIDS[@]}"; do kill "$p" 2>/dev/null || true; done
    for p in "${PIDS[@]}"; do wait "$p" 2>/dev/null || true; done
}
trap cleanup EXIT

ctl() { "$BIN" ctl --socket "$SOCK" "$@"; }
wait_for() { # pattern, what, command...
    local pat=$1 what=$2
    shift 2
    for _ in $(seq 100); do
        "$@" 2>/dev/null | grep -q -- "$pat" && return 0
        sleep 0.1
    done
    fail "$what: $("$@" 2>&1 | head -30)"
}
state() { ctl library state; }
starts() { [ -f "$RUN/starts-$1" ] && grep -c . "$RUN/starts-$1" || echo 0; }
# The screen point of tile `id` in row `title` ("Alle Programme" ...).
tile_at() {
    local row
    row=$(state | sed -n "s/^row \([0-9]*\) title=\"$1\".*/\1/p")
    [ -n "$row" ] || fail "no row $1: $(state)"
    state | sed -n "s/^tile row=$row i=[0-9]* id=$2 at=\([0-9]*\),\([0-9]*\) .*/\1 \2/p" | head -1
}

echo "== bitscomp headless 2560x1440 with the fake stick bin"
BITSCOMP_LIBRARY_BIN=$FAKE BITSCOMP_LIBRARY_APPS=$RUN/apps \
    "$BIN" --backend headless --size 2560x1440 --socket "$SOCK" --fixed-status --mcp none --settings none >"$RUN/bitscomp.log" 2>&1 &
PIDS+=($!)
wait_for "^writer	" "no Writer in the index" ctl library
wait_for "^brave	" "no Brave in the index" ctl library
ctl pointer 4 1400 >/dev/null

echo "== Win+Up does not open it, Win alone does, Win again closes it"
ctl key +125 103 -125 >/dev/null
state | grep -q "^library open=false" || fail "Win+Up opened the library"
ctl key +125 -125 >/dev/null
state | grep -q "^library open=true" || fail "Win alone did not open the library: $(state | head -3)"
grep -q "bitscomp: library: opened (Win alone)" "$RUN/bitscomp.log" || fail "no event line for the opening"
state | grep -q '^row 0 title="Favoriten" ids=terminal,einstellungen' || fail "favourites row: $(state | grep '^row')"
state | grep -q '^row 1 title="Alle Programme" ids=brave,calc,cobalt,draw,einstellungen,impress,libreoffice,steam,terminal,vkquake,writer$' \
    || fail "all programs A-Z: $(state | grep '^row')"
ctl key +125 -125 >/dev/null
state | grep -q "^library open=false" || fail "Win again did not close it"
grep -q "bitscomp: library: closed (Win alone)" "$RUN/bitscomp.log" || fail "no event line for the closing"

echo "== combinations through the real input path keep it closed; Win alone over the address bar"
ctl key +125 >/dev/null
ctl click 4 1400 >/dev/null
ctl key -125 >/dev/null
state | grep -q "^library open=false" || fail "Win with a click between opened the library"
for combo in "+125 15 -125" "+125 44 -125" "+125 57 -125" "+125 1 -125" "+56 15 -56"; do
    ctl key $combo >/dev/null
    state | grep -q "^library open=false" || fail "key $combo opened the library"
done
ctl address >/dev/null
ctl edit | grep -q "^edit text=" || fail "the address bar did not start editing"
ctl key +125 -125 >/dev/null
state | grep -q "^library open=true" || fail "Win alone over the address bar did not open the library"
ctl edit | grep -q "^edit none" || fail "the address bar still edits under the library"
ctl key 103 >/dev/null
state | grep -q "^library open=true" || fail "an arrow closed the library"
ctl key +125 103 -125 >/dev/null
state | grep -q "^library open=true" || fail "Win+Up with the library open closed or toggled it"
ctl key 1 >/dev/null
state | grep -q "^library open=false" || fail "Esc did not close it"

echo "== the top bar's Programme button opens it; references dark and light"
set -- $(ctl hot | sed -n 's/^programs at=\([0-9.]*\),\([0-9.]*\) .*/\1 \2/p')
[ $# = 2 ] || fail "no programs line in ctl hot"
ctl click "$1" "$2" >/dev/null
state | grep -q "^library open=true" || fail "the Programme button did not open it"
ctl pointer 4 1400 >/dev/null
SHOTS=$RUN/shots
rm -rf "$SHOTS"
mkdir -p "$SHOTS"
ctl shot "$SHOTS/library-dark.ppm" >/dev/null
ctl set theme.mode light >/dev/null
ctl shot "$SHOTS/library-light.ppm" >/dev/null
ctl set theme.mode dark >/dev/null
for s in library-dark library-light; do
    if [ "${DS_UPDATE_REF:-0}" = 1 ]; then
        $IMG png "$SHOTS/$s.ppm" "$REF/$s.png"
        echo "reference $s.png written"
    else
        $IMG diff "$SHOTS/$s.ppm" "$REF/$s.png" || fail "$s differs from host/tests/ds/ref/$s.png"
    fi
done
$IMG diff "$SHOTS/library-dark.ppm" "$SHOTS/library-light.ppm" >/dev/null && fail "the library looks the same in both modes"
ctl key 1 >/dev/null
state | grep -q "^library open=false" || fail "Esc did not close it"

echo "== \"wri\" + Enter starts the fake Writer"
ctl key +125 -125 17 19 23 >/dev/null
state | grep -q '^library open=true query="wri" focus=writer ' || fail "search wri: $(state | head -2)"
ctl key 28 >/dev/null
wait_for 'app_id=libreoffice-writer title="Writer"' "Writer's window did not map" ctl list
state | grep -q "^library open=false" || fail "a start did not close the library"
[ "$(starts libreoffice)" = 1 ] || fail "LibreOffice started $(starts libreoffice) times"
grep -q "bitscomp: library: started writer (pid [0-9]*): /bin/sh $FAKE/libreoffice writer" "$RUN/bitscomp.log" || fail "no event line for the start"

echo "== Brave by a click; minimised, it comes back instead of a second start"
ctl key +125 -125 >/dev/null
set -- $(tile_at "Alle Programme" brave)
[ $# = 2 ] || fail "no Brave tile: $(state)"
ctl click "$1" "$2" >/dev/null
wait_for 'app_id=brave-browser title="Brave"' "Brave's window did not map" ctl list
[ "$(starts brave)" = 1 ] || fail "Brave started $(starts brave) times"
id=$(ctl list | grep 'title="Brave"' | sed -n 's/.* id=\([0-9]*\) .*/\1/p')
ctl minimize "$id" >/dev/null
wait_for "^minimized=0 .*title=\"Brave\"" "Brave did not minimise" ctl list
ctl key +125 -125 >/dev/null
state | grep -q '^row [0-9] title="Zuletzt benutzt" ids=brave,writer$' || fail "recent by program: $(state | grep '^row')"
ctl key 48 19 30 28 >/dev/null
wait_for 'title="Brave"' "Brave was not restored" ctl list
ctl list | grep -q '^minimized=.*title="Brave"' && fail "Brave is still minimised"
grep -q "bitscomp: library: brave restored" "$RUN/bitscomp.log" || fail "no event line for the restore"
sleep 0.5
[ "$(starts brave)" = 1 ] || fail "a second Brave started instead of the restore ($(starts brave) starts)"

echo "== the circle menu of a tile; pinning Brave puts it on the start page"
ctl key +125 -125 >/dev/null
set -- $(tile_at "Alle Programme" brave)
ctl click "$1" "$2" right >/dev/null
hot=$(ctl hot)
for want in 'item=0 .*enabled=true label="An Startseite anheften"' 'item=2 .*enabled=true label="In neuer Kachel öffnen"' \
    'item=4 .*enabled=false label="Programm verwalten"' 'item=6 .*enabled=false label="Entfernen"'; do
    printf '%s\n' "$hot" | grep -q "^$want" || fail "menu entry $want: $(printf '%s\n' "$hot" | grep '^item')"
done
set -- $(printf '%s\n' "$hot" | sed -n 's/^item=0 at=\([0-9.]*\),\([0-9.]*\) .*/\1 \2/p')
ctl click "$1" "$2" >/dev/null
state | grep -q "^favourites=programm/terminal,einstellungen/aussehen,programm/brave$" || fail "Brave not pinned: $(state | sed -n 2p)"
state | grep -q '^row 0 title="Favoriten" ids=terminal,einstellungen,brave$' || fail "favourites row after the pin"
state | grep -q "^library open=true .*note=\"„Brave“ ist an der Startseite angeheftet. Nur bis zum Neustart" || fail "no note for the pin: $(state | head -1)"
grep -q "bitscomp: library: brave added to the favourites (local only" "$RUN/bitscomp.log" || fail "no event line for the pin"
# Lösen is offered now.
set -- $(tile_at "Favoriten" brave)
ctl click "$1" "$2" right >/dev/null
ctl hot | grep -q '^item=0 .*label="Lösen"' || fail "a pinned tile offers no Lösen"
ctl key 1 >/dev/null
ctl hot | grep -q '^menu centre' && fail "Esc did not close the circle menu"
state | grep -q "^library open=true" || fail "Esc on the menu closed the library too"
ctl key 1 >/dev/null
state | grep -q "^library open=false" || fail "Esc did not close the library"
ctl start >/dev/null
wait_for "^start fav 2 at=" "the start page shows no third favourite" ctl hot

echo "== a program that ends at once with an error says so"
ctl key +125 -125 >/dev/null
state | grep -q "^library open=true" || fail "Win alone did not open the library over the start page"
ctl key 47 37 16 28 >/dev/null
wait_for "hat sich gleich wieder beendet (Fehlercode 3)" "no notice for a start that ended at once" ctl notice
grep -q "bitscomp: library: .* (pid [0-9]*) ended before its window" "$RUN/bitscomp.log" || fail "no event line for the failed start"
echo "PASS 52-library-overlay"
