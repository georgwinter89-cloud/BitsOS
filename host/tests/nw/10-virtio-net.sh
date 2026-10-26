#!/bin/bash
# NW-1: virtio-net ktests on a real (QEMU) card. Three runs: the transitional device QEMU puts on the q35 root
# bus (1AF4:1000, `xtask qemu --net virtio`), a modern-only one (1AF4:1041, disable-legacy=on), and a link
# watch run where QMP pulls the "cable" (set_link off/on) while `nw.link.qmp` waits.
# Fails if a run is red or if a test only passed because no card was found (`ktest.no_device`).
# Known trap (not NW's): OVMF sometimes stalls before it reaches the boot loader (no `BdsDxe: loading` line);
# such a boot is retried once and reported.
set -eu
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/nw-10-virtio-net}"
mkdir -p "$RUN_DIR"
RUN_DIR="$(realpath "$RUN_DIR")"
TESTS="nw.virtio,contract.netdevice,contract.quiesce"
BOOT_LINE="BdsDxe: loading"

check() { # $1 = log, $2 = PCI device id the driver must have probed
    local log="$1" want="$2" ok
    grep -a "KTEST" "$log" || true
    if grep -aq "ktest.no_device" "$log"; then
        echo "RED: a test found no virtio-net card"; return 1
    fi
    ok=$(grep -ac "KTEST ok nw.virtio." "$log" || true)
    if [ "$ok" -lt 8 ]; then
        echo "RED: only $ok nw.virtio tests passed (want 8)"; return 1
    fi
    if ! grep -aq "virtio.probe.*device=0x$want" "$log"; then
        echo "RED: device 1af4:$want was not the one probed"; return 1
    fi
    grep -aq "ktest.dhcp_offer" "$log" || { echo "RED: no DHCP offer event"; return 1; }
}

run_boot() { # $1 = name, $2 = log, rest = xtask qemu options; one retry after a firmware stall
    local name="$1" log="$2"; shift 2
    for attempt in 1 2; do
        if cargo xtask qemu --name "$name" --timeout 120 --serial-log "$log" "$@"; then
            return 0
        fi
        grep -aq "$BOOT_LINE" "$log" && return 1
        echo "note: attempt $attempt: firmware never reached the boot loader, retrying"
    done
    return 1
}

# 1. transitional (QEMU default on the root bus)
cargo xtask image --name nw-virtio-t --ktest --config-extra "ktest=$TESTS;loglevel=info"
run_boot nw-virtio-t "$RUN_DIR/transitional.log" --net virtio
check "$RUN_DIR/transitional.log" 1000

# 2. modern only
cargo xtask image --name nw-virtio-m --ktest --config-extra "ktest=$TESTS;loglevel=info"
run_boot nw-virtio-m "$RUN_DIR/modern.log" --net none -- -netdev user,id=n1 -device virtio-net-pci,netdev=n1,disable-legacy=on
check "$RUN_DIR/modern.log" 1041

# 3. link watch: QMP set_link off/on while nw.link.qmp waits (a cable pull in QEMU)
qmp() { # $1 = up (true|false)
    python3 -I -c '
import json, socket, sys
s = socket.socket(socket.AF_UNIX); s.connect(sys.argv[1]); f = s.makefile("rw")
f.readline()
for c in ({"execute": "qmp_capabilities"}, {"execute": "set_link", "arguments": {"name": "n1", "up": sys.argv[2] == "true"}}):
    f.write(json.dumps(c) + "\n"); f.flush(); r = json.loads(f.readline())
    if "error" in r: sys.exit(r)
' "$RUN_DIR/qmp.sock" "$1"
}
wait_line() { # $1 = pattern, $2 = log, $3 = seconds
    local n=$(( $3 * 10 ))
    for _ in $(seq 1 "$n"); do grep -aq "$1" "$2" 2>/dev/null && return 0; sleep 0.1; done
    return 1
}
QPID=""
# Only our own processes: the QEMU this xtask started (its pid file) and the xtask itself.
stop_own() {
    if [ -n "$QPID" ] && kill -0 "$QPID" 2>/dev/null; then
        local q; q=$(cat target/xtask/qemu-nw-virtio-l.pid 2>/dev/null || true)
        if [ -n "$q" ]; then kill "$q" 2>/dev/null || true; fi
        kill "$QPID" 2>/dev/null || true
        wait "$QPID" 2>/dev/null || true
    fi
    QPID=""
}
trap stop_own EXIT
cargo xtask image --name nw-virtio-l --ktest --config-extra "ktest=nw.link;loglevel=info"
for attempt in 1 2; do
    rm -f "$RUN_DIR/qmp.sock" "$RUN_DIR/link.log"
    cargo xtask qemu --name nw-virtio-l --net none --timeout 120 --serial-log "$RUN_DIR/link.log" -- \
        -netdev user,id=n1 -device virtio-net-pci,netdev=n1,mac=52:54:00:4e:57:01 -qmp "unix:$RUN_DIR/qmp.sock,server=on,wait=off" &
    QPID=$!
    if wait_line "$BOOT_LINE" "$RUN_DIR/link.log" 60; then break; fi
    echo "note: attempt $attempt: firmware never reached the boot loader, retrying"
    stop_own
    [ "$attempt" = 2 ] && { echo "RED: link run never booted"; exit 1; }
done
wait_line "ktest.link_wait" "$RUN_DIR/link.log" 60 || { echo "RED: no ktest.link_wait"; exit 1; }
qmp false
wait_line "ktest.link_down_seen" "$RUN_DIR/link.log" 60 || { echo "RED: link down not seen"; exit 1; }
qmp true
wait "$QPID" || true
QPID=""
grep -a "KTEST\|link\." "$RUN_DIR/link.log"
grep -aq "KTEST ok nw.link.qmp" "$RUN_DIR/link.log" || { echo "RED: nw.link.qmp"; exit 1; }
echo "nw virtio-net: green (transitional + modern + link watch)"
