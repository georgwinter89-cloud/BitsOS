#!/bin/bash
# slow
# PK-0 on V2 (QEMU): /apps is init's bind of /data/apps. A normal boot with MCP and a 64 MB data.img on the stick:
# a fixture package (bitspkg build on the host) installs as root through the console shell (QMP keys), then its
# version 2 over it (links and renames on the bound /apps), MCP reboot; second boot: `bitspkg list` still shows
# both, `verify --store` is clean, init's `bitspkg register` gave settingsd the package's key again, then remove
# leaves no store or trash entry and no data folder of the package (B2; system services' folders such as
# /data/appdata/ctxd stay). Then the same without a data image (data_img_mb=0): /run/bitsos/apps-state
# says tmpfs and the package is gone after the reboot (expected). pk_apps_check.py. Port: lane range + 50.
set -eu -o pipefail
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
ROOT="$(pwd)"
for t in python3 mkfs.fat mcopy; do command -v $t >/dev/null || { echo "RED: $t missing on this host (this scenario needs it)"; exit 1; }; done
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/pk-50-apps-on-data}"
rm -rf "$RUN_DIR"
mkdir -p "$RUN_DIR"
RUN_DIR="$(realpath "$RUN_DIR")"
LO="${BITSOS_PORT_LO:-18500}"
FWD=$((LO + 50))
TOKEN="pk0-scenario-token-5b1e"
# shellcheck source=host/tests/mcp/lib.bash
. "$ROOT/host/tests/mcp/lib.bash"

(cd user && env -u CARGO_TARGET_DIR -u RUSTUP_TOOLCHAIN cargo build -q --offline --release -p bitspkg --target-dir "$ROOT/target/user-host") \
    > "$RUN_DIR/build.log" 2>&1 || { tail -20 "$RUN_DIR/build.log"; echo "RED: host build"; exit 1; }
B="$ROOT/target/user-host/release/bitspkg"
mkdir -p "$RUN_DIR/out"
fixture() { # version, out
    local T="$RUN_DIR/tree$1"
    mkdir -p "$T/share" "$T/bin"
    echo "kept on /data v$1" > "$T/share/note.txt"
    printf '#!/bin/sh\necho hello from apps\n' > "$T/bin/hello"
    chmod 755 "$T/bin/hello"
    sed "s/^version = .*/version = \"$1\"/" > "$T/package.toml" <<'EOF'
format = 1
name = "appsdemo"
version = "1"
en = "PK-0 fixture"
program = "bin/hello"

[[setting]]
key = "app.appsdemo.note"
type = "text"
max_bytes = 100
default = "x"
de = "Notiz"
en = "Note"
EOF
    "$B" build "$T" --out "$RUN_DIR/out/$2" > "$RUN_DIR/build-$2.txt"
}
fixture 1 pkg
fixture 2 pkg2
tar -cf "$RUN_DIR/pkg.tar" -C "$RUN_DIR/out" pkg pkg2

NAME=""
QPID=""
cleanup() {
    if [ -n "$QPID" ]; then stop_run "$QPID" "$NAME"; fi
    # The sticks are 700 MB larger than an image (the data.img lives in them): never left behind.
    rm -f target/xtask/pk-apps.img target/xtask/pk-apps0.img
}
trap cleanup EXIT

image() { # name, data_img_mb
    cargo xtask image --name "$1" --config-extra "token=$TOKEN;mcp_port=8765;loglevel=info;nvme_write=off;data_img_mb=$2" > "$RUN_DIR/image-$1.out" 2>&1 \
        || { tail -20 "$RUN_DIR/image-$1.out"; echo "RED: image $1"; exit 1; }
    local img="target/xtask/$1.img" kib
    [ "$2" = 0 ] && return 0
    # The stick holds the 64 MB data.img the kernel makes at the first boot (it keeps 512 MB free beside it).
    kib=$(( $(stat -c %s "$img") / 1024 + 700 * 1024 ))
    rm -f "$img"
    mkfs.fat -F 32 -n BITSOS -C "$img" "$kib" > /dev/null
    mcopy -s -i "$img" "target/xtask/esp-$1/EFI" "target/xtask/esp-$1/BITSOS2" ::/
}

boot() { # name, phase
    NAME="$1"
    local phase="$2" log="$RUN_DIR/serial-$2.log" qmp="$RUN_DIR/$2.qmp"
    stop_qemu "$NAME"
    rm -f "$log" "$qmp"
    cargo xtask qemu --name "$NAME" --net virtio --hostfwd "$FWD:8765" --timeout 420 --serial-log "$log" \
        -- -machine i8042=off -device qemu-xhci,id=xb -device usb-kbd,bus=xb.0,port=1,id=kbd1 -qmp "unix:$qmp,server=on,wait=off" \
        > "$RUN_DIR/qemu-$phase.out" 2>&1 &
    QPID=$!
    if ! python3 -I host/tests/pk/pk_apps_check.py "$phase" "$FWD" "$TOKEN" "$qmp" "$log" "$RUN_DIR/pkg.tar" | tee "$RUN_DIR/check-$phase.txt"; then
        grep -a "init: \|bitspkg\|PANIC" "$log" | tail -30
        exit 1
    fi
    case "$phase" in
        *-1) for _ in $(seq 1 60); do kill -0 "$QPID" 2>/dev/null || break; sleep 1; done ;;
    esac
    stop_run "$QPID" "$NAME"
    QPID=""
    if grep -aq "PANIC" "$log"; then echo "RED: the kernel panicked in $phase"; tail -20 "$log"; exit 1; fi
}

image pk-apps 64
boot pk-apps data-1
boot pk-apps data-2
image pk-apps0 0
boot pk-apps0 tmpfs-1
boot pk-apps0 tmpfs-2
if grep -aq "$TOKEN" "$RUN_DIR"/serial-*.log; then echo "RED: the token appears in a serial log (R8i)"; exit 1; fi
echo "PK-0 QEMU scenario: green (/apps on /data survives a reboot, register at boot; without a data image tmpfs, as said)"
