#!/bin/bash
# slow
# CL-1 on V2 (QEMU): a normal boot with MCP; the static musl settingsd is sent with write_file (it is not in the
# initramfs until CL-1 CR1) and started from the console's BusyBox shell (uid 0, as init will start it after
# CR2) by typing with send_keys; its client runs as uid 1000 through MCP `run`: set/apply/undo for every
# key, R8 switches refused, kernel keys as CONFIG.TXT lines with the guarded lines unchanged, values and journal
# after a restart, the fallback without a stick. Afterwards the image is read on the host with mtools: the
# settings file is a plain FAT file Georg can read on the Gaming-PC. Ports: lane range + 20.
set -eu
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
ROOT="$(pwd)"
for t in python3 mtype; do command -v $t >/dev/null || { echo "SKIP: $t missing"; exit 0; }; done
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/cl-20-settingsd-qemu}"
mkdir -p "$RUN_DIR"
RUN_DIR="$(realpath "$RUN_DIR")"
LO="${BITSOS_PORT_LO:-30000}"
FWD=$((LO + 20))
TOKEN="cl1-scenario-token-5e1b"
LOG="$RUN_DIR/serial.log"
NAME=cl-settingsd

(cd user/services/settingsd && CARGO_TARGET_DIR="$ROOT/target" cargo build --offline --release --target x86_64-unknown-linux-musl) \
    > "$RUN_DIR/build.log" 2>&1 || { tail -20 "$RUN_DIR/build.log"; echo "RED: musl build"; exit 1; }
BIN="$ROOT/target/x86_64-unknown-linux-musl/release/settingsd"

cargo xtask image --name "$NAME" --config-extra "token=$TOKEN;mcp_port=8765;loglevel=info;wlan=off;nvme_write=off"
rm -f "$LOG"
cargo xtask qemu --name "$NAME" --net virtio --hostfwd "$FWD:8765" --timeout 500 --serial-log "$LOG" \
    > "$RUN_DIR/qemu.out" 2>&1 &
QPID=$!
stop() {
    kill "$QPID" 2>/dev/null || true
    pf="target/xtask/qemu-$NAME.pid"
    if [ -f "$pf" ]; then
        pid="$(head -1 "$pf")"
        if tr '\0' ' ' < "/proc/$pid/cmdline" 2>/dev/null | grep -Eq -- "-name v2-[A-Za-z0-9]+-$NAME( |$)"; then kill "$pid" 2>/dev/null || true; fi
    fi
    wait "$QPID" 2>/dev/null || true
}
trap stop EXIT
python3 -I host/tests/cl/settings_qemu_check.py "$FWD" "$TOKEN" "$BIN" "$RUN_DIR"
if grep -aq "PANIC" "$LOG"; then echo "RED: the kernel panicked"; tail -20 "$LOG"; exit 1; fi
grep -aq "settingsd set key=" "$LOG" || { echo "RED: no settingsd events in the kernel log (/dev/kmsg)"; exit 1; }
grep -a "settingsd " "$LOG" | grep -av " set key=\| undo key=" | tail -5 || true
stop
trap - EXIT
IMG="target/xtask/$NAME.img"
mtype -i "$IMG" ::BITSOS2/settings/theme.toml > "$RUN_DIR/theme.toml" 2>&1 || { cat "$RUN_DIR/theme.toml"; echo "RED: no theme.toml on the image"; exit 1; }
grep -q 'theme.accent = "#ff00aa"' "$RUN_DIR/theme.toml" || { cat "$RUN_DIR/theme.toml"; echo "RED: theme.toml content"; exit 1; }
mtype -i "$IMG" ::BITSOS2/CONFIG.TXT | grep -Eq "^nvme_write=off" || { echo "RED: nvme_write line changed"; exit 1; }
echo "settingsd on V2 (QEMU): green; theme.toml on the image reads on the host:"
cat "$RUN_DIR/theme.toml"
