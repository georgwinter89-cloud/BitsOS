#!/bin/bash
# RB-1: klog under a real boot with CONFIG `loglevel=` per subsystem and `log_buf=`.
# Runs the rb ktests (early ring, try_event, levels, deferred sink, filters; the `log` tool and the drain thread
# once klogd is linked) and checks the CONFIG side: the level settings and the ring size from `log_buf`.
# Until the kernel calls `klog::configure` (change request in plan/notes/rb-RB-1.md) the CONFIG part is skipped
# with a note, the ktests still have to pass.
set -eu
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/rb-10-klog-config}"
mkdir -p "$RUN_DIR"
RUN_DIR="$(realpath "$RUN_DIR")"
LOG="$RUN_DIR/serial.log"
BOOT_LINE="BdsDxe: loading"

cargo xtask image --name rb-klog-config --ktest --config-extra "ktest=rb;loglevel=warn,klog=debug;log_buf=2M"
for attempt in 1 2; do
    if cargo xtask qemu --name rb-klog-config --timeout 120 --serial-log "$LOG" --net virtio; then
        break
    fi
    grep -aq "$BOOT_LINE" "$LOG" && { echo "RED: ktests failed"; grep -a "KTEST" "$LOG"; exit 1; }
    echo "note: attempt $attempt: firmware never reached the boot loader, retrying"
done
grep -a "KTEST" "$LOG" || true
grep -aq "KTEST done passed=.* failed=0" "$LOG" || { echo "RED: no clean KTEST done line"; exit 1; }
ok=$(grep -ac "KTEST ok rb.klog." "$LOG" || true)
[ "$ok" -ge 5 ] || { echo "RED: only $ok rb.klog ktests passed (want 5)"; exit 1; }

# Only the boot part counts (the ktests set levels themselves).
BOOT="$RUN_DIR/boot.log"
sed -n '1,/KTEST start/p' "$LOG" > "$BOOT"
if ! grep -aq "klog levels.set" "$BOOT"; then
    echo "skip (CONFIG part): the kernel does not call klog::configure yet"
    exit 0
fi
grep -aq 'klog levels.set levels="warn,klog=debug"' "$BOOT" || { echo "RED: loglevel= not applied"; exit 1; }
grep -aq "klog ring.sized bytes=2097152 source=log_buf" "$BOOT" || { echo "RED: log_buf=2M not applied"; exit 1; }
# warn as default: Info events of other subsystems stay off the serial line, klog's own (debug) appear.
if grep -aq "I kernel start" "$BOOT"; then echo "RED: Info event despite loglevel=warn"; exit 1; fi
if grep -aq "KTEST ok rb.log.ring_sized" "$LOG" || grep -aq "KTEST skip rb.log.ring_sized: CONFIG log_buf" "$LOG"; then
    echo "klogd linked: log tool and drain thread tested"
fi
echo "GREEN: klog CONFIG and ktests"
