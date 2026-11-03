#!/bin/bash
# slow
# MC-11: the desk's tools after MC-10 went live, under mcp=service with the real confirmer setting
# (mcp_confirmer=bitscomp) and no desktop: what the Mini-PC is after a cold start until Georg types `bitscomp`.
#   boot 1: the pairing is asked on the text console (the same title and text as bitscomp's dialog, Erlauben
#     = typing the dialog's random code, armed 2 s after the dialog and after every earlier key, only the real
#     keyboard: QEMU's USB keyboard through QMP); N refuses, the code typed early does not count, a lone J
#     does nothing, the code after the arm pairs; request_access is asked there too (1 = 1 h), the code
#     grants; push.mjs --tls puts a package on the stick with the grant (no question); a reboot over TLS.
#   boot 2 (the same stick): access.mjs status right after bitsmcp serves, before bitsinstall has reported:
#     it waits while the answer is "grants not loaded yet" and then says full access (exit 0); the kernel took
#     the first table (access.loaded); the code injected by MCP send_keys and the code written into the real
#     keyboard's evdev node by a program both neither confirm nor refuse, a real N then refuses.
set -eu -o pipefail
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/mc-98-console}"
mkdir -p "$RUN_DIR"
RUN_DIR="$(realpath "$RUN_DIR")"
HERE="$(pwd)/host/tests/mcp"
# shellcheck source=host/tests/mcp/lib.bash
. "$HERE/lib.bash"
LO="${BITSOS_PORT_LO:-25000}"
KFWD=$((LO + 190))
export BITSOS_PUSH_PORTS="$((LO + 491))-$((LO + 495))"
TOKEN="mc11-scenario-token-7c2e"
NAME=mc-console
command -v node > /dev/null || { echo "RED: node is needed (host/mcp's tools)"; exit 1; }

QPID=""
BG=""
cleanup() {
    if [ -n "$BG" ]; then kill "$BG" 2>/dev/null || true; fi
    if [ -n "$QPID" ]; then stop_run "$QPID" "$NAME"; fi
}
trap cleanup EXIT
red() { echo "RED: $*"; [ -n "${LOG:-}" ] && grep -a "bitsmcp: \|bitsinstall: \|mcp access\|BitsOS fragt" "$LOG" | tail -20; exit 1; }
ok() { echo "ok: $*"; }

IMG="target/xtask/$NAME.img"
STORE="$RUN_DIR/store"
rm -rf "$STORE" "$RUN_DIR"/*.serial
export BITSOS_TOKEN="$TOKEN"
tcall() { node host/mcp/mcp-call.mjs "127.0.0.1:$KFWD" "$@" --tls "$STORE"; }
access() { node host/mcp/access.mjs "127.0.0.1:$KFWD" "$@" --tls "$STORE"; }
pair() { node host/mcp/pair.mjs "127.0.0.1:$KFWD" --store "$STORE" --expect "$FP"; }
typed() { python3 -I "$HERE/qmp_type.py" "$QMP" "$1"; }
code() { # the Erlauben code the console dialog shows now, in lower case
    grep -ao "tippe den Code  [A-Z ]*  (" "$LOG" | tail -1 | sed -e 's/tippe den Code  //' -e 's/  (//' -e 's/ //g' | tr 'A-Z' 'a-z'
}

cargo xtask image --name "$NAME" --config-extra "token=$TOKEN;mcp_port=8765;mcp=service;loglevel=info;data_img_mb=64;stick_write=on" | tail -1
kib=$(( $(du -sk "target/xtask/esp-$NAME" | cut -f1) + 700 * 1024 ))
rm -f "$IMG"
mkfs.fat -F 32 -n BITSOS -C "$IMG" "$kib" > /dev/null
mcopy -s -i "$IMG" "target/xtask/esp-$NAME/EFI" "target/xtask/esp-$NAME/BITSOS2" ::/
mcopy -n -i "$IMG" ::/BITSOS2/CONFIG.TXT "$RUN_DIR/CONFIG.TXT"
grep -q "^mcp=service" "$RUN_DIR/CONFIG.TXT" || red "the image is not mcp=service"
grep -q "^mcp_confirmer=test" "$RUN_DIR/CONFIG.TXT" && red "the image has the test confirmer"

start() { # $1 = boot number, $2 = wait for bitsinstall too (1) or only for bitsmcp (0)
    stop_qemu "$NAME"
    LOG="$RUN_DIR/boot$1.serial"
    QMP="$RUN_DIR/boot$1.qmp"
    rm -f "$LOG" "$QMP"
    cargo xtask qemu --name "$NAME" --net igb --hostfwd "$KFWD:8765" --timeout 600 --serial-log "$LOG" \
        -- -machine i8042=off -device qemu-xhci,id=xb -device usb-kbd,bus=xb.0,port=1,id=kbd1 -qmp "unix:$QMP,server=on,wait=off" > "$RUN_DIR/boot$1.qemu" 2>&1 &
    QPID=$!
    for _ in $(seq 600); do
        grep -asq "bitsmcp: serving MCP on port 8765" "$LOG" && { [ "$2" = 0 ] || grep -asq "bitsinstall: serving installer requests" "$LOG"; } && break
        sleep 0.25
    done
    grep -asq "bitsmcp: serving MCP on port 8765" "$LOG" || red "boot $1: bitsmcp did not serve"
    LINE=$(grep -a "bitsmcp: TLS server key" "$LOG" | tail -1)
    FP=$(echo "$LINE" | grep -o "TLS server key sha256 [0-9a-f]\{64\}" | awk '{print $5}')
    [ -n "$FP" ] || red "boot $1: no TLS on the start line: $LINE"
}
wait_log() { # pattern, what, seconds
    for _ in $(seq "${3:-30}"); do grep -aq -- "$1" "$LOG" && return 0; sleep 1; done
    red "$2: no /$1/ in the serial log"
}
count() { grep -ac -- "$1" "$LOG" || true; }
wait_count() { # pattern, more than, what
    for _ in $(seq 60); do [ "$(count "$1")" -gt "$2" ] && return 0; sleep 0.5; done
    red "$3: no new /$1/ in the serial log"
}
bg() { # runs "$@" in the background, its output in $RUN_DIR/bg.out
    "$@" > "$RUN_DIR/bg.out" 2>&1 &
    BG=$!
}
bg_end() { # -> exit code of the background command (at most 60 s)
    local rc=0
    for _ in $(seq 120); do kill -0 "$BG" 2>/dev/null || break; sleep 0.5; done
    kill -0 "$BG" 2>/dev/null && red "the background command still runs: $(cat "$RUN_DIR/bg.out")"
    wait "$BG" || rc=$?
    BG=""
    return "$rc"
}

# --- boot 1: no desktop: the text console asks -------------------------------------------------------------
start 1 1
wait_log "init: started the console shell" "boot 1: the console shell" 60
asks=$(count "BitsOS fragt (Frage")
bg pair
wait_count "BitsOS fragt (Frage" "$asks" "boot 1: the pairing dialog on the text console"
wait_log "Neuen Gaming-PC koppeln" "boot 1: the pairing dialog's title"
wait_log "no confirmer connected: asking at the text console" "boot 1: bitsinstall says where it asks"
wait_log "text console: keyboard .*(real, written events filtered, grabbed for the dialog)" "boot 1: the USB keyboard is taken as real, filtered and grabbed"
typed 'n'
rc=0; bg_end || rc=$?
[ "$rc" = 1 ] && grep -q "said no" "$RUN_DIR/bg.out" || red "boot 1: N at the console: exit $rc: $(cat "$RUN_DIR/bg.out")"
wait_log "Ablehnen at the text console (physical)" "boot 1: the refusal was journaled"
ok "boot 1: the pairing asked on the text console; N refused"
rm -rf "$STORE"
asks=$(count "tippe den Code")
bg pair
wait_count "tippe den Code" "$asks" "boot 1: the second pairing dialog"
c=$(code)
[ "${#c}" = 4 ] || red "boot 1: no code on the dialog: '$c'"
typed "$c"
wait_log "Noch nicht scharf" "boot 1: the code typed early is not taken"
sleep 0.5
kill -0 "$BG" 2>/dev/null || red "boot 1: the early code answered: $(cat "$RUN_DIR/bg.out")"
sleep 2.5
typed 'j'
sleep 1
kill -0 "$BG" 2>/dev/null || red "boot 1: a lone J answered: $(cat "$RUN_DIR/bg.out")"
sleep 2.2
[ "$(code)" = "$c" ] || red "boot 1: the code changed within one dialog: $c -> $(code)"
typed "$c"
bg_end || red "boot 1: the code after the arm: $(cat "$RUN_DIR/bg.out")"
wait_log "bitsinstall: pairing .* confirmed at the keyboard: it may get full access" "boot 1: the confirmed pairing was recorded"
out=$(tcall hw_info '{"section":"cpu"}') && echo "$out" | grep -q "cpu vendor=" || red "boot 1: hw_info over TLS: $out"
ok "boot 1: the code typed early did not count, a lone J did nothing, the code after the arm paired; TLS works"
asks=$(count "BitsOS fragt (Frage")
bg access request --hours 8 --reason "QEMU scenario 98"
wait_count "BitsOS fragt (Frage" "$asks" "boot 1: the access dialog on the text console"
wait_log "Dauer: 1 h  8 h  \[24 h\]  7 Tage" "boot 1: the access dialog's choices (24 h preselected)"
typed '1'
wait_log "Dauer: \[1 h\]" "boot 1: 1 chose 1 h"
sleep 2.3
c=$(code)
typed "$c"
bg_end || red "boot 1: access at the console: $(cat "$RUN_DIR/bg.out")"
grep -q "full access for this client until" "$RUN_DIR/bg.out" || red "boot 1: access answer: $(cat "$RUN_DIR/bg.out")"
wait_log "Erlauben at the text console (physical, code typed), 1 h, NVMe false" "boot 1: the access choice was journaled"
wait_log "mcp access.live client=" "boot 1: the kernel took the grant"
ok "boot 1: request_access granted at the text console ($(tail -1 "$RUN_DIR/bg.out"))"
# push.mjs over TLS under mcp=service with the grant: no question, the package on the stick.
T="$RUN_DIR/mc11pkg"
rm -rf "$T"
mkdir -p "$T/sub"
echo "mc11 push over tls" > "$T/sub/a.txt"
head -c 300000 /dev/urandom > "$T/b.bin"
asks=$(count "BitsOS fragt (Frage")
out=$(node host/mcp/push.mjs "127.0.0.1:$KFWD" --tree "$T" --bind 127.0.0.1 --tls "$STORE" 2>&1) || red "push.mjs --tls: $out"
[ "$(count "BitsOS fragt (Frage")" = "$asks" ] || red "the push was asked at the console although granted"
out=$(tcall read_file '{"path":"/boot/BITSOS2/mc11pkg/sub/a.txt","encoding":"text"}') && echo "$out" | grep -q "mc11 push over tls" || red "the pushed file: $out"
ok "push.mjs --tls under mcp=service with the grant: the package is on the stick"
out=$(tcall reboot '{}') || red "reboot: $out"
for _ in $(seq 90); do kill -0 "$QPID" 2>/dev/null || break; sleep 1; done
kill -0 "$QPID" 2>/dev/null && red "QEMU still runs after the reboot"
wait "$QPID" 2>/dev/null || true
QPID=""
ok "boot 1 ended with the reboot over TLS (no question)"

# --- boot 2: the grant at a cold start; an injected key on a console dialog ------------------------------
start 2 0
early=$(grep -ac "bitsinstall: serving installer requests" "$LOG" || true)
rc=0; out=$(access status 2>&1) || rc=$?
[ "$rc" = 0 ] && echo "$out" | grep -q "full access for this client until" || red "boot 2: access status at the start: exit $rc: $out"
if echo "$out" | grep -q "grants not loaded yet"; then how="it waited while the grants were not loaded"; else how="the grants were already loaded"; fi
ok "boot 2: access status right after bitsmcp served (bitsinstall reported: $early): full access; $how"
wait_log "mcp access.loaded" "boot 2: the kernel noted the installer's first table"
out=$(tcall run '{"path":"/bin/sh","args":["-c","echo cold-start-run"]}') && echo "$out" | grep -q "cold-start-run" || red "boot 2: run with the kept grant: $out"
wait_log "init: started the console shell" "boot 2: the console shell" 60
asks=$(count "BitsOS fragt (Frage")
bg access request --hours 1
wait_count "BitsOS fragt (Frage" "$asks" "boot 2: the access dialog on the text console"
sleep 2.5
c=$(code)
[ "${#c}" = 4 ] || red "boot 2: no code on the dialog: '$c'"
out=$(tcall send_keys "{\"text\":\"$c\"}" 2>&1) || red "boot 2: send_keys: $out"
sleep 1.5
kill -0 "$BG" 2>/dev/null || red "boot 2: the code injected by send_keys answered the console dialog: $(cat "$RUN_DIR/bg.out")"
if grep -aq "injected key .* at the text console ignored" "$LOG"; then inj="logged as ignored"; else inj="never reached the dialog's keyboards"; fi
# The code written into every evdev node by a root program (crates/console evdevfs Device::write: injected):
# the dialog's reader of the real keyboard has EVIOC_BITSOS_PHYSICAL, so none of it arrives.
wait_log "evdev.physical_only" "boot 2: bitsinstall set the physical-only filter on the real keyboard" 5
keys=$(python3 -I - "$c" <<'PY'
import base64, struct, sys
codes = {'a': 30, 'b': 48, 'c': 46, 'd': 32, 'e': 18, 'f': 33, 'g': 34, 'h': 35, 'k': 37, 'l': 38, 'm': 50,
         'q': 16, 'r': 19, 's': 31, 't': 20, 'u': 22, 'v': 47, 'w': 17, 'x': 45}
ev = lambda t, c, v: bytes(16) + struct.pack('<HHi', t, c, v)
out = b''.join(ev(1, codes[ch], 1) + ev(0, 0, 0) + ev(1, codes[ch], 0) + ev(0, 0, 0) for ch in sys.argv[1])
print(base64.b64encode(out).decode())
PY
)
out=$(tcall write_file "{\"path\":\"/boot/BITSOS2/mc11pkg/keys.bin\",\"data\":\"$keys\"}") || red "boot 2: write_file keys.bin: $out"
out=$(tcall run '{"path":"/bin/sh","args":["-c","n=0; for f in /dev/input/event*; do cat /boot/BITSOS2/mc11pkg/keys.bin > \"$f\" 2>/dev/null && n=$((n+1)); done; echo written=$n"]}') || red "boot 2: writing into the evdev nodes: $out"
echo "$out" | grep -q "written=[1-9]" || red "boot 2: no evdev node took the written keys: $out"
wn=$(echo "$out" | grep -o "written=[0-9]*")
sleep 1.5
kill -0 "$BG" 2>/dev/null || red "boot 2: the code written into the evdev nodes answered the console dialog: $(cat "$RUN_DIR/bg.out")"
typed 'n'
rc=0; bg_end || rc=$?
[ "$rc" = 1 ] && grep -q "said no" "$RUN_DIR/bg.out" || red "boot 2: N after the injected J: exit $rc: $(cat "$RUN_DIR/bg.out")"
out=$(access status) || red "boot 2: the refused request ended the running grant: $out"
ok "boot 2: the code injected by send_keys ($inj) and written into the evdev nodes ($wn) did not answer; the real N refused; the running grant stays"
for f in "$RUN_DIR"/boot*.serial; do
    if grep -aq "$TOKEN" "$f"; then red "the token appears in $f (R8i)"; fi
done
stop_run "$QPID" "$NAME"
QPID=""
echo "mc 98-console: green (pairing and full access asked on the text console with the real keyboard only, the typed code and the 2 s arm, push over TLS with the grant, the grant at a cold start with the wait for the installer, injected keys ignored)"
