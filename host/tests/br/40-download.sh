#!/bin/bash
# minipc: left the QEMU runs (Georg 30.10., CONTRACTS section 4); its checks run on the Mini-PC by the check list in plan/notes/br-BR-7.md
# slow
# BR-7: downloads and GTK's save dialog. Georg (24.10.): Brave died when he downloaded a file, because GTK's file
# chooser found no image loader (no MIME database, no glycin loader configs) and aborted on its first icon. Steps:
#  1. Host: GTK 3 loads the file chooser's icons from the staged tree (Adwaita, MIME database, glycin loaders as the
#     tree's configs name them) in a clean environment as the launcher's (tools/gtk-icons.c).
#  2. V2 (QEMU -smp 4, 6 GiB, fbdev, CONFIG data_img_mb=64 on a stick made larger than xtask makes it, so V2 makes
#     the data volume /data at the first boot): `brave --check` names /data/home/1000/Downloads; Brave opens
#     page/download.html from the host's HTTP server, which clicks its own download link: the file arrives whole in
#     /data/home/1000/Downloads (Brave's question where to save, if it asks, is answered with its Save button,
#     Alt+S); Ctrl+S opens the save dialog, Save saves the page into the same folder; Brave's log has no GTK abort,
#     no pixbuf warning, no FATAL line; no CPU fault in Brave or the loaders (v2_download.py).
# The dbus "Failed to connect to the bus" lines are known noise (no session bus). Needs the stage
# (bash user/ports/brave-launcher/scripts/stage-v2.sh).
set -euo pipefail
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
ROOT=$PWD
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/br-40-download}"
mkdir -p "$RUN_DIR"
RUN_DIR="$(realpath "$RUN_DIR")"
LO="${BITSOS_PORT_LO:-31000}"
FWD=$((LO + 11))
PORT=$((LO + 21))
TOKEN="br7-scenario-token-3e9d"
LOG="$RUN_DIR/serial.log"
QMP="$RUN_DIR/qmp.sock"
NAME=br-download
IMG="target/xtask/$NAME.img"
# The manifest's tree (user/packaging/manifest.txt `tree brave`): the image takes this one.
STAGE=/var/tmp/claude-v2/stage/brave
# BR7_HOST_STAGE: run only the host step against another stage (a stage-v2.sh BRAVE_STAGE= trial), then stop.
[ -z "${BR7_HOST_STAGE:-}" ] || STAGE=$BR7_HOST_STAGE
V2T=/boot/BITSOS2/brave
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-$ROOT/target}
[ -x "$STAGE/brave" ] || { echo "RED: no staged Brave in $STAGE (bash user/ports/brave-launcher/scripts/stage-v2.sh)"; exit 1; }
[ -f "$STAGE/share/mime/mime.cache" ] || { echo "RED: $STAGE is older than BR-7 (no MIME database): stage again"; exit 1; }

echo "== host: GTK 3 draws the file chooser's icons from the tree"
gcc -O1 -o "$RUN_DIR/gtk-icons" host/tests/br/tools/gtk-icons.c $(pkg-config --cflags --libs gtk+-3.0)
OV=$RUN_DIR/overlay
rm -rf "$OV"; mkdir -p "$OV/glycin-loaders/2+/conf.d"
# The loader config names V2's path; on the host the same file lies in the stage.
for f in "$STAGE/share/glycin-loaders/2+/conf.d/"*.conf; do
  sed "s|=$V2T/|=$STAGE/|" "$f" >"$OV/glycin-loaders/2+/conf.d/${f##*/}"
done
# PATH as V2's: the tree's bwrap stand-in first (the host's own bwrap would sandbox the loaders instead).
rm -rf "$RUN_DIR/home"; mkdir -p "$RUN_DIR/home"
if ! env -i PATH="$STAGE/libexec:/usr/bin:/bin" HOME="$RUN_DIR/home" XDG_DATA_DIRS="$OV:$STAGE/share" \
    GDK_PIXBUF_MODULE_FILE="$STAGE/etc/gdk-pixbuf/loaders.cache" "$RUN_DIR/gtk-icons" >"$RUN_DIR/gtk-icons.txt" 2>&1; then
  echo "RED: GTK cannot load the tree's icons on the host"; cat "$RUN_DIR/gtk-icons.txt"; exit 1
fi
sed "s|$STAGE|<tree>|" "$RUN_DIR/gtk-icons.txt"
[ -z "${BR7_HOST_STAGE:-}" ] || { echo "GREEN: host step only ($STAGE)"; exit 0; }

# The page and the file to download (3 MiB, the same bytes every run), from the host.
WWW=$RUN_DIR/www
rm -rf "$WWW"; mkdir -p "$WWW"
cp host/tests/br/page/download.html "$WWW/"
python3 -I -c 'import random, sys; open(sys.argv[1], "wb").write(random.Random(7).randbytes(3 << 20))' "$WWW/br7-download.bin"
if (exec 3<>"/dev/tcp/127.0.0.1/$PORT") 2>/dev/null; then echo "RED: port $PORT already in use"; exit 1; fi
python3 -m http.server "$PORT" --bind 127.0.0.1 --directory "$WWW" >"$RUN_DIR/http.log" 2>&1 &
HTTP=$!
QPID=""
stop() {
    [ -n "$QPID" ] && kill "$QPID" 2>/dev/null || true
    pf="target/xtask/qemu-$NAME.pid"
    if [ -n "$QPID" ] && [ -f "$pf" ]; then
        pid="$(head -1 "$pf")"
        if tr '\0' ' ' < "/proc/$pid/cmdline" 2>/dev/null | grep -Eq -- "-name v2-[A-Za-z0-9]+-$NAME( |$)"; then kill "$pid" 2>/dev/null || true; fi
    fi
    kill "$HTTP" 2>/dev/null || true
    wait 2>/dev/null || true
}
trap stop EXIT
up=0
for _ in $(seq 50); do
    kill -0 "$HTTP" 2>/dev/null || break
    if curl -sf --max-time 2 "http://127.0.0.1:$PORT/download.html" | cmp -s - "$WWW/download.html"; then up=1; break; fi
    sleep 0.2
done
[ "$up" = 1 ] || { echo "RED: http server on $PORT did not serve $WWW"; exit 1; }

V2_STICK_TREES=brave cargo xtask image --name "$NAME" \
    --config-extra "token=$TOKEN;mcp_port=8765;loglevel=warn,syscall=info;screen=1280x800;data_img_mb=64"
# Room for data.img (64 MiB) and the 512 MiB V2 keeps free on the stick: the FAT image again, larger, from xtask's
# ESP tree, as host/tests/fs/50-data.sh makes it.
kib=$(( $(stat -c %s "$IMG") / 1024 + 700 * 1024 ))
rm -f "$IMG"
mkfs.fat -F 32 -n BITSOS -C "$IMG" "$kib" > /dev/null
mcopy -s -i "$IMG" "target/xtask/esp-$NAME/EFI" "target/xtask/esp-$NAME/BITSOS2" ::/
rm -f "$LOG" "$QMP" "$RUN_DIR"/dump*.ppm
cargo xtask qemu --name "$NAME" --mem 6G --net virtio --hostfwd "$FWD:8765" --timeout 900 --serial-log "$LOG" \
    -- -smp 4 -qmp "unix:$QMP,server=on,wait=off" > "$RUN_DIR/qemu.out" 2>&1 &
QPID=$!
python3 -I host/tests/br/v2_download.py "$FWD" "$TOKEN" "$QMP" "$RUN_DIR" - "$PORT"
if grep -aq "PANIC" "$LOG"; then echo "RED: the kernel panicked"; tail -20 "$LOG"; exit 1; fi
echo "GREEN: downloads and the save dialog in Brave on V2"
