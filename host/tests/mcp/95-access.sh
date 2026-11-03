#!/bin/bash
# slow
# MC-10: the developer access. The Gaming-PC's client key (host/mcp/pair.mjs) proves itself in every TLS
# connection; full access for that key is granted only by the confirmer (here bitsinstall's test confirmer,
# CONFIG.TXT mcp_confirmer=test, standing in for Georg at the keyboard) and kept on /data.
#   boot 1 (mcp=both: the kernel's plain server on 8765 is the scenario's harness, the service on 8766): the
#     pairing by the token alone (MC-8b's rule); without a grant `run` is refused and ktest runs Safe tests only;
#     request_access from that client is refused before any dialog: it was not paired with Georg's yes at the
#     keyboard (a leaked token never leads to full access).
#   boot 2 (a new stick, mcp=service: what the Mini-PC will run): a token over plain HTTP opens nothing (403); a
#     pairing without a confirmer is refused, the test confirmer (typed into the root console) says no, then yes:
#     only then paired, TLS only; request_access -> the test confirmer grants 1 h with the NVMe tick -> run,
#     write_file/read_file, screen, send_keys, a QemuOnly ktest and a reboot dry run work over TLS without a
#     question; the grant's store is out of the file tools' reach; the NVMe run-time gate opened; a second,
#     unpaired client key gets nothing, the token alone nothing; a Reload over TLS keeps the grant (the new
#     kernel takes the table again, the NVMe gate opens again); a real reboot over TLS (no question) ends the boot.
#   boot 3 (the same stick, the grants handed to the kernel 15 s late: mcp_test_grants_delay_s): access_status
#     says "grants not loaded yet" before the installer reported, access.mjs waits through it (MC-11);
#     the grant survived the reboot (access_status, `run` works); the test confirmer,
#     started through the granted `run`, revokes at the next request_access; afterwards `run` is refused again
#     and the NVMe gate closed; plain HTTP gets nothing.
set -eu -o pipefail
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/mc-95-access}"
mkdir -p "$RUN_DIR"
RUN_DIR="$(realpath "$RUN_DIR")"
HERE="$(pwd)/host/tests/mcp"
# shellcheck source=host/tests/mcp/lib.bash
. "$HERE/lib.bash"
LO="${BITSOS_PORT_LO:-25000}"
KFWD=$((LO + 130))
SFWD=$((LO + 131))
HTTP=$((LO + 132))
TOKEN="mc10-scenario-token-4b19"
NAME=mc-access
command -v node > /dev/null || { echo "RED: node is needed (host/mcp's tools)"; exit 1; }
grep -q "fn bitsinstall" user/init/src/main.rs || { echo "RED: user/init lacks the bitsmcp/bitsinstall glue"; exit 1; }

QPID=""
cleanup() { if [ -n "$QPID" ]; then stop_run "$QPID" "$NAME"; fi; }
trap cleanup EXIT
red() { echo "RED: $*"; [ -n "${LOG:-}" ] && grep -a "bitsmcp: \|bitsinstall: \|mcp access\|nvme.runtime" "$LOG" | tail -20; exit 1; }
ok() { echo "ok: $*"; }

IMG="target/xtask/$NAME.img"
STORE="$RUN_DIR/store"
OTHER="$RUN_DIR/other"
rm -rf "$STORE" "$OTHER" "$RUN_DIR"/*.serial
export BITSOS_TOKEN="$TOKEN"
call() { node host/mcp/mcp-call.mjs "$@"; }
tcall() { node host/mcp/mcp-call.mjs "127.0.0.1:$PORT" "$@" --tls "$STORE"; }
access() { node host/mcp/access.mjs "127.0.0.1:$PORT" "$@" --tls "$STORE"; }

cargo xtask image --name "$NAME" --config-extra "token=$TOKEN;mcp_port=8765;mcp=both;mcp_confirmer=test;loglevel=info;data_img_mb=64;stick_write=on" | tail -1
stick() { # a new stick from the image's files (room for /data's image)
    local kib
    kib=$(( $(du -sk "target/xtask/esp-$NAME" | cut -f1) + 700 * 1024 ))
    rm -f "$IMG"
    mkfs.fat -F 32 -n BITSOS -C "$IMG" "$kib" > /dev/null
    mcopy -s -i "$IMG" "target/xtask/esp-$NAME/EFI" "target/xtask/esp-$NAME/BITSOS2" ::/
}
mode() { # CONFIG.TXT's mcp= on the stick image (offline: the guarded key is Georg's, written on "the Gaming-PC")
    mcopy -n -i "$IMG" ::/BITSOS2/CONFIG.TXT "$RUN_DIR/CONFIG.TXT"
    sed -i "s/^mcp=.*/mcp=$1/" "$RUN_DIR/CONFIG.TXT"
    mcopy -o -i "$IMG" "$RUN_DIR/CONFIG.TXT" ::/BITSOS2/CONFIG.TXT
}

start() { # $1 = boot number, $2 = the service's guest port
    stop_qemu "$NAME"
    LOG="$RUN_DIR/boot$1.serial"
    QMP="$RUN_DIR/boot$1.qmp"
    rm -f "$LOG" "$QMP"
    cargo xtask qemu --name "$NAME" --net igb --hostfwd "$KFWD:8765" --hostfwd "$SFWD:8766" --timeout 600 --serial-log "$LOG" \
        -- -machine i8042=off -device qemu-xhci,id=xb -device usb-kbd,bus=xb.0,port=1,id=kbd1 -qmp "unix:$QMP,server=on,wait=off" > "$RUN_DIR/boot$1.qemu" 2>&1 &
    QPID=$!
    for _ in $(seq 150); do
        grep -asq "bitsmcp: serving MCP on port $2" "$LOG" && grep -asq "bitsinstall: serving installer requests" "$LOG" && break
        sleep 1
    done
    grep -asq "bitsmcp: serving MCP on port $2" "$LOG" || red "boot $1: bitsmcp did not serve on $2"
    grep -asq "bitsinstall: serving installer requests" "$LOG" || red "boot $1: bitsinstall did not start"
    LINE=$(grep -a "bitsmcp: TLS server key" "$LOG" | tail -1)
    FP=$(echo "$LINE" | grep -o "TLS server key sha256 [0-9a-f]\{64\}" | awk '{print $5}')
    [ -n "$FP" ] || red "boot $1: no TLS on the start line: $LINE"
    echo "boot $1: $(echo "$LINE" | sed 's/.*bitsmcp: //')"
}
wait_log() { # pattern, what, seconds
    for _ in $(seq "${3:-30}"); do grep -aq -- "$1" "$LOG" && return 0; sleep 1; done
    red "$2: no /$1/ in the serial log"
}
gone() { # waits for QEMU to end (the reset of a confirmed reboot; -no-reboot)
    for _ in $(seq 90); do kill -0 "$QPID" 2>/dev/null || break; sleep 1; done
    kill -0 "$QPID" 2>/dev/null && red "QEMU still runs after the reboot"
    wait "$QPID" 2>/dev/null || true
    QPID=""
}

# --- boot 1: mcp=both, a pairing by the token alone: never full access ------------------------------------
stick
start 1 8766
PORT=$SFWD
out=$(node host/mcp/pair.mjs "127.0.0.1:$SFWD" --store "$STORE" --expect "$FP" 2>&1) || red "pair.mjs: $out"
ok "paired by the token alone ($(echo "$out" | grep -o 'over TLS.*'))"
CN=$(node -e "const c=require('node:crypto'),f=require('node:fs');console.log(new c.X509Certificate(f.readFileSync(process.argv[1])).subject)" "$STORE/client.crt")
echo "$CN" | grep -q "^CN=bitsos-desk " || red "the client certificate names no host: $CN"
rc=0; out=$(tcall run '{"path":"/bin/true"}' 2>&1) || rc=$?
[ "$rc" = 1 ] && echo "$out" | grep -q "not offered by the user-space MCP service" || red "run without a grant: exit $rc: $out"
out=$(tcall ktest '{"filter":"mcp.screen.too_big"}' 2>&1) || true
echo "$out" | grep -q "not Safe" || red "ktest without a grant ran a QemuOnly test: $out"
rc=0; out=$(access status 2>&1) || rc=$?
[ "$rc" = 1 ] && echo "$out" | grep -q "no full access" || red "access status before: exit $rc: $out"
# The test confirmer (uid 1000, through the kernel's plain server: the harness of this boot) would grant.
call "127.0.0.1:$KFWD" run '{"path":"/bin/bitsinstall","args":["test-confirmer","--answers","access:1:nvme"],"timeout_ms":1500}' > /dev/null || true
wait_log "bitsinstall: confirmer connected: /bin/bitsinstall" "the test confirmer is connected"
rc=0; out=$(access request --hours 1 --reason "QEMU scenario 95" 2>&1) || rc=$?
[ "$rc" = 1 ] && echo "$out" | grep -q "not a client Georg paired at the keyboard" || red "request_access of a token-only pairing: exit $rc: $out"
grep -aq "test-confirmer: ask" "$LOG" && red "the token-only client reached the access dialog"
grep -aq "mcp access.live" "$LOG" && red "the kernel took a grant for the token-only client"
ok "a client paired by the token alone: run refused, ktest Safe only, request_access refused before any dialog"
stop_run "$QPID" "$NAME"
QPID=""

# --- boot 2: a new stick under mcp=service: 403, pairing only with a yes, the grant, the full tool set ------
stick
mode service
rm -rf "$STORE"
start 2 8765
PORT=$KFWD
echo "$LINE" | grep -q "plain HTTP only for the pairing, which Georg confirms at the keyboard" || red "boot 2: $LINE"
rc=0; out=$(call "127.0.0.1:$KFWD" hw_info '{"section":"cpu"}' 2>&1) || rc=$?
[ "$rc" = 2 ] && echo "$out" | grep -q "HTTP 403" || red "boot 2: the token over plain HTTP: exit $rc: $out"
rc=0; out=$(node host/mcp/pair.mjs "127.0.0.1:$KFWD" --store "$STORE" --expect "$FP" 2>&1) || rc=$?
[ "$rc" = 1 ] && echo "$out" | grep -q "no desktop session" || red "boot 2: a pairing without a confirmer: exit $rc: $out"
wait_log "init: started the console shell" "boot 2: the console shell" 60
python3 -I "$HERE/qmp_type.py" "$QMP" '\nbitsinstall test-confirmer --answers no,yes,access:1:nvme &\n'
wait_log "bitsinstall: confirmer connected: /bin/bitsinstall" "boot 2: the test confirmer is connected"
rm -rf "$STORE"
rc=0; out=$(node host/mcp/pair.mjs "127.0.0.1:$KFWD" --store "$STORE" --expect "$FP" 2>&1) || rc=$?
[ "$rc" = 1 ] && echo "$out" | grep -q "said no" || red "boot 2: the pairing Georg refused: exit $rc: $out"
rm -rf "$STORE"
out=$(node host/mcp/pair.mjs "127.0.0.1:$KFWD" --store "$STORE" --expect "$FP" 2>&1) || red "boot 2: the pairing Georg confirmed: $out"
wait_log "bitsinstall: pairing .* confirmed at the keyboard: it may get full access" "boot 2: the confirmed pairing was recorded"
CN=$(node -e "const c=require('node:crypto'),f=require('node:fs');console.log(new c.X509Certificate(f.readFileSync(process.argv[1])).subject)" "$STORE/client.crt")
out=$(tcall hw_info '{"section":"cpu"}') && echo "$out" | grep -q "cpu vendor=" || red "boot 2: hw_info over TLS: $out"
rc=0; out=$(tcall run '{"path":"/bin/true"}' 2>&1) || rc=$?
[ "$rc" = 1 ] || red "boot 2: run without a grant: exit $rc: $out"
python3 -I "$HERE/tls_check.py" plain "$KFWD" "$TOKEN" || red "boot 2: plain HTTP after the pairing got an answer"
ok "boot 2: 403 for the token alone, the pairing only after the yes, then TLS only"
out=$(access request --hours 1 --reason "QEMU scenario 95" 2>&1) || red "request_access: $out"
echo "$out" | grep -q "full access for this client until" || red "request_access answer: $out"
ok "request_access: $(echo "$out" | tail -1)"
wait_log "bitsinstall: full access for .* (${CN#CN=}) set at the keyboard for 1 h" "bitsinstall journaled the grant with the client's name"
wait_log "mcp access.live client=" "the kernel took the grant"
wait_log "block nvme.runtime_opened" "the NVMe tick opened the run-time NVMe gate"
out=$(tcall run '{"path":"/bin/sh","args":["-c","echo granted-run $((6*7))"]}') || red "run with the grant: $out"
echo "$out" | grep -q "granted-run 42" || red "run's output: $out"
out=$(tcall write_file '{"path":"/tmp/mc10.txt","data":"bWMxMA=="}') || red "write_file: $out"
out=$(tcall read_file '{"path":"/tmp/mc10.txt","encoding":"text"}') && echo "$out" | grep -q "mc10" || red "read_file: $out"
out=$(tcall screen '{}' --png "$RUN_DIR/screen.png") || red "screen: $out"
[ -s "$RUN_DIR/screen.png" ] || red "screen gave no PNG"
out=$(tcall send_keys '{"text":"x"}') || red "send_keys: $out"
out=$(tcall ktest '{"filter":"mcp.screen.too_big"}' 2>&1) || true
echo "$out" | grep -q "not Safe" && red "ktest with the grant still Safe only: $out"
echo "$out" | grep -q "mcp.screen.too_big" || red "ktest with the grant: $out"
out=$(tcall reboot '{"dry_run":true}') || red "reboot dry run: $out"
wait_log "mcp svc.call tool=reboot peer=10.0.2.2:[0-9]* result=ok ms=[0-9]* access=full" "the reboot ran as a call of the granted client, no question"
grep -aq "svc.install_asked.*tool=reboot" "$LOG" && red "the reboot was asked at the keyboard although granted"
rc=0; out=$(tcall list_dir '{"path":"/data/bitsos-access"}' 2>&1) || rc=$?
[ "$rc" = 1 ] && echo "$out" | grep -q "full-access grants" || red "the grants' store reached by list_dir: exit $rc: $out"
rc=0; out=$(tcall read_file '{"path":"/data/bitsos-access/grants"}' 2>&1) || rc=$?
[ "$rc" = 1 ] && echo "$out" | grep -q "full-access grants" || red "the grants' file reached by read_file: exit $rc: $out"
# `run` starts programs as the desktop user: it sees a root-only directory and cannot read the file in it.
out=$(tcall run '{"path":"/bin/sh","args":["-c","stat -c \"mode %a uid %u\" /data/bitsos-access; cat /data/bitsos-access/grants || echo unreadable"]}') || true
echo "$out" | grep -q "mode 700 uid 0" && echo "$out" | grep -q "unreadable" || red "the store is not root-only: $out"
out=$(access status) || red "access status with the grant: $out"
echo "$out" | grep -q "NVMe write gate is open" || red "access status: $out"
ok "with the grant: run, files, screen, send_keys, a QemuOnly ktest, reboot dry run; store root-only and out of the file tools' reach; $(echo "$out" | head -1)"
# A second client: another key (the pin copied) gets nothing; the token alone over plain HTTP gets nothing.
mkdir -p "$OTHER/pins"
cp "$STORE"/pins/*.der "$OTHER/pins/"
node -e "import('./host/mcp/mcp-keys.mjs').then((m) => m.clientKey(process.argv[1], { make: true }))" "$OTHER"
rc=0; out=$(call "127.0.0.1:$KFWD" hw_info '{"section":"cpu"}' --tls "$OTHER" 2>&1) || rc=$?
[ "$rc" = 2 ] && ! echo "$out" | grep -q "cpu vendor=" || red "an unpaired client key: exit $rc: $out"
python3 -I "$HERE/tls_check.py" plain "$KFWD" "$TOKEN" || red "the token alone over plain HTTP got an answer"
ok "an unpaired client key and the token alone get nothing"
# A Reload over TLS with the grant live (no question): the new kernel's gate gets the table again from the
# installer (which reads /data again), the NVMe gate opens again for the rest of the grant.
lives=$(grep -ac "mcp access.live client=" "$LOG")
opens=$(grep -ac "block nvme.runtime_opened" "$LOG")
out=$(node host/mcp/reload.mjs "127.0.0.1:$KFWD" --name "$NAME" --no-build --bind 127.0.0.1 --port "$HTTP" --back-s 90 --tls "$STORE" 2>&1) || red "reload over TLS with the grant: $out"
ok "reload: $(echo "$out" | tail -1)"
for _ in $(seq 60); do [ "$(grep -ac "mcp access.live client=" "$LOG")" -gt "$lives" ] && break; sleep 1; done
[ "$(grep -ac "mcp access.live client=" "$LOG")" -gt "$lives" ] || red "after the Reload the kernel did not take the grant again"
for _ in $(seq 20); do [ "$(grep -ac "block nvme.runtime_opened" "$LOG")" -gt "$opens" ] && break; sleep 1; done
[ "$(grep -ac "block nvme.runtime_opened" "$LOG")" -gt "$opens" ] || red "after the Reload the NVMe gate did not open again"
out=$(access status) && echo "$out" | grep -q "full access for this client until" || red "after the Reload: access status: $out"
out=$(tcall run '{"path":"/bin/sh","args":["-c","echo after-reload"]}') && echo "$out" | grep -q "after-reload" || red "after the Reload: run: $out"
ok "the grant survived the Reload ($(echo "$out" | head -1)); the NVMe gate opened again"
# A real reboot over TLS: no question (the grant), the answer first, then the reset.
out=$(tcall reboot '{}') || red "reboot: $out"
gone
ok "boot 2 ended with the reboot over TLS"

# --- boot 3: the same stick: the grant survived the reboot, then Georg revokes ------------------------------
# MC-11 (fixer): the installer hands the kernel its table 15 s late (QEMU-only knob under mcp_confirmer=test),
# so the "grants not loaded yet" window is seen end to end: the raw answer first, then the loaded one.
mcopy -n -i "$IMG" ::/BITSOS2/CONFIG.TXT "$RUN_DIR/CONFIG.TXT"
echo "mcp_test_grants_delay_s=15" >> "$RUN_DIR/CONFIG.TXT"
mcopy -o -i "$IMG" "$RUN_DIR/CONFIG.TXT" ::/BITSOS2/CONFIG.TXT
start 3 8765
PORT=$KFWD
echo "$LINE" | grep -q "1 paired client(s), TLS only" || red "boot 3: the pairing was not kept: $LINE"
wait_log "mcp server.not_started reason=\"mcp=service\"" "boot 3: no kernel listener under mcp=service" 10
wait_log "bitsinstall: CONFIG.TXT mcp_test_grants_delay_s" "boot 3: bitsinstall holds the grants back" 10
rc=0; out=$(BITSOS_GRANT_WAIT_MS=0 access status 2>&1) || rc=$?
grep -aq "mcp access.loaded" "$LOG" && red "boot 3: the table was loaded before the raw status (the delay did not hold)"
[ "$rc" = 2 ] && echo "$out" | grep -q "^grants not loaded yet" || red "boot 3: access status before the installer reported: exit $rc: $out"
rc=0; out=$(access status 2>&1) || rc=$?
[ "$rc" = 0 ] && echo "$out" | grep -q "waiting: BitsOS says \"grants not loaded yet\"" && echo "$out" | grep -q "full access for this client until" \
    || red "boot 3: access status did not wait for the grants: exit $rc: $out"
wait_log "mcp access.loaded" "boot 3: the kernel noted the installer's first table" 5
wait_log "mcp access.live client=" "boot 3: the kernel got the kept grant" 40
ok "boot 3: before the installer reported access status said \"grants not loaded yet\" (exit 2); access.mjs waited and then said full access"
out=$(access status) || red "boot 3: access status: $out"
echo "$out" | grep -q "full access for this client until" || red "boot 3: the grant did not survive the reboot: $out"
out=$(tcall run '{"path":"/bin/sh","args":["-c","echo after-reboot"]}') && echo "$out" | grep -q "after-reboot" || red "boot 3: run: $out"
python3 -I "$HERE/tls_check.py" plain "$KFWD" "$TOKEN" || red "boot 3: plain HTTP got an answer"
ok "boot 3: the grant survived the reboot ($(echo "$out" | head -1)); plain HTTP gets nothing"
tcall run '{"path":"/bin/bitsinstall","args":["test-confirmer","--answers","revoke"],"timeout_ms":1500}' > /dev/null || true
wait_log "bitsinstall: confirmer connected: /bin/bitsinstall" "boot 3: the test confirmer is connected"
rc=0; out=$(access request --hours 8 2>&1) || rc=$?
[ "$rc" = 1 ] && echo "$out" | grep -q "said no" || red "boot 3: the request at the revoke: exit $rc: $out"
wait_log "bitsinstall: full access for .* revoked at the keyboard" "boot 3: bitsinstall journaled the revoke"
wait_log "mcp access.ended client=" "boot 3: the kernel ended the grant"
wait_log "block nvme.runtime_closed" "boot 3: the NVMe run-time gate closed"
rc=0; out=$(tcall run '{"path":"/bin/true"}' 2>&1) || rc=$?
[ "$rc" = 1 ] && echo "$out" | grep -q "not offered" || red "boot 3: run after the revoke: exit $rc: $out"
rc=0; out=$(access status 2>&1) || rc=$?
[ "$rc" = 1 ] || red "boot 3: access status after the revoke: exit $rc: $out"
ok "boot 3: revoked at the keyboard: run refused again, NVMe closed"
for f in "$RUN_DIR"/boot*.serial; do
    if grep -aq "$TOKEN" "$f"; then red "the token appears in $f (R8i)"; fi
done
stop_run "$QPID" "$NAME"
QPID=""
echo "mc 95-access: green (a token-only pairing never gets full access, strict pairing, client key per connection, request_access granted by the confirmer, full tool set, store out of reach, NVMe gate bounded, unpaired key and token alone refused, the grant survives a Reload and a reboot, revoke)"
