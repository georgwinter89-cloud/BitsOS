#!/usr/bin/env bash
# PK-L1a on the host (headless back end, nothing on the screen): the program library's index over a fake stick bin
# folder with the real .desktop files of the stick launchers (user/ports/*/bitsos/*.desktop) beside fake launchers
# (small scripts named brave, libreoffice, steam, vkquake, cobalt; nothing big is started). `ctl library` lists
# Brave, LibreOffice with Writer, Calc, Impress and Draw, Steam, vkQuake, Cobalt, Terminal and Einstellungen; Brave's
# icon (a host PNG standing in for brave.png) is decoded to the tile size, the others show the fallback; a search
# finds Writer first; a launcher taken away takes its entries with it while bitscomp runs (inotify); a package put
# into the /apps that was missing at the start shows up (5 s rescan) and goes with its `current` link.
set -euo pipefail

ROOT=${BITSOS_ROOT:-$(cd "$(dirname "$0")/../../.." && pwd)}
RUN=${BITSOS_RUN_DIR:-$(mktemp -d)}
mkdir -p "$RUN"
RUN=$(realpath "$RUN")
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-$ROOT/target}
BIN=$CARGO_TARGET_DIR/release/bitscomp
export XDG_RUNTIME_DIR=$RUN/xdg
mkdir -m 700 -p "$XDG_RUNTIME_DIR"
unset DISPLAY WAYLAND_DISPLAY
SOCK=wy51-$$

fail() { echo "FAIL: $*"; exit 1; }

echo "== build bitscomp"
(cd "$ROOT/user/services/bitscomp" && cargo build --release -q --offline)

FAKE=$RUN/stick-bin
rm -rf "$FAKE" "$RUN/apps"
mkdir -p "$FAKE"
for l in brave libreoffice steam vkquake cobalt; do
    printf '#!/bin/sh\necho "fake %s $*"\n' "$l" > "$FAKE/$l"
    chmod 755 "$FAKE/$l"
done
cp "$ROOT"/user/ports/{brave-launcher,libreoffice-launcher,steam,vkquake,cobalt-launcher}/bitsos/*.desktop "$FAKE/"
ICON=$(ls /usr/share/icons/hicolor/128x128/apps/*.png | head -1)
cp "$ICON" "$FAKE/brave.png"

PIDS=()
cleanup() {
    for p in "${PIDS[@]}"; do kill "$p" 2>/dev/null || true; done
    for p in "${PIDS[@]}"; do wait "$p" 2>/dev/null || true; done
}
trap cleanup EXIT

ctl() { "$BIN" ctl --socket "$SOCK" "$@"; }

echo "== bitscomp headless with the fake stick bin (no /apps)"
BITSCOMP_LIBRARY_BIN=$FAKE BITSCOMP_LIBRARY_APPS=$RUN/apps \
    "$BIN" --backend headless --size 1280x800 --socket "$SOCK" --mcp none --settings none >"$RUN/bitscomp.log" 2>&1 &
PIDS+=($!)

WANT="brave libreoffice writer calc impress draw steam vkquake cobalt terminal einstellungen"
ok=
for _ in $(seq 100); do
    if out=$(ctl library 2>/dev/null) && [ "$(printf '%s\n' "$out" | grep -c .)" -ge 11 ] && ! printf '%s\n' "$out" | grep -q pending; then
        ok=1
        break
    fi
    sleep 0.1
done
[ -n "$ok" ] || fail "no full index in 10 s: $(ctl library 2>&1 | head -20)"
printf '%s\n' "$out" > "$RUN/library.txt"
cat "$RUN/library.txt"
for id in $WANT; do
    grep -q "^$id	" "$RUN/library.txt" || fail "no entry $id"
done
for pair in "brave	Brave" "writer	Writer" "calc	Calc" "impress	Impress" "draw	Draw" "steam	Steam" "vkquake	vkQuake" \
    "cobalt	Cobalt" "terminal	Terminal" "einstellungen	Einstellungen"; do
    grep -q "^$pair	" "$RUN/library.txt" || fail "entry not named as wanted: $pair"
done
grep -q "^brave	Brave	stick	png:128x128:$FAKE/brave.png	/bin/sh $FAKE/brave$" "$RUN/library.txt" || fail "Brave's line"
grep -q "^writer	Writer	stick	fallback:W (.*	/bin/sh $FAKE/libreoffice writer$" "$RUN/library.txt" || fail "Writer's line (its tree icon is not on the host)"
grep -q "^steam	Steam	stick	fallback:S (no Icon)	" "$RUN/library.txt" || fail "Steam's fallback icon"
grep -q "^terminal	Terminal	builtin	fallback:T	foot " "$RUN/library.txt" || fail "Terminal's line"
# A-Z, German order: the names' first column order.
names=$(cut -f2 "$RUN/library.txt" | tr '\n' ',')
[ "$names" = "Brave,Calc,Cobalt,Draw,Einstellungen,Impress,LibreOffice,Steam,Terminal,vkQuake,Writer," ] || fail "order: $names"

echo "== search"
first=$(ctl library search wri | head -1 | cut -f1)
[ "$first" = writer ] || fail "search wri: first is '$first'"
first=$(ctl library search tabellen | head -1 | cut -f1)
[ "$first" = calc ] || fail "search tabellen: first is '$first'"

echo "== a launcher taken away while bitscomp runs"
rm "$FAKE/libreoffice"
for _ in $(seq 100); do
    ctl library | grep -q "^writer	" || break
    sleep 0.1
done
ctl library | grep -q "^writer	\|^calc	\|^libreoffice	" && fail "LibreOffice's entries stayed without its launcher"
grep -q "library: .*libreoffice is not there" "$RUN/bitscomp.log" || fail "no event line for the missing launcher"
grep -q "bitscomp: library: 11 programs" "$RUN/bitscomp.log" || fail "no event line for the first index"

echo "== a package appears in the missing /apps (5 s rescan, then watched) and goes again"
grep -q "library: no watch on $RUN/apps .*looking every 5 s" "$RUN/bitscomp.log" || fail "no event line for the rescan fallback"
mkdir -p "$RUN/apps/jq/1/bin"
printf '#!/bin/sh\nexit 0\n' > "$RUN/apps/jq/1/bin/jq"
chmod 755 "$RUN/apps/jq/1/bin/jq"
printf 'format = 1\nname = "jq"\nprogram = "bin/jq"\n' > "$RUN/apps/jq/1/manifest.toml"
ln -s 1 "$RUN/apps/jq/current"
ok=
for _ in $(seq 150); do
    if ctl library | grep -q "^jq	Jq	package:jq	"; then ok=1; break; fi
    sleep 0.1
done
[ -n "$ok" ] || fail "the package jq did not appear in 15 s: $(ctl library 2>&1 | cut -f1 | tr '\n' ' ')"
rm "$RUN/apps/jq/current"
ok=
for _ in $(seq 150); do
    if ! ctl library | grep -q "^jq	"; then ok=1; break; fi
    sleep 0.1
done
[ -n "$ok" ] || fail "the package jq stayed after its current link went"
echo "PASS 51-library"
