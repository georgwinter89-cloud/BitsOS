#!/usr/bin/env bash
# MC-9b on the host (headless back end, nothing on the screen): bitscomp as the installer's confirmer, against a
# stand-in installer (tools/fake_installer.py: bitsinstall's protocol and decide.rs's rule, since the real one
# needs V2's gate). The question lies over a fullscreen window (frame around the whole screen) and keeps every key
# and click from it; an injected click on "Erlauben" goes out marked injected and is refused; a real one (the
# headless test path `ctl phys`) is taken only once armed; Esc and the dialog time are no; the installer's cancel
# closes it. Strg+Alt+G from injected keys does nothing; from the real keyboard it opens the grant dialog, whose
# grant lets the next request through without a dialog, shows in the bar and runs out by itself; "Alle
# zurücknehmen" sends the revoke. A second ask while one is shown is answered no, a bad line is ignored and the
# longest text (4000 characters, an argument without a break) is drawn without losing the link.
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
SOCK=wy96-$$
ISOCK=$RUN/install.sock
FLOG=$RUN/fake.log
rm -f "$ISOCK" "$FLOG"
: >"$FLOG"

fail() { echo "FAIL: $*"; echo "--- fake installer"; tail -20 "$FLOG"; echo "--- bitscomp"; grep installer "$RUN/bitscomp.log" | tail -20; exit 1; }

echo "== build bitscomp, wyclient"
(cd "$ROOT/user/services/bitscomp" && cargo build --release -q --offline && cargo build --release -q --offline -p wyclient)
# WY-14: WY_GPU=1 runs this scenario with the gpu build (tools/gpu-host.sh). The stand-in installer checks no
# executable; bitsinstall's own check of /proc/PID/exe is MC's (change request WY-14/3).
. "$ROOT/host/tests/wy/tools/gpu-host.sh"

PIDS=()
cleanup() {
    for p in "${PIDS[@]}"; do kill "$p" 2>/dev/null || true; done
    for p in "${PIDS[@]}"; do wait "$p" 2>/dev/null || true; done
}
trap cleanup EXIT

ctl() { "$BIN" ctl --socket "$SOCK" "$@"; }
key() { ctl key "$@" >/dev/null; }
phys() { ctl phys "$@" >/dev/null; }
wait_for() { # pattern, command..., what
    local pat=$1 what=${!#}
    for _ in $(seq 100); do
        "${@:2:$#-2}" 2>/dev/null | grep -q -- "$pat" && return 0
        sleep 0.05
    done
    fail "$what (last: $("${@:2:$#-2}" 2>&1 | tail -1))"
}
gone() { # pattern, what: `ctl install` stops showing it
    for _ in $(seq 100); do ctl install | grep -q -- "$1" || return 0; sleep 0.05; done
    fail "$2"
}
flog() { # pattern, what: the stand-in installer logged it (10 s)
    for _ in $(seq 200); do grep -q -- "$1" "$FLOG" && return 0; sleep 0.05; done
    fail "$2"
}
field() { # name: "X Y" of that button in `ctl install`
    local v
    v=$(ctl install | sed -n "s/.* $1=\([-0-9.]*\),\([-0-9.]*\).*/\1 \2/p")
    [ -n "$v" ] || fail "no '$1' in ctl install: $(ctl install)"
    echo "$v"
}
pixel() { $IMG pixel "$RUN/$1.ppm" "$2" "$3"; }
count() { grep -c -- "$1" "$RUN/Voll.out" || true; }

echo "== bitscomp 2000x1125 (125 %), a fullscreen window"
"${START[@]}" --backend headless --size 2000x1125 --socket "$SOCK" --mcp none --settings none --fixed-status \
    --installer "$ISOCK" >"$RUN/bitscomp.log" 2>&1 &
PIDS+=($!)
gpu_host_check "$RUN/bitscomp.log" "$!"
for _ in $(seq 50); do [ -S "$XDG_RUNTIME_DIR/$SOCK.ctl" ] && break; sleep 0.1; done
wait_for "end" ctl list "no control socket"
ctl install | grep -q "link=down" || fail "a link without an installer: $(ctl install)"
WAYLAND_DISPLAY=$SOCK "$WYC" --title Voll --pattern 0 >"$RUN/Voll.out" 2>&1 &
PIDS+=($!)
wait_for 'title="Voll"' ctl list "the client did not map"
wait_for "^enter keyboard" cat "$RUN/Voll.out" "the client got no keyboard focus"
key 87
wait_for 'fullscreen visible=true focused=true.*title="Voll"' ctl list "F11 did not make the client fullscreen"
sleep 0.3
ctl shot "$RUN/before.ppm" >/dev/null
[ "$(pixel before 3 560)" = "192 48 48" ] || fail "the fullscreen client does not cover the left edge"
ctl phys key 30 >/dev/null 2>&1 || fail "ctl phys is missing in the headless build"
wait_for "^key 30 released" cat "$RUN/Voll.out" "a real key did not reach the client before any dialog"

echo "== the stand-in installer; bitscomp connects by itself"
python3 -I "$ROOT/host/tests/wy/tools/fake_installer.py" "$ISOCK" "$FLOG" >"$RUN/fake.out" 2>&1 &
PIDS+=($!)
flog "confirmer connected" "bitscomp did not connect to the installer"
wait_for "link=up" ctl install "ctl install does not show the link"

echo "== 1: the question lies over the fullscreen window; injected input is refused"
wait_for " ask=1 kind=reboot focus=no" ctl install "no question 1 (focus on Ablehnen)"
sleep 0.3
ctl shot "$RUN/ask.ppm" >/dev/null
left=$(pixel ask 3 560)
[ "$left" != "192 48 48" ] || fail "the frame is not drawn over the fullscreen window at the left edge"
[ "$left" = "$(pixel ask 1996 560)" ] && [ "$left" = "$(pixel ask 1000 1121)" ] || fail "the frame is not the same all around: $left"
[ "$(pixel ask 300 300)" != "192 48 48" ] || fail "the screen is not dimmed over the fullscreen window"
keys=$(count "^key "); buttons=$(count "^button ")
key 15 105 106
ctl install | grep -q " focus=no " || fail "injected Tab/arrows moved the focus: $(ctl install)"
ctl click $(field yes) >/dev/null
flog "decided 1 no by=refused input=injected" "the injected click on Erlauben was not sent as injected"
ctl install log | grep -q "answer 1 Erlauben from injected input" || fail "bitscomp did not log the injected answer"
gone " ask=1 " "question 1 stayed open"

echo "== 2: a real Enter on Erlauben, only once armed"
wait_for " ask=2 " ctl install "no question 2"
phys key 15
ctl install | grep -q " focus=yes " || fail "a real Tab did not move the focus: $(ctl install)"
if ctl install | grep -q "armed=false"; then
    phys key 28
    sleep 0.1
    ctl install | grep -q " ask=2 " || fail "Erlauben answered before it was armed"
    grep -q "decided 2" "$FLOG" && fail "an answer went out before Erlauben was armed"
fi
wait_for "armed=true" ctl install "Erlauben never armed"
phys key 28
flog "decided 2 yes by=dialog" "the real Enter on Erlauben did not confirm"
gone " ask=2 " "question 2 stayed open"

echo "== 3: a real Esc is no; 4: the dialog time is no; 5: the installer's cancel closes it"
wait_for " ask=3 " ctl install "no question 3"
flog 'got {"type":"answer","id":103,"yes":false,"input":"none"}' "a second ask while question 3 was shown was not answered no"
ctl install | grep -q " ask=3 " || fail "the second ask replaced question 3: $(ctl install)"
ctl install log | grep -q "not understood, ignored" || fail "the bad line was not logged"
ctl install log | grep -q "ask 103 while 3 is shown: answered no" || fail "bitscomp did not log the second ask"
phys key 1
flog "decided 3 no by=dialog input=physical" "Esc was not a no"
wait_for " ask=4 " ctl install "no question 4"
ctl shot "$RUN/long.ppm" >/dev/null || fail "no picture of the 4000-character question"
ctl install | grep -q "link=up" || fail "the long question broke the link: $(ctl install)"
flog "decided 4 no by=dialog input=none" "the dialog time did not end question 4 with a no"
wait_for " ask=5 " ctl install "no question 5"
flog "decided 5 no by=timeout" "the stand-in did not cancel question 5"
gone " ask=5 " "the installer's cancel did not close the question"
[ "$(count "^key ")" = "$keys" ] || fail "keys reached the fullscreen window while a question was shown"
[ "$(count "^button ")" = "$buttons" ] || fail "clicks reached the fullscreen window while a question was shown"

echo "== grants: Strg+Alt+G from injected keys does nothing, from the real keyboard opens the dialog"
key 87
for _ in $(seq 60); do ctl list | grep 'title="Voll"' | grep -q fullscreen || break; sleep 0.05; done
ctl list | grep 'title="Voll"' | grep -q fullscreen && fail "F11 did not end the fullscreen (the bar's marker is checked next)"
ctl pointer 1000 600 >/dev/null
sleep 0.3
ctl shot "$RUN/nomarker.ppm" >/dev/null
ctl install test-seconds 4 >/dev/null
key +29 +56 34 -56 -29
sleep 0.1
ctl install | grep -q "grant-dialog" && fail "injected Strg+Alt+G opened the grant dialog"
ctl install log | grep -q "injected Strg+Alt+G ignored" || fail "the injected Strg+Alt+G was not logged"
phys key +29 +56 34 -56 -29
wait_for "grant-dialog kind=install" ctl install "a real Strg+Alt+G did not open the grant dialog"
ctl install | grep -q 'sentence="Claude darf bis [0-9:]* Uhr ohne Rückfrage Kernel und Generationen installieren."' \
    || fail "the grant sentence: $(ctl install)"
key 28 28 28
ctl install | grep -q "focus=Kind" || fail "injected keys moved the grant dialog: $(ctl install)"
ctl shot "$RUN/grant.ppm" >/dev/null
phys key 15 105 105
ctl install | grep -q 'dur="4 Sekunden' || fail "Left did not pick the 4 s test grant: $(ctl install)"
phys key 15 106
ctl install | grep -q "focus=Ok" || fail "Right on Abbrechen did not reach Erlauben: $(ctl install)"
phys key 28
flog "grant install set for" "the grant did not reach the installer"
wait_for 'marker="Claude darf: installieren bis' ctl install "no grant marker"
ctl pointer 1000 600 >/dev/null
sleep 0.2
ctl shot "$RUN/marker.ppm" >/dev/null
$IMG diff "$RUN/marker.ppm" "$RUN/nomarker.ppm" 0 0 >/dev/null && fail "the status bar shows no marker for the grant"
flog "decided 6 yes by=grant" "the grant did not let request 6 through without a dialog"
ctl install log | grep -q "asks 6" && fail "request 6 showed a dialog despite the grant"
for _ in $(seq 160); do ctl install | grep -q "grants=none" && break; sleep 0.05; done
ctl install | grep -q "grants=none" || fail "the grant did not run out in bitscomp: $(ctl install)"
ctl install | grep -q "marker=" && fail "the marker stayed after the grant ran out"
wait_for " ask=7 kind=install" ctl install "after the grant ran out, request 7 was not asked"
phys key 1
flog "decided 7 no by=dialog input=physical" "request 7 was not refused by Esc"

echo "== Alle zurücknehmen: a second grant (1 h), then the grant dialog takes it back"
flog "waiting for grant #2" "the stand-in does not wait for a second grant"
phys key +29 +56 34 -56 -29
wait_for "grant-dialog kind=install" ctl install "the grant dialog did not open again"
phys key 15 15 15
ctl install | grep -q "focus=Ok" || fail "three Tabs without running grants did not reach Erlauben: $(ctl install)"
phys key 28
flog "(grant #2)" "the second grant did not reach the installer"
wait_for 'marker="Claude darf: installieren bis' ctl install "no marker for the second grant"
phys key +29 +56 34 -56 -29
wait_for "grant-dialog kind=install" ctl install "the grant dialog did not open with a grant running"
phys key 15 15 15
ctl install | grep -q "focus=Revoke" || fail "the third Tab with a grant running is not on Alle zurücknehmen: $(ctl install)"
phys key 28
flog 'got {"type":"revoke","kind":"all"}' "Alle zurücknehmen sent no revoke line"
flog "grants revoked" "the stand-in kept the grant"
wait_for "grants=none" ctl install "the revoked grant is still shown"
ctl install | grep -q "marker=" && fail "the marker stayed after Alle zurücknehmen"
flog "DONE" "the stand-in did not finish"

echo "== after the overlay: keys reach the window again; the installer gone shows as link=down"
gone " ask=" "a question stayed open"
n=$(count "^key 30 pressed")
phys key 30
for _ in $(seq 40); do [ "$(count "^key 30 pressed")" -gt "$n" ] && break; sleep 0.05; done
[ "$(count "^key 30 pressed")" -gt "$n" ] || fail "the window gets no keys after the overlay"
kill "${PIDS[2]}" 2>/dev/null || true
wait_for "link=down" ctl install "the installer's end was not noticed"
echo "OK: 96-install"
