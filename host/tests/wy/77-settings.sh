#!/usr/bin/env bash
# WY-5b on the host (headless back end, nothing on the screen): the settings page bitscomp draws itself as a tab
# (`einstellungen/aussehen`), against references in dark and light; its controls change settingsd (a swatch, the
# hex field, a choice, a switch, the scale, the favourites list) and its "Rückgängig" button undoes through
# settingsd until the picture is the first one again; `einstellungen/aussehen/akzent` jumps to that row; without
# settingsd the page only shows. WY-9: rows come from settingsd's schema, so `einstellungen/kernel/snd_output`
# offers that key's choices auto/jack/hdmi/usb and hdmi writes the key (and its /sys file), undone again. WY-10: a confirm key changes only after the page's dialog (Abbrechen, Esc and an early Enter
# change nothing; Ändern and its undo go through with settingsd's "confirmed"). Idle: no frames. foot (when installed): started with bitscomp's foot.ini, its
# background is the theme's paper, and a mode change switches the running terminal.
# WY_UPDATE_REF=1 writes the references instead of comparing (only after a look change that was meant).
set -euo pipefail

ROOT=${BITSOS_ROOT:-$(cd "$(dirname "$0")/../../.." && pwd)}
RUN=${BITSOS_RUN_DIR:-$(mktemp -d)}
mkdir -p "$RUN"
RUN=$(realpath "$RUN")
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-$ROOT/target}
BIN=$CARGO_TARGET_DIR/release/bitscomp
SD=$CARGO_TARGET_DIR/release/settingsd
REF=$ROOT/host/tests/wy/ref
IMG="python3 -I $ROOT/host/tests/wy/tools/img.py"
export XDG_RUNTIME_DIR=$RUN/xdg
mkdir -m 700 -p "$XDG_RUNTIME_DIR"
unset DISPLAY WAYLAND_DISPLAY
SOCK=wy77-$$
export SETTINGS_SOCKET=$RUN/settings.sock

fail() { echo "FAIL: $*"; exit 1; }

echo "== build bitscomp, settingsd"
(cd "$ROOT/user/services/bitscomp" && cargo build --release -q --offline)
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
reveal() { # hot pattern: the wheel over the (first shown) window's content until a line matches (down, then up)
    local xy
    for steps in $(printf '2 %.0s' $(seq 15)) $(printf -- '-2 %.0s' $(seq 30)); do
        ctl hot | grep -q -- "$1" && return 0
        xy=$(hot '^win ' content)
        ctl pointer $xy >/dev/null
        ctl wheel "$steps" >/dev/null
        sleep 0.03
    done
    fail "no hot line matching '$1', also after scrolling"
}
click() { # hot pattern
    local xy
    reveal "$1"
    xy=$(hot "$1" at)
    ctl click $xy >/dev/null
}
get() { "$SD" get "$1"; }
settings() { "$SD" "$@" >/dev/null; }
wait_for() { # pattern, command..., what
    local pat=$1 what=${!#}
    for _ in $(seq 60); do
        "${@:2:$#-2}" 2>/dev/null | grep -q -- "$pat" && return 0
        sleep 0.05
    done
    fail "$what"
}
wait_get() { # key, value
    for _ in $(seq 60); do
        [ "$(get "$1")" = "$2" ] && return 0
        sleep 0.05
    done
    fail "$1 is $(get "$1"), not $2"
}
shot() { # name (the pointer on the status bar first: no hover in the picture)
    ctl pointer 1990 1110 >/dev/null
    sleep 0.2
    # Every answer is in first (an answer line after the clear would be in the picture).
    for _ in $(seq 100); do ctl page | grep -q "busy=[1-9]" || break; sleep 0.05; done
    ctl page clear >/dev/null
    ctl shot "$RUN/$1.ppm" >/dev/null
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
same() { $IMG diff "$RUN/$1.ppm" "$RUN/$2.ppm" 0 0 >/dev/null || fail "$1 is not the same picture as $2"; }
differs() { ! $IMG diff "$RUN/$1.ppm" "$RUN/$2.ppm" 0 0 >/dev/null || fail "$1 looks like $2"; }
type_keys() { ctl key "$@" >/dev/null; }
start_comp() { # extra args...
    "$BIN" --backend headless --size 2000x1125 --socket "$SOCK" --mcp none "$@" >>"$RUN/bitscomp.log" 2>&1 &
    COMP=$!
    PIDS+=("$COMP")
    for _ in $(seq 50); do [ -S "$XDG_RUNTIME_DIR/$SOCK.ctl" ] && break; sleep 0.1; done
    wait_for "end" ctl list "no control socket"
}

echo "== settingsd on the host"
mkdir -p "$RUN/stick" "$RUN/fallback" "$RUN/sys"
"$SD" serve --stick "$RUN/stick" --fallback "$RUN/fallback" --sys "$RUN/sys" --socket "$SETTINGS_SOCKET" --no-mcp \
    >"$RUN/settingsd.log" 2>&1 &
PIDS+=($!)
for _ in $(seq 50); do [ -S "$SETTINGS_SOCKET" ] && break; sleep 0.1; done
[ -S "$SETTINGS_SOCKET" ] || fail "settingsd did not start"

echo "== bitscomp 2000x1125 at 125 %, the page einstellungen/aussehen as a tab"
start_comp --settings "$SETTINGS_SOCKET" --fixed-status
wait_for "settingsd=true" ctl theme "bitscomp did not read settingsd"
ctl address open einstellungen/aussehen tab >/dev/null
wait_for 'app_id=bitscomp.einstellungen title="Einstellungen"' ctl list "the settings page did not map"
wait_for "address=einstellungen/aussehen prefix=theme .*live=true" ctl page "the page has no state"
ctl hot | grep -q "page pid=.* swatch=#ffb020" || fail "no swatch on the hot lines"
shot settings-dark
compare settings-dark

echo "== light through settingsd: the page follows"
settings set theme.mode light
wait_for "mode=light" ctl theme "theme.mode light did not arrive"
shot settings-light
compare settings-light
settings undo
wait_for "mode=dark" ctl theme "undo did not bring dark back"
shot back
same back settings-dark

echo "== a swatch: theme.accent in settingsd and on the screen; Rückgängig restores the picture"
click 'swatch=#ffb020'
wait_get theme.accent '#ffb020'
wait_for "accent=#ffb020" ctl theme "the accent did not reach the screen"
wait_for 'msg="Akzentfarbe: Geändert"' ctl page "no answer line for the swatch"
shot amber
differs amber settings-dark
click 'page pid=.* undo'
wait_get theme.accent '#00e5ff'
wait_for "accent=#00e5ff" ctl theme "Rückgängig did not restore the accent"
shot undone
same undone settings-dark

echo "== the hex field: a colour typed and Enter; a bad one is refused on the page"
click 'page pid=.* hex'
wait_for 'edit=hex:"#00e5ff"' ctl page "the hex field did not open"
# BackSpace x7, then 3 d d c 9 7 (evdev codes), Enter.
type_keys 14 14 14 14 14 14 14 4 32 32 46 10 8 28
wait_get theme.accent '#3ddc97'
click 'page pid=.* hex'
type_keys 14 14 14 14 14 14 14 44 44 28
wait_for 'msg=error:"Keine gültige Farbe' ctl page "a bad colour gave no error line"
[ "$(get theme.accent)" = '#3ddc97' ] || fail "a bad colour changed the accent"
click 'page pid=.* undo'
wait_get theme.accent '#00e5ff'

echo "== a choice, a switch, the scale"
click 'choice=theme.mode=light'
wait_get theme.mode light
wait_for "mode=light" ctl theme "the choice did not reach the screen"
click 'page pid=.* undo'
wait_get theme.mode dark
click 'switch=theme.layout.statusbar'
wait_get theme.layout.statusbar false
click 'page pid=.* undo'
wait_get theme.layout.statusbar true
click 'more=theme.scale'
wait_get theme.scale 150
wait_for "scale=1.5" ctl theme "the scale did not reach the output"
click 'page pid=.* undo'
wait_get theme.scale 125
wait_for "scale=1.25" ctl theme "undo did not restore the scale"

echo "== favourites: remove, add through the field, one up; each undone"
click 'fav-remove=1'
wait_get desktop.favourites 'programm/terminal'
click 'page pid=.* undo'
wait_get desktop.favourites 'programm/terminal, einstellungen/aussehen'
click 'fav-field'
type_keys 20 18 19 50 23 49 30 38 28   # t e r m i n a l, Enter
wait_get desktop.favourites 'programm/terminal, einstellungen/aussehen, terminal'
click 'fav-up=2'
wait_get desktop.favourites 'programm/terminal, terminal, einstellungen/aussehen'
click 'page pid=.* undo'
click 'page pid=.* undo'
wait_get desktop.favourites 'programm/terminal, einstellungen/aussehen'

echo "== Rückgängig undoes only the page's own changes"
settings set theme.accent '#b388ff'
wait_for "accent=#b388ff" ctl theme "the accent from the socket did not arrive"
click 'page pid=.* undo'
wait_for 'msg=error:"Akzentfarbe: zuletzt geändert von einem anderen Programm' ctl page "Rückgängig did not refuse a change made elsewhere"
[ "$(get theme.accent)" = '#b388ff' ] || fail "Rückgängig undid a change made elsewhere"
settings undo
wait_get theme.accent '#00e5ff'
ctl pointer $(hot '^win ' content) >/dev/null
ctl wheel -40 >/dev/null
wait_for "scroll=0 " ctl page "the wheel did not bring the page's top back"
shot favs-back
same favs-back settings-dark

echo "== idle: the page draws nothing by itself"
ctl stats reset >/dev/null
sleep 2
ctl stats | grep -q "frames=0 " || fail "frames while idle: $(ctl stats)"

echo "== einstellungen/aussehen/akzent: a page with the accent row lit (in view: no scroll)"
ctl address open einstellungen/aussehen/akzent tab >/dev/null
wait_for "address=einstellungen/aussehen/akzent prefix=theme.accent scroll=0 " ctl page "no page for the accent"
ctl hot | grep -q "page pid=.* swatch=#3ddc97" || fail "the accent row is not shown"
wait_for 'visible=true focused=true .*app_id=bitscomp.einstellungen' ctl list "the new page is not the shown tab"
shot settings-target
compare settings-target
echo "== einstellungen/aufbau/statusleiste: the page scrolls to that row, Rückgängig stays in the head"
ctl address open einstellungen/aufbau/statusleiste tab >/dev/null
wait_for "address=einstellungen/aufbau/statusleiste prefix=theme.layout.statusbar scroll=[1-9]" ctl page "the page did not scroll to the status bar row"
sleep 0.2
ctl hot | grep -q "switch=theme.layout.statusbar" || fail "the status bar row is not shown"
ctl hot | grep -q "page pid=.* undo" || fail "Rückgängig scrolled away"
ctl hot | grep -q "swatch=#3ddc97" && fail "the swatches should be scrolled under the head"
echo "== the wheel scrolls the page back up"
xy=$(hot 'switch=theme.layout.statusbar' at)
ctl pointer $xy >/dev/null
# -40 (was -20): INT-W15's theme.cursor.size row made the page one row longer.
ctl wheel -40 >/dev/null
wait_for "address=einstellungen/aufbau/statusleiste .*scroll=0 " ctl page "the wheel did not scroll up"

echo "== WY-9: einstellungen/kernel/snd_output offers the registry's choices, hdmi writes the key, undo"
: >"$RUN/sys/snd_output"
ctl address open einstellungen/kernel/snd_output tab >/dev/null
wait_for "address=einstellungen/kernel/snd_output prefix=kernel.snd_output scroll=[1-9]" ctl page "the page did not scroll to the sound row"
for v in auto jack hdmi usb; do
    wait_for "page pid=.* choice=kernel.snd_output=$v at=" ctl hot "the sound row offers no '$v'"
done
[ "$(ctl hot | grep -c 'choice=kernel.snd_output=')" = 4 ] || fail "the sound row offers more than its four choices"
shot settings-kernel
compare settings-kernel
# INT-W13: the rows next to it depend on the registry (FS-13 added kernel.preload.*): any other kernel row.
ctl hot | grep -v "kernel\.snd_output" | grep -q "=kernel\." || fail "no other kernel row near the sound row"
# INT-W14 (FS-13b): the preload cap takes a number or the word; its pill "alles" sits after + (the shot shows it lit).
ctl hot | grep -q "word=kernel.preload.max_mb at=" || fail "the preload cap row has no 'alles' pill"
click 'choice=kernel.snd_output=hdmi at='
wait_get kernel.snd_output hdmi
wait_for 'msg="Wo der Ton spielt' ctl page "no answer line for the sound choice"
[ "$(cat "$RUN/sys/snd_output")" = hdmi ] || fail "the kernel's file got '$(cat "$RUN/sys/snd_output")', not hdmi"
click 'page pid=.* undo'
wait_get kernel.snd_output auto
[ "$(cat "$RUN/sys/snd_output")" = auto ] || fail "undo did not write auto to the kernel's file"
echo "== WY-9: without the kernel's file (an empty --sys) the choice is still stored"
rm -f "$RUN/sys/snd_output"
click 'choice=kernel.snd_output=jack at='
wait_get kernel.snd_output jack
[ -e "$RUN/sys/snd_output" ] && fail "settingsd created a kernel file that was not there"
click 'page pid=.* undo'
wait_get kernel.snd_output auto
echo "== WY-9: a number row (theme.glow) steps by its own step, a bad typed number is refused"
reveal 'more=theme.glow'
click 'more=theme.glow'
wait_get theme.glow 65
click 'page pid=.* undo'
wait_get theme.glow 60
click 'field=theme.glow'
wait_for "edit=field=theme.glow:\"60\"" ctl page "the number field did not start editing"
type_keys 14 14 6 11 11 28   # Backspace twice, 5 0 0, Enter
wait_for 'msg=error:".*500.*von 0 bis 100 %' ctl page "a number out of range gave no error line"
[ "$(get theme.glow)" = 60 ] || fail "a refused number changed theme.glow"
echo "== WY-9: a generated text row (claude.name, group CLAUDE) is typed and written, undo"
ctl address open einstellungen/claude/name tab >/dev/null
wait_for "address=einstellungen/claude/name prefix=claude.name scroll=[1-9]" ctl page "the page did not scroll to the name row"
click 'field=claude.name'
wait_for 'edit=field=claude.name:"Claude"' ctl page "the text field did not start editing"
type_keys 14 14 14 14 14 14 48 23 20 31 28   # Backspace six times, b i t s, Enter
wait_get claude.name bits
click 'page pid=.* undo'
wait_get claude.name Claude

echo "== WY-10: a confirm key (test.risky, registered like a service does): the page asks before every change"
ctl quit >/dev/null || true
wait "$COMP" 2>/dev/null || true
REG='{"op":"register","owner":"testsvc","keys":[{"key":"test.risky","schema":{"type":"int","min":0,"max":9},"default":1,"text":{"de":"Riskanter Testwert","en":"Risky test value"},"restart":"reboot","safety":"confirm"}]}'
"$SD" raw "$REG" | grep -q '"ok":true' || fail "settingsd did not register test.risky"
start_comp --settings "$SETTINGS_SOCKET" --fixed-status
wait_for "settingsd=true" ctl theme "bitscomp did not read settingsd"
ctl address open einstellungen/test/risky tab >/dev/null
wait_for "address=einstellungen/test/risky prefix=test.risky .*dialog=none" ctl page "the page did not open at the confirm key"
H0=$("$SD" history 1)
unchanged() { # what: test.risky is still 1 and settingsd's history did not move
    [ "$(get test.risky)" = 1 ] || fail "$1 changed test.risky to $(get test.risky)"
    [ "$("$SD" history 1)" = "$H0" ] || fail "$1 reached settingsd: $("$SD" history 1)"
    if grep -q 'asks settingsd: .*"key":"test.risky"' "$RUN/bitscomp.log"; then fail "$1 sent a request for test.risky"; fi
}
ask() { # hot pattern of the control: the dialog opens, focus on Abbrechen, the page under it is off
    click "$1"
    wait_for 'dialog=set:test.risky:focus=cancel ' ctl page "'$1' opened no dialog"
    ctl hot | grep -q "page pid=.* dialog-ok at=" || fail "the dialog has no Ändern on the hot lines"
    ctl hot | grep -q "page pid=.* more=test.risky" && fail "the page under the dialog still takes clicks"
    true
}
ask 'more=test.risky'
unchanged "opening the dialog"
ctl pointer 1990 1110 >/dev/null
sleep 0.2
ctl shot "$RUN/settings-confirm.ppm" >/dev/null
compare settings-confirm
click 'page pid=.* dialog-cancel'
wait_for 'dialog=none msg="Riskanter Testwert: nicht geändert' ctl page "Abbrechen did not close the dialog"
unchanged "Abbrechen"
ask 'more=test.risky'
type_keys 1   # Esc
wait_for 'dialog=none msg="Riskanter Testwert: nicht geändert' ctl page "Esc did not cancel the dialog"
unchanged "Esc"
ask 'more=test.risky'
type_keys 28   # Enter at once: the focus is on Abbrechen
wait_for 'dialog=none msg="Riskanter Testwert: nicht geändert' ctl page "an early Enter did not cancel"
unchanged "an early Enter"
ask 'more=test.risky'
ctl page clear >/dev/null
wait_for 'dialog=none ' ctl page "ctl page clear did not close the dialog"
unchanged "ctl page clear"
ask 'more=test.risky'
click 'page pid=.* dialog-ok'
wait_get test.risky 2
wait_for 'dialog=none msg="Riskanter Testwert: Geändert"' ctl page "no answer line after Ändern"
"$SD" history 1 | grep -q "set test.risky: .* -> 2 by einstellungen" || fail "the change is not the page's: $("$SD" history 1)"
grep -q 'asks settingsd: .*"key":"test.risky","value":2,.*"confirmed":true' "$RUN/bitscomp.log" \
    || fail "Ändern sent no confirmed request: $(grep 'asks settingsd' "$RUN/bitscomp.log" | tail -1)"
echo "== WY-10: a typed value asks too; Tab moves to Ändern, Enter confirms"
click 'field=test.risky'
wait_for 'edit=field=test.risky:"2"' ctl page "the number field did not start editing"
type_keys 14 5 28   # Backspace, 4, Enter
wait_for 'dialog=set:test.risky:focus=cancel ' ctl page "Enter in the field opened no dialog"
[ "$(get test.risky)" = 2 ] || fail "the typed value went through without the dialog"
type_keys 15   # Tab
wait_for 'dialog=set:test.risky:focus=ok ' ctl page "Tab did not move to Ändern"
type_keys 28
wait_get test.risky 4
echo "== WY-10: Rückgängig of a confirm key asks again (Abbrechen keeps it), then undoes twice back to the default"
H4=$("$SD" history 1)
click 'page pid=.* undo'
wait_for 'dialog=undo:test.risky:focus=cancel ' ctl page "undo of a confirm key opened no dialog"
click 'page pid=.* dialog-cancel'
wait_for 'dialog=none ' ctl page "Abbrechen did not close the undo dialog"
[ "$(get test.risky)" = 4 ] || fail "a cancelled undo changed test.risky"
[ "$("$SD" history 1)" = "$H4" ] || fail "a cancelled undo reached settingsd: $("$SD" history 1)"
click 'page pid=.* undo'
wait_for 'dialog=undo:test.risky' ctl page "undo asked no second time"
click 'page pid=.* dialog-ok'
wait_get test.risky 2
wait_for 'dialog=none msg="Riskanter Testwert: Rückgängig gemacht"' ctl page "no answer line after the undo"
click 'page pid=.* undo'
wait_for 'dialog=undo:test.risky' ctl page "the second undo asked nothing"
click 'page pid=.* dialog-ok'
wait_get test.risky 1

echo "== without settingsd the page only shows"
ctl quit >/dev/null || true
wait "$COMP" 2>/dev/null || true
start_comp --settings none --fixed-status
ctl address open einstellungen tab >/dev/null
wait_for 'app_id=bitscomp.einstellungen' ctl list "the page did not map without settingsd"
wait_for "live=false" ctl page "the page claims settingsd"
click 'swatch=#ffb020'
wait_for 'msg=error:' ctl page "a click without settingsd gave no error line"
ctl theme | grep -q "accent=#00e5ff" || fail "the accent changed without settingsd"

if command -v foot >/dev/null; then
    echo "== foot: started with bitscomp's foot.ini, background = paper, the mode switches it while it runs"
    ctl quit >/dev/null || true
    wait "$COMP" 2>/dev/null || true
    start_comp --settings "$SETTINGS_SOCKET" --fixed-status
    wait_for "settingsd=true" ctl theme "bitscomp did not read settingsd"
    INI=$(ctl foot | sed -n 's/.*config=\([^ ]*\) ready=true.*/\1/p')
    [ -n "$INI" ] && [ -f "$INI" ] || fail "no foot.ini: $(ctl foot)"
    grep -q "^initial-color-theme=dark" "$INI" || fail "foot.ini is not dark"
    grep -A1 "^\[colors-dark\]" "$INI" | grep -q "background=131c26" || fail "dark background is not the paper"
    ctl address open programm/terminal tab >/dev/null
    wait_for 'app_id=foot' ctl list "foot did not map"
    FPID=$(ctl list | sed -n 's/.*app_id=foot .*pid=\([0-9]*\).*/\1/p' | head -1)
    tr '\0' ' ' </proc/"$FPID"/cmdline | grep -q -- "--config=$INI" || fail "foot was started without the theme's config"
    sleep 1
    BODY=$(ctl hot | grep '^win ' | head -1 | sed -n 's/.* body=\([0-9]*\),\([0-9]*\),\([0-9]*\),\([0-9]*\).*/\1 \2 \3 \4/p')
    read -r bx by bw bh <<<"$BODY"
    px() { ctl shot "$RUN/foot.ppm" >/dev/null; $IMG pixel "$RUN/foot.ppm" $((bx + bw / 2)) $((by + bh * 3 / 4)); }
    [ "$(px)" = "19 28 38" ] || fail "foot's background is $(px), not the dark paper 19 28 38"
    settings set theme.mode light
    wait_for "mode=light" ctl theme "light did not arrive"
    grep -q "^initial-color-theme=light" "$INI" || fail "foot.ini did not follow the mode"
    for _ in $(seq 40); do [ "$(px)" = "255 255 255" ] && break; sleep 0.05; done
    [ "$(px)" = "255 255 255" ] || fail "the running foot did not switch to light: $(px)"
    settings undo
    wait_for "mode=dark" ctl theme "undo did not bring dark back"
    for _ in $(seq 40); do [ "$(px)" = "19 28 38" ] && break; sleep 0.05; done
    [ "$(px)" = "19 28 38" ] || fail "the running foot did not switch back to dark: $(px)"
    echo "== foot: a mode switch right after a start neither ends the new terminal nor misses it"
    ctl address open programm/terminal tab >/dev/null
    settings set theme.mode light
    for _ in $(seq 100); do [ "$(ctl list | grep -c 'app_id=foot ')" -ge 2 ] && break; sleep 0.05; done
    FPID2=$(ctl list | sed -n 's/.*app_id=foot .*pid=\([0-9]*\).*/\1/p' | grep -vx "$FPID" | head -1)
    [ -n "$FPID2" ] || fail "the second foot did not map: $(ctl list)"
    wait_for "mode=light" ctl theme "light did not arrive"
    for _ in $(seq 40); do [ "$(px)" = "255 255 255" ] && break; sleep 0.05; done
    [ "$(px)" = "255 255 255" ] || fail "the new foot is not light: $(px)"
    kill -0 "$FPID" && kill -0 "$FPID2" || fail "a foot ended at the mode switch"
    settings undo
    wait_for "mode=dark" ctl theme "undo did not bring dark back"
    for _ in $(seq 40); do [ "$(px)" = "19 28 38" ] && break; sleep 0.05; done
    [ "$(px)" = "19 28 38" ] || fail "the new foot did not switch back to dark: $(px)"
    kill -0 "$FPID2" || fail "the new foot ended at the switch back"
else
    echo "SKIP foot part: foot is not installed"
fi

echo "PASS: settings page (references, swatch, hex, choice, switch, scale, favourites, undo, own undo only, jump, schema rows: kernel.snd_output hdmi, number step and range, WY-10 confirm dialog: cancel, Esc, early Enter, page clear, Ändern, typed, undo; offline), foot colours"
