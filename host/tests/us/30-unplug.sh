#!/bin/bash
# US-2: unplug while a key is held, then replug, twice per boot (ktest us.unplug.held drives the kernel side,
# this script the "hands" through QMP). Boot 1: keyboard right on a root port (high speed); boot 2: keyboard
# behind a usb-hub (full speed, the hub's status endpoint reports the change). Per round: the ktest binds a test
# driver to the boot keyboard and says `ktest.kbd_ready`; QMP presses "a" and keeps it down; the ktest sees the
# report (`ktest.key_held`); QMP device_del pulls the keyboard; the ktest checks the detach (driver's disconnect
# once, its pipe stopped, the slot disabled, controller fine) and says `ktest.unplug_done`; QMP device_add plugs
# it back; the ktest sees it enumerated again (`ktest.replug_seen`). No PS/2 keyboard (i8042=off), like the
# Mini-PC, so QEMU sends the key to the USB keyboard.
set -eu
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/us-30-unplug}"
mkdir -p "$RUN_DIR"
RUN_DIR="$(realpath "$RUN_DIR")"
SOCK="$RUN_DIR/qmp.sock"
BOOT_LINE="BdsDxe: loading"

qmp() { # $1 = JSON command
    python3 -I -c '
import json, socket, sys
s = socket.socket(socket.AF_UNIX); s.settimeout(20); s.connect(sys.argv[1]); f = s.makefile("rw")
f.readline()
for c in ({"execute": "qmp_capabilities"}, json.loads(sys.argv[2])):
    f.write(json.dumps(c) + "\n"); f.flush()
    while True:
        r = json.loads(f.readline())
        if "return" in r or "error" in r:
            break
    if "error" in r:
        sys.exit("qmp: %s" % r)
' "$SOCK" "$1"
}
key_down='{"execute":"input-send-event","arguments":{"events":[{"type":"key","data":{"down":true,"key":{"type":"qcode","data":"a"}}}]}}'
wait_line() { # $1 = pattern, $2 = log, $3 = seconds
    local n=$(( $3 * 10 ))
    for _ in $(seq 1 "$n"); do grep -aq "$1" "$2" 2>/dev/null && return 0; sleep 0.1; done
    return 1
}
QPID=""
NAME=""
# Only our own processes: the QEMU this xtask started (its pid file) and the xtask itself.
stop_own() {
    if [ -n "$QPID" ] && kill -0 "$QPID" 2>/dev/null; then
        local q; q=$(cat "target/xtask/qemu-$NAME.pid" 2>/dev/null || true)
        if [ -n "$q" ]; then kill "$q" 2>/dev/null || true; fi
        kill "$QPID" 2>/dev/null || true
        wait "$QPID" 2>/dev/null || true
    fi
    QPID=""
}
trap stop_own EXIT
fail() { echo "RED: $*"; exit 1; }

run() { # $1 = name, $2 = device_add arguments of the keyboard, rest = extra QEMU devices
    NAME="$1"; local kbd="$2"; shift 2
    local log="$RUN_DIR/$NAME.log"
    cargo xtask image --name "$NAME" --ktest --config-extra "ktest=us.unplug;usb_unplug_test=on;loglevel=info"
    for attempt in 1 2; do
        rm -f "$SOCK" "$log"
        cargo xtask qemu --name "$NAME" --timeout 240 --serial-log "$log" -- \
            -machine i8042=off -device qemu-xhci,id=xb "$@" -device "$kbd" -qmp "unix:$SOCK,server=on,wait=off" &
        QPID=$!
        if wait_line "$BOOT_LINE" "$log" 60; then break; fi
        echo "note: attempt $attempt: firmware never reached the boot loader, retrying"
        stop_own
        [ "$attempt" = 2 ] && fail "$NAME never booted"
    done
    for round in 1 2; do
        wait_line "ktest.kbd_ready round=$round" "$log" 90 || fail "$NAME round $round: no ktest.kbd_ready"
        qmp "$key_down"
        wait_line "ktest.key_held round=$round" "$log" 60 || fail "$NAME round $round: key not seen"
        qmp '{"execute":"device_del","arguments":{"id":"kbd1"}}'
        wait_line "ktest.unplug_done round=$round" "$log" 60 || fail "$NAME round $round: unplug not handled"
        qmp "{\"execute\":\"device_add\",\"arguments\":$(python3 -I -c 'import json,sys; print(json.dumps(dict(kv.split("=",1) for kv in ("driver="+sys.argv[1]).split(","))))' "$kbd")}"
        wait_line "ktest.replug_seen round=$round" "$log" 60 || fail "$NAME round $round: replug not seen"
    done
    wait "$QPID" || true
    QPID=""
    grep -a "KTEST\|ktest\.\|device\.attached\|device\.detached" "$log" || true
    grep -aq "KTEST ok us.unplug.held" "$log" || fail "$NAME: us.unplug.held did not pass"
    if grep -aq "xhci.failed\|xhci.slot_leaked\|lock.violation\|PANIC" "$log"; then fail "$NAME: failure line"; fi
}

run us-unplug-root "usb-kbd,id=kbd1,bus=xb.0,port=2"
run us-unplug-hub "usb-kbd,id=kbd1,bus=xb.0,port=1.2" -device usb-hub,bus=xb.0,port=1
echo "us-30-unplug: green"
