#!/usr/bin/env bash
# PK-L1c on the host (headless back end at 2560x1440, nothing on the screen): the gamepad in the program library.
# The headless back end has no evdev, so a test DragonRise (0079:0006, its buttons BTN_TRIGGER..BTN_BASE6 and a hat,
# as the kernel's fake reports them) is fed through `ctl phys pad-event` into the same state machine and routing
# that bitscomp's evdev reader uses (refused without `phys`). Select+Start held 500 ms (Home) opens the library,
# the hat moves the focus to the fake Writer, b2 (A) starts it, B closes, Y focuses the search, X opens the
# circle menu; a minimised Writer comes back by A instead of a second start; on the installer's question (the
# stand-in installer) a pad press only refuses. Nothing big runs:
# Writer's launcher maps a wyclient window with LibreOffice's app_id.
set -euo pipefail

ROOT=${BITSOS_ROOT:-$(cd "$(dirname "$0")/../../.." && pwd)}
RUN=${BITSOS_RUN_DIR:-$(mktemp -d)}
mkdir -p "$RUN"
RUN=$(realpath "$RUN")
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-$ROOT/target}
BIN=$CARGO_TARGET_DIR/release/bitscomp
WYC=$CARGO_TARGET_DIR/release/wyclient
export XDG_RUNTIME_DIR=$RUN/xdg
rm -rf "$XDG_RUNTIME_DIR"
mkdir -m 700 -p "$XDG_RUNTIME_DIR"
unset DISPLAY WAYLAND_DISPLAY
SOCK=wy53-$$
ISOCK=$RUN/install.sock
FLOG=$RUN/fake.log
rm -f "$ISOCK" "$FLOG"
: >"$FLOG"

fail() { echo "FAIL: $*"; echo "--- bitscomp"; grep -E "pad|library|installer" "$RUN/bitscomp.log" | tail -20; exit 1; }

echo "== build bitscomp, wyclient"
(cd "$ROOT/user/services/bitscomp" && cargo build --release -q --offline && cargo build --release -q --offline -p wyclient)

FAKE=$RUN/stick-bin
rm -rf "$FAKE" "$RUN"/starts-*
mkdir -p "$FAKE"
cat > "$FAKE/libreoffice" <<EOF
#!/bin/sh
echo "\$*" >> "$RUN/starts-libreoffice"
exec "$WYC" --title Writer --app-id libreoffice-writer
EOF
for l in brave steam vkquake cobalt; do
    printf '#!/bin/sh\necho "fake %s $*"\n' "$l" > "$FAKE/$l"
done
chmod 755 "$FAKE"/*
cp "$ROOT"/user/ports/{brave-launcher,libreoffice-launcher,steam,vkquake,cobalt-launcher}/bitsos/*.desktop "$FAKE/"

PIDS=()
cleanup() {
    for p in "${PIDS[@]}"; do kill "$p" 2>/dev/null || true; done
    for p in "${PIDS[@]}"; do wait "$p" 2>/dev/null || true; done
}
trap cleanup EXIT

ctl() { "$BIN" ctl --socket "$SOCK" "$@"; }
# A pad event through the headless test path; an "error:" answer fails the run.
pad() { local r; r=$(ctl phys pad-event "$@"); case "$r" in error:*) fail "pad-event $*: $r" ;; esac; }
# A button pressed and let go (codes of the DragonRise: b0 0x120 = Y, b1 = B, b2 = A, b3 = X, b8 Select, b9 Start).
tap() { pad key "$1" 1; pad key "$1" 0; }
# The hat: x or y to -1/1 and back (quicker than the 400 ms repeat).
hat() { pad abs "$1" "$2"; pad abs "$1" 0; }
home() { pad key 0x128 1 key 0x129 1; sleep 0.7; pad key 0x128 0 key 0x129 0; }
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
focus() { state | sed -n 's/^library .* focus=\([^ ]*\) .*/\1/p'; }
starts() { [ -f "$RUN/starts-$1" ] && grep -c . "$RUN/starts-$1" || echo 0; }

echo "== bitscomp headless 2560x1440 with the fake stick bin"
BITSCOMP_LIBRARY_BIN=$FAKE BITSCOMP_LIBRARY_APPS=$RUN/apps \
    "$BIN" --backend headless --size 2560x1440 --socket "$SOCK" --fixed-status --mcp none --settings none \
    --installer "$ISOCK" >"$RUN/bitscomp.log" 2>&1 &
PIDS+=($!)
wait_for "^writer	" "no Writer in the index" ctl library

echo "== the test pad: only through phys; table-mapped"
ctl pad-event plug dragonrise | grep -q "^error: pad-event: a test command" || fail "pad-event worked without phys"
pad plug dragonrise
ctl pad | grep -q '^pad [0-9]* path=test:dragonrise .* id=0079:0006 mapping=table home="Select+Start 500 ms" origin=physical' || fail "ctl pad: $(ctl pad)"
grep -q 'bitscomp: input: pad test:dragonrise .*: mapping table, Home Select+Start 500 ms' "$RUN/bitscomp.log" || fail "no event line for the pad"

echo "== Select+Start held is Home: it opens the library; the hat moves to Writer, A starts it"
pad key 0x128 1 key 0x129 1
sleep 0.2
state | grep -q "^library open=false" || fail "Select+Start opened it before 500 ms"
sleep 0.5
pad key 0x128 0 key 0x129 0
state | grep -q "^library open=true" || fail "Select+Start held did not open the library: $(state | head -2)"
grep -q "bitscomp: library: opened (pad Home)" "$RUN/bitscomp.log" || fail "no event line for the pad's Home"
ctl pad | grep -q "last=Home@" || fail "ctl pad does not show the last press: $(ctl pad)"
first=$(focus)
hat 0x11 1
[ "$(focus)" != "$first" ] || fail "hat down did not move the focus from $first"
for _ in $(seq 20); do
    [ "$(focus)" = writer ] && break
    hat 0x10 1
done
[ "$(focus)" = writer ] || fail "the hat did not reach Writer: $(state | head -3)"
tap 0x122
wait_for 'app_id=libreoffice-writer title="Writer"' "A did not start Writer" ctl list
state | grep -q "^library open=false" || fail "the start did not close the library"
[ "$(starts libreoffice)" = 1 ] || fail "LibreOffice started $(starts libreoffice) times"

echo "== the launcher rule by pad: Writer minimised, Home, A on Writer restores it instead of a second start"
id=$(ctl list | grep 'title="Writer"' | sed -n 's/.* id=\([0-9]*\) .*/\1/p')
[ -n "$id" ] || fail "no Writer id: $(ctl list)"
ctl minimize "$id" >/dev/null
wait_for '^minimized=0 .*title="Writer"' "Writer did not minimise" ctl list
home
state | grep -q "^library open=true" || fail "Home did not open the library"
for _ in $(seq 20); do
    [ "$(focus)" = writer ] && break
    hat 0x10 1
done
[ "$(focus)" = writer ] || fail "the hat did not reach Writer again: $(state | head -3)"
tap 0x122
for _ in $(seq 50); do ctl list | grep -q '^minimized=.*title="Writer"' || break; sleep 0.1; done
ctl list | grep -q '^minimized=.*title="Writer"' && fail "Writer is still minimised"
grep -q "bitscomp: library: writer restored" "$RUN/bitscomp.log" || fail "no event line for the restore"
sleep 0.5
[ "$(starts libreoffice)" = 1 ] || fail "a second Writer started instead of the restore ($(starts libreoffice) starts)"
[ "$(ctl list | grep -c 'title="Writer"')" = 1 ] || fail "Writer windows: $(ctl list | grep -c 'title="Writer"')"

echo "== B closes; Y focuses the search, B leaves it; X opens the circle menu, B closes it"
home
state | grep -q "^library open=true" || fail "Home over Writer did not open the library"
tap 0x121
state | grep -q "^library open=false" || fail "B did not close the library"
grep -q "bitscomp: library: closed (pad B)" "$RUN/bitscomp.log" || fail "no event line for B"
home
tap 0x120
state | grep -q "^library open=true .* search_focus=true " || fail "Y did not focus the search: $(state | head -1)"
tap 0x121
state | grep -q "^library open=true .* search_focus=false " || fail "B did not leave the search: $(state | head -1)"
tap 0x123
grep -q "bitscomp: library: circle menu of .* (pad X)" "$RUN/bitscomp.log" || fail "X did not open the circle menu"
tap 0x121
state | grep -q "^library open=true" || fail "B on the circle menu closed the library too"
tap 0x121
state | grep -q "^library open=false" || fail "B did not close the library after the menu"

echo "== the installer's question: a pad press only refuses"
python3 -I "$ROOT/host/tests/wy/tools/fake_installer.py" "$ISOCK" "$FLOG" >"$RUN/fake.out" 2>&1 &
PIDS+=($!)
wait_for " ask=1 kind=reboot focus=no" "no question 1" ctl install
tap 0x122
hat 0x10 1
tap 0x123
home
ctl install | grep -q " ask=1 kind=reboot focus=no" || fail "a pad press moved or answered the question: $(ctl install)"
state | grep -q "^library open=false" || fail "Home opened the library over the question"
grep -q "decided 1" "$FLOG" && fail "a pad press answered the question: $(grep decided "$FLOG")"
ctl install log | grep -q "pad A on the dialog ignored" || fail "no line for the ignored A"
tap 0x121
for _ in $(seq 100); do grep -q "decided 1" "$FLOG" && break; sleep 0.05; done
grep -q "decided 1 no by=dialog input=injected" "$FLOG" || fail "B was not an injected no: $(grep decided "$FLOG" || echo none)"

echo "== unplugged: the pad is gone"
pad unplug
ctl pad | grep -q "^pads 0" || fail "the pad stayed: $(ctl pad)"
grep -q "bitscomp: input: pad test:dragonrise went away" "$RUN/bitscomp.log" || fail "no event line for the unplug"

echo "PASS 53-library-pad"
