#!/bin/bash
# slow
# MC-8b: mutual TLS with the one-time pairing in the user-space MCP service bitsmcp (mcp=both: the kernel's server
# stays plain on 8765, the service on 8766; hostfwd LO+120, LO+121).
#   boot 1: the service makes its key on the stick and serves plain HTTP with the token while no client is paired;
#     mcp-call.mjs --tls refuses before the pairing; host/mcp/pair.mjs pairs (--expect the fingerprint of the
#     service's start line) and pins the Mini-PC's key; mcp-call.mjs --tls calls hw_info; from now a plain HTTP
#     request, a TLS client without a key and one with an unpaired key get nothing back (tls_check.py, no HTTP
#     byte); the kernel's server still answers plain; the key and the paired certificate are on the stick image.
#   boot 2 (same stick): the same key, the pairing kept, TLS only from the start.
#   boot 3 (server.key and server.crt removed from the stick): a new key; mcp-call.mjs --tls refuses it loudly
#     ("ANOTHER KEY", nothing sent), the kernel's server answers on.
#   boot 4 (the same files on a stick fs-fat keeps read-only: one FAT instead of two, crates/fs-fat/src/stick.rs
#     rule 4): the key and the pairing are read, TLS only (the new key of boot 3 pinned by hand here), plain gets
#     nothing.
#   boot 5 (a new read-only stick): no key can be made and no client is paired: the warning line, plain HTTP with
#     the token, a pairing is refused ("no TLS this start").
# Needs init's glue (plan/notes/mc-MC-8a.md, change request 1) like 60-service.sh; MC_ALLOW_NO_GLUE=1 turns its
# absence into a SKIP (lane branch only).
set -eu -o pipefail
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/mc-80-tls}"
mkdir -p "$RUN_DIR"
RUN_DIR="$(realpath "$RUN_DIR")"
HERE="$(pwd)/host/tests/mcp"
# shellcheck source=host/tests/mcp/lib.bash
. "$HERE/lib.bash"
LO="${BITSOS_PORT_LO:-25000}"
KFWD=$((LO + 120))
SFWD=$((LO + 121))
TOKEN="mc8b-scenario-token-7d03"
NAME=mc-tls

if ! grep -q "fn bitsmcp" user/init/src/main.rs; then
    if [ "${MC_ALLOW_NO_GLUE:-}" = 1 ]; then echo "SKIP: user/init/src/main.rs does not start bitsmcp yet (plan/notes/mc-MC-8a.md, change request 1)"; exit 0; fi
    echo "RED: user/init lacks the bitsmcp glue (plan/notes/mc-MC-8a-init.patch, change request 1)"
    exit 1
fi
command -v node > /dev/null || { echo "RED: node is needed (host/mcp's tools)"; exit 1; }

QPID=""
cleanup() { if [ -n "$QPID" ]; then stop_run "$QPID" "$NAME"; fi; }
trap cleanup EXIT
red() { echo "RED: $*"; [ -n "${LOG:-}" ] && grep -a "bitsmcp" "$LOG" | tail -12; exit 1; }

IMG="target/xtask/$NAME.img"
# The key store: in the run directory (a build directory: pair.mjs refuses any other place in the repository).
STORE="$RUN_DIR/store"
OTHER="$RUN_DIR/other"
rm -rf "$STORE" "$OTHER"
export BITSOS_TOKEN="$TOKEN"
call() { node host/mcp/mcp-call.mjs "$@"; }

image() {
    cargo xtask image --name "$NAME" --config-extra "token=$TOKEN;mcp_port=8765;mcp=both;loglevel=info;stick_write=on" | tail -1
    local kib
    kib=$(( $(stat -c %s "$IMG") / 1024 + 64 * 1024 ))
    rm -f "$IMG"
    mkfs.fat -F 32 -n BITSOS -C "$IMG" "$kib" > /dev/null
    mcopy -s -i "$IMG" "target/xtask/esp-$NAME/EFI" "target/xtask/esp-$NAME/BITSOS2" ::/
}

# The stick image's files on a FAT32 volume with one FAT: fs-fat mounts it but never decides it writable.
read_only() {
    rm -rf "$RUN_DIR/tree"
    mkdir "$RUN_DIR/tree"
    mcopy -s -n -i "$IMG" ::/EFI ::/BITSOS2 "$RUN_DIR/tree/"
    local kib
    kib=$(( $(stat -c %s "$IMG") / 1024 ))
    rm -f "$IMG"
    mkfs.fat -F 32 -f 1 -n BITSOS -C "$IMG" "$kib" > /dev/null
    mcopy -s -i "$IMG" "$RUN_DIR/tree/EFI" "$RUN_DIR/tree/BITSOS2" ::/
    rm -rf "$RUN_DIR/tree"
}

decided_ro() { # $1 = boot number: fs-fat decides about 2 s after the mount
    for _ in $(seq 30); do
        grep -asq "fat stick.decided" "$LOG" && break
        sleep 1
    done
    grep -aq "fat stick.decided .*rw=false" "$LOG" || red "boot $1: the stick was not kept read-only: $(grep -a "fat stick.decided" "$LOG")"
}

start() { # $1 = boot number, $2 = notls when the service is to start without TLS; the same stick image
    stop_qemu "$NAME"
    LOG="$RUN_DIR/boot$1.serial"
    rm -f "$LOG"
    cargo xtask qemu --name "$NAME" --net virtio --hostfwd "$KFWD:8765" --hostfwd "$SFWD:8766" --timeout 300 --serial-log "$LOG" > "$RUN_DIR/boot$1.qemu" 2>&1 &
    QPID=$!
    for _ in $(seq 120); do
        grep -asq "bitsmcp: serving MCP on port 8766" "$LOG" && break
        sleep 1
    done
    grep -asq "bitsmcp: serving MCP on port 8766" "$LOG" || red "boot $1: bitsmcp did not report serving within 120 s"
    if [ "${2:-}" = notls ]; then
        LINE=$(grep -a "bitsmcp: no TLS this start" "$LOG" | tail -1)
        [ -n "$LINE" ] || red "boot $1: no warning line about the start without TLS"
        grep -aq "bitsmcp: TLS server key" "$LOG" && red "boot $1: TLS after all"
        echo "boot $1: $(echo "$LINE" | sed 's/.*bitsmcp: //')"
        return 0
    fi
    LINE=$(grep -a "bitsmcp: TLS server key" "$LOG" | tail -1)
    FP=$(echo "$LINE" | grep -o "TLS server key sha256 [0-9a-f]\{64\}" | awk '{print $5}')
    [ -n "$FP" ] || red "boot $1: no TLS on the start line: $LINE"
    echo "boot $1: $(echo "$LINE" | sed 's/.*bitsmcp: //')"
}

stop() {
    stop_run "$QPID" "$NAME"
    QPID=""
}

# --- boot 1: the first start makes the key; pairing --------------------------------------------------------
image
start 1
echo "$LINE" | grep -q "(made now), no client paired yet" || red "boot 1: the key was not made at the first start: $LINE"
FP1="$FP"
out=$(call "127.0.0.1:$SFWD" hw_info '{"section":"cpu"}') && echo "$out" | grep -q "cpu vendor=" || red "plain hw_info before the pairing: $out"
rc=0; out=$(call "127.0.0.1:$SFWD" hw_info '{}' --tls "$STORE" 2>&1) || rc=$?
[ "$rc" = 2 ] && echo "$out" | grep -q "pair first" || red "--tls before the pairing: exit $rc: $out"
rc=0; out=$(node host/mcp/pair.mjs "127.0.0.1:$SFWD" --store "$STORE" --expect 0000 2>&1) || rc=$?
[ "$rc" = 1 ] && echo "$out" | grep -q "not the expected 0000; not paired" || red "pair.mjs --expect of another key: exit $rc: $out"
out=$(node host/mcp/pair.mjs "127.0.0.1:$SFWD" --store "$STORE" --expect "$FP1" 2>&1) || red "pair.mjs: $out"
echo "$out"
grep -aq "bitsmcp: paired the client [0-9a-f]\{64\} from 10.0.2.2:" "$LOG" || red "no log line about the pairing"
out=$(call "127.0.0.1:$SFWD" hw_info '{"section":"cpu"}' --tls "$STORE") && echo "$out" | grep -q "cpu vendor=" || red "hw_info over TLS: $out"
out=$(call "127.0.0.1:$SFWD" version '{}' --tls "$STORE") || red "version over TLS: $out"
echo "ok: over TLS: $(echo "$out" | head -1)"
rc=0; out=$(call "127.0.0.1:$SFWD" run '{"path":"/bin/true"}' --tls "$STORE" 2>&1) || rc=$?
[ "$rc" = 1 ] && echo "$out" | grep -q "not offered by the user-space MCP service" || red "run over TLS: exit $rc: $out"
python3 -I "$HERE/tls_check.py" plain "$SFWD" "$TOKEN" || red "a plain request after the pairing got an answer"
python3 -I "$HERE/tls_check.py" nocert "$SFWD" "$TOKEN" || red "a TLS client without a key got an answer"
rc=0; out=$(call "127.0.0.1:$SFWD" hw_info '{}' 2>&1) || rc=$?
[ "$rc" = 2 ] || red "mcp-call.mjs without --tls after the pairing: exit $rc: $out"
# Another host key (the pin copied, so only the client key differs).
mkdir -p "$OTHER/pins"
cp "$STORE"/pins/*.der "$OTHER/pins/"
node -e "import('./host/mcp/mcp-keys.mjs').then((m) => m.clientKey(process.argv[1], { make: true }))" "$OTHER"
rc=0; out=$(call "127.0.0.1:$SFWD" hw_info '{"section":"cpu"}' --tls "$OTHER" 2>&1) || rc=$?
[ "$rc" = 2 ] && ! echo "$out" | grep -q "cpu vendor=" || red "an unpaired client key: exit $rc: $out"
echo "ok: an unpaired client key: exit 2 ($out)"
# A second pairing (no pin yet in that store): the plain path is closed, nothing pairs.
rm -f "$OTHER"/pins/*.der
rc=0; out=$(node host/mcp/pair.mjs "127.0.0.1:$SFWD" --store "$OTHER" --trust-first-use 2>&1) || rc=$?
[ "$rc" = 2 ] && [ ! -e "$OTHER/pins/127.0.0.1.der" ] || red "a second pairing: exit $rc: $out"
sleep 1
grep -aq "refused 10.0.2.2:[0-9]*: no TLS (plain HTTP is off since a client is paired)" "$LOG" || red "no log line for the plain request"
grep -aq "refused 10.0.2.2:[0-9]*: TLS handshake failed: its key is not paired" "$LOG" || red "no log line for the unpaired key"
grep -aq "refused 10.0.2.2:[0-9]*: TLS handshake failed: it showed no key" "$LOG" || red "no log line for the client without a key"
out=$(call "127.0.0.1:$KFWD" version '{}') || red "the kernel's server (plain) after the pairing: $out"
echo "ok: the kernel's server answers plain on 8765: $(echo "$out" | head -1)"
grep -a "mcp svc.call" "$LOG" | grep -q "result=ok" || red "no kernel event for the calls over TLS"
stop
# The stick holds the key and the paired certificate.
mdir -b -i "$IMG" ::/BITSOS2/mcp/ ::/BITSOS2/mcp/paired/ | sed 's/^/stick: /'
mcopy -n -i "$IMG" ::/BITSOS2/mcp/server.crt "$RUN_DIR/server.crt"
mcopy -n -i "$IMG" ::/BITSOS2/mcp/paired/first.der "$RUN_DIR/first.der"
cmp "$RUN_DIR/server.crt" "$STORE/pins/127.0.0.1.der" || red "the pinned key is not the stick's server.crt"
node -e "const c=require('node:crypto'),f=require('node:fs');process.exit(Buffer.compare(new c.X509Certificate(f.readFileSync(process.argv[1])).raw,f.readFileSync(process.argv[2]))?1:0)" "$STORE/client.crt" "$RUN_DIR/first.der" \
    || red "paired/first.der is not this host's certificate"
echo "ok: the stick keeps server.key, server.crt and paired/first.der (this host's certificate)"

# --- boot 2: the same stick ---------------------------------------------------------------------------------
start 2
[ "$FP" = "$FP1" ] || red "boot 2: another key ($FP, was $FP1)"
echo "$LINE" | grep -q "1 paired client(s), TLS only" || red "boot 2: the pairing was not kept: $LINE"
echo "$LINE" | grep -q "(made now)" && red "boot 2: the key was made again"
out=$(call "127.0.0.1:$SFWD" hw_info '{"section":"cpu"}' --tls "$STORE") && echo "$out" | grep -q "cpu vendor=" || red "boot 2: hw_info over TLS: $out"
python3 -I "$HERE/tls_check.py" plain "$SFWD" "$TOKEN" || red "boot 2: a plain request got an answer"
echo "ok: boot 2: the same key, the pairing kept, TLS only"
stop

# --- boot 3: the key made anew ------------------------------------------------------------------------------
mdel -i "$IMG" ::/BITSOS2/mcp/server.key ::/BITSOS2/mcp/server.crt
start 3
[ "$FP" != "$FP1" ] && echo "$LINE" | grep -q "(made now), 1 paired client(s)" || red "boot 3: no new key: $LINE"
rc=0; out=$(call "127.0.0.1:$SFWD" hw_info '{"section":"cpu"}' --tls "$STORE" 2>&1) || rc=$?
[ "$rc" = 2 ] && echo "$out" | grep -q "ANOTHER KEY" && echo "$out" | grep -q "Nothing was sent" || red "boot 3: a changed key was not refused: exit $rc: $out"
echo "ok: boot 3: $out"
out=$(call "127.0.0.1:$KFWD" version '{}') || red "boot 3: the kernel's server: $out"
stop

# --- boot 4: the same files on a read-only stick -----------------------------------------------------------
read_only
# The host takes boot 3's new key deliberately (the way pairing again would end).
mcopy -n -i "$IMG" ::/BITSOS2/mcp/server.crt "$STORE/pins/127.0.0.1.der"
FP3="$FP"
start 4
[ "$FP" = "$FP3" ] && echo "$LINE" | grep -q "1 paired client(s), TLS only" || red "boot 4: the read-only stick's key and pairing were not taken: $LINE"
out=$(call "127.0.0.1:$SFWD" hw_info '{"section":"cpu"}' --tls "$STORE") && echo "$out" | grep -q "cpu vendor=" || red "boot 4: hw_info over TLS: $out"
python3 -I "$HERE/tls_check.py" plain "$SFWD" "$TOKEN" || red "boot 4: a plain request got an answer"
decided_ro 4
echo "ok: boot 4: a read-only stick keeps the key and the pairing, TLS only"
stop

# --- boot 5: a new read-only stick: nothing paired, no key can be made -------------------------------------
image
read_only
start 5 notls
decided_ro 5
echo "$LINE" | grep -q "no client is paired, so plain HTTP with the token" || red "boot 5: $LINE"
out=$(call "127.0.0.1:$SFWD" hw_info '{"section":"cpu"}') && echo "$out" | grep -q "cpu vendor=" || red "boot 5: plain hw_info: $out"
rc=0; out=$(node host/mcp/pair.mjs "127.0.0.1:$SFWD" --store "$RUN_DIR/store5" --trust-first-use 2>&1) || rc=$?
[ "$rc" = 1 ] && echo "$out" | grep -q "the service has no TLS this start" || red "boot 5: a pairing without TLS: exit $rc: $out"
echo "ok: boot 5: no TLS on a new read-only stick: plain HTTP with the token, no pairing"
rm -rf "$RUN_DIR/store5"
for f in "$RUN_DIR"/boot*.serial; do
    if grep -aq "$TOKEN" "$f"; then red "the token appears in $f (R8i)"; fi
done
stop
echo "mc 80-tls: green (key made once and kept, pairing with --expect, TLS calls, plain/keyless/unpaired get nothing, the changed key refused, a read-only stick keeps TLS, a new read-only stick serves plain, the kernel's server plain throughout)"
