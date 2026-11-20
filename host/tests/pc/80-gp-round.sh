#!/bin/bash
# slow
# GP-ROUND: the rehearsal of the first stick round on the Gaming-PC (plan/guides/gaming-pc.md, network on). OVMF with
# the board's DMI names, 20 Intel CPUs, a pxb-pcie second root at bus 0x80 (as the board's) with an xHCI, a USB
# keyboard and a second USB disk behind it, an NVMe standing in for the internal SSDs, virtio-net as the cable card.
# The stick says gp_net=on and gp_stick=on, as the round's stick will. No key is sent until the page is checked.
#   R  the report page appears by itself and holds every group; MCP answers with the same text (hw_info section
#      report); the page turns by itself; keys on the second root's keyboard turn it and then give the shell, which
#      lists /boot/BITSOS2; the report file lands on the stick below \BITSOS2\REPORT\ and says what MCP said; the
#      NVMe and the second USB disk are byte for byte what they were.
#   N  the same machine without gp_net (twice): no network, the network rows say why, the file is written anyway
#      (the evidence of a round without network), and the second boot keeps the first boot's file beside its own.
#      GP-R2: the remote desk's firmware path: the gaming-pc firmware set (xtask stick --firmware gaming-pc --tree)
#      goes onto the running machine's stick with push.mjs --add --via write (gp_stick=on), next to the firmware the
#      image already holds, which stays; afterwards the stick image holds every file of the set byte for byte.
#   W  the same stick with its clone plugged in behind the second root: the stick is only read, the page comes in
#      time all the same and its file row says why nothing was written (the page never waits for the stick).
. "$(dirname "$0")/../bt/lib.bash"
export BITSOS_KTEST_RUN="pc/80-gp-round"
HERE="$ROOT/host/tests/pc"
LO="${BITSOS_PORT_LO:-22500}"
FWD=$((LO + 80))
TOKEN="gp-round-scenario-token-7d21"
# xtask takes sockets only below the worktree's target; the run folder's path stays far below the 108-byte limit.
QMP="$RUN_DIR/qmp.sock"

INTEL=(-cpu SierraForest -smp 20,sockets=1,dies=1,modules=5,cores=4,threads=1)
GPC_DMI=(-smbios "type=1,manufacturer=ASRock,product=Z890 Taichi OCF" -smbios "type=2,manufacturer=ASRock,product=Z890 Taichi OCF")
NVME_IMG="$RUN_DIR/nvme.img"
USB_IMG="$RUN_DIR/usb2.img"
# No PS/2 (i8042=off): the one keyboard of the machine is the USB one behind the second root, so every key QMP
# sends arrives through 81:00.0's xHCI, its HID driver and the console, as Georg's keyboard on the board's bus 80.
SECOND_ROOT=(-machine i8042=off
             -device pxb-pcie,id=pxb80,bus_nr=0x80,bus=pcie.0
             -device pcie-root-port,id=rp80,bus=pxb80,addr=0x14,chassis=80,slot=80
             -device qemu-xhci,id=x80,bus=rp80
             -device usb-kbd,id=kbd80,bus=x80.0,port=1
             -drive "if=none,id=gpusb,format=raw,file=$USB_IMG" -device "usb-storage,bus=x80.0,port=2,drive=gpusb")
NVME=(-drive "if=none,id=gpnv,format=raw,file=$NVME_IMG" -device "nvme,drive=gpnv,serial=GPROUND01")

python3 -I - "$NVME_IMG" 16 "$USB_IMG" 8 <<'PY'
import sys
for path, mib in zip(sys.argv[1::2], sys.argv[2::2]):
    open(path, "wb").write(bytes((i * 11 + (i >> 9)) & 0xFF for i in range(1 << 16)) * (int(mib) << 4))
PY
sum_of() { sha256sum "$1" | cut -d' ' -f1; }
NVME_SUM="$(sum_of "$NVME_IMG")"
USB_SUM="$(sum_of "$USB_IMG")"
disks_unchanged() {
    if [ "$(sum_of "$NVME_IMG")" = "$NVME_SUM" ]; then echo "  ok: the NVMe image is byte for byte the same after $1"; else echo "  RED: the NVMe image changed in $1"; FAILED=1; fi
    if [ "$(sum_of "$USB_IMG")" = "$USB_SUM" ]; then echo "  ok: the second USB disk is byte for byte the same after $1"; else echo "  RED: the second USB disk changed in $1"; FAILED=1; fi
}

make_plain_image() {
    cargo xtask image --name "$1" --config-extra "$2" >"$RUN_DIR/image-$1.txt" 2>&1 \
        || { cat "$RUN_DIR/image-$1.txt"; echo "RED: cannot build image $1"; exit 1; }
}

QPID=""
stop_own() {
    if [ -n "$QPID" ] && kill -0 "$QPID" 2>/dev/null; then
        local q; q="$(head -1 "$X/qemu-$QNAME.pid" 2>/dev/null || true)"
        case "$q" in ''|*[!0-9]*) ;; *) kill "$q" 2>/dev/null || true ;; esac
        kill "$QPID" 2>/dev/null || true
        wait "$QPID" 2>/dev/null || true
    fi
    QPID=""
}
cleanup() { stop_own; rm -f "$QMP"; }
trap cleanup EXIT

# Extra QEMU arguments of the next boot (boot W's clone).
EXTRA=()
# $1 = image, $2 = run tag, $3 = net option of xtask qemu.
start_qemu() {
    local name="$1" tag="$2" net="$3"
    rm -f "$(logf "$tag")" "$QMP"
    QNAME="$name"
    cargo xtask qemu --name "$name" --boot-retries 9 --timeout 420 --serial-log "$(logf "$tag")" --smp 20 --net "$net" --hostfwd "$FWD:8765" \
        -- "${INTEL[@]}" "${GPC_DMI[@]}" "${NVME[@]}" "${SECOND_ROOT[@]}" "${EXTRA[@]}" -qmp "unix:$QMP,server=on,wait=off" >"$RUN_DIR/$tag.out" 2>&1 &
    QPID=$!
}

# Waits up to $2 s for the fixed string $3 in run $1's log.
wait_for() {
    local i
    for i in $(seq 1 $(( $2 * 2 ))); do
        grep -aqF -- "$3" "$(logf "$1")" 2>/dev/null && return 0
        kill -0 "$QPID" 2>/dev/null || return 1
        sleep 0.5
    done
    return 1
}

quit_qemu() {
    python3 -I "$ROOT/host/tests/cp/qmp_hmp.py" "$QMP" --quit >/dev/null 2>&1 || true
    for _ in $(seq 1 40); do kill -0 "$QPID" 2>/dev/null || break; sleep 0.5; done
    stop_own
}

# Seconds since kernel start of the first log line of run $1 matching the fixed string $2 (empty if none).
secs_of() {
    grep -aF -- "$2" "$(logf "$1")" 2>/dev/null | head -1 | sed -n 's/^\[ *\([0-9]*\)\..*/\1/p'
}
# The page came by itself in time and before the stick's file: waited_s at most 46 (the network's bound is 45 s),
# the page shown at most 120 s after the kernel started, and no file line before the page.
page_in_time() {
    local w shown file
    w="$(grep -aoE 'I kernel gp.report rows=[0-9]+ waited_s=[0-9]+' "$(logf "$1")" | head -1 | sed 's/.*waited_s=//')"
    shown="$(secs_of "$1" "I console report.shown")"
    file="$(secs_of "$1" "I kernel gp.report.file path=")"
    if [ -n "$w" ] && [ "$w" -le 46 ]; then echo "  ok: $1: the page waited $w s for the network (at most 46)"; else echo "  RED: $1: the page waited '${w}' s"; FAILED=1; fi
    if [ -n "$shown" ] && [ "$shown" -le 120 ]; then echo "  ok: $1: the page was on the screen $shown s after the kernel started"; else echo "  RED: $1: the page came at '${shown}' s"; FAILED=1; fi
    local ls lf
    ls="$(grep -anF "I console report.shown" "$(logf "$1")" | head -1 | cut -d: -f1)"
    lf="$(grep -anF "I kernel gp.report.file path=" "$(logf "$1")" | head -1 | cut -d: -f1)"
    if [ -z "$lf" ] || { [ -n "$ls" ] && [ "$ls" -lt "$lf" ]; }; then echo "  ok: $1: the page came before the stick's file${file:+ (file at $file s)}"; else echo "  RED: $1: the stick's file was written before the page"; FAILED=1; fi
}

# The report file $2 from image $1, LF line ends.
report_file() {
    mtype -i "$(img "$1")" "::/BITSOS2/REPORT/$2" 2>/dev/null | tr -d '\r'
}

# R. The round as it will be: gp_net=on and gp_stick=on.
make_plain_image gp-round-r "token=$TOKEN;mcp_port=8765;loglevel=info;gp_net=on;gp_stick=on;data_img_mb=0"
echo "--- boot r: the round with network and a writable stick"
start_qemu gp-round-r r virtio
# The remote desk's package as the Mini-PC under Windows gets it: a copy in a path with spaces, every text file with
# CR LF line ends, the token in a file outside the copy. Its self-test and the check list's steps that hold in QEMU
# too (the rest names the real machine's parts) run against this boot before anything else touches it.
DESK="$RUN_DIR/remote desk copy"
rm -rf "$DESK"; mkdir -p "$DESK/host"
cp -r "$ROOT/host/mcp" "$ROOT/host/remote-desk" "$DESK/host/"
rm -rf "$DESK/host/remote-desk/out"
find "$DESK" -type f \( -name '*.mjs' -o -name '*.json' -o -name '*.md' \) -exec sed -i 's/\r*$/\r/' {} +
mkdir -p "$RUN_DIR/desk home"
printf '%s\r\n' "$TOKEN" >"$RUN_DIR/desk home/token.txt"
chmod 600 "$RUN_DIR/desk home/token.txt"
for _ in $(seq 1 240); do
    node "$DESK/host/mcp/mcp-call.mjs" "127.0.0.1:$FWD" version --token-file "$RUN_DIR/desk home/token.txt" >/dev/null 2>&1 && break
    sleep 1
done
# The page and its text come up to a minute after MCP answers (the network's self-check first): wait for it, as
# the remote desk does when step 03 says "not ready yet".
wait_for r 150 "I kernel gp.report rows=" || echo "  (no report line in the log yet; collect.mjs will say so)"
if node "$DESK/host/remote-desk/selftest.mjs" --host "127.0.0.1:$FWD" --token-file "$RUN_DIR/desk home/token.txt" >"$RUN_DIR/desk-selftest.txt" 2>&1; then
    echo "  ok: the remote desk's self-test is green from a CR LF copy in a path with spaces"
else
    echo "  RED: the remote desk's self-test:"; cat "$RUN_DIR/desk-selftest.txt"; FAILED=1
fi
if node "$DESK/host/remote-desk/collect.mjs" "127.0.0.1:$FWD" --token-file "$RUN_DIR/desk home/token.txt" --out "$RUN_DIR/desk out" --only 01,02,03,05,06,25 --confirm-wait-s 0 >"$RUN_DIR/desk-collect.txt" 2>&1; then
    echo "  ok: the check list's QEMU-valid steps are green through collect.mjs ($(grep -c '^ok ' "$RUN_DIR/desk-collect.txt") steps)"
else
    echo "  RED: collect.mjs:"; cat "$RUN_DIR/desk-collect.txt"; FAILED=1
fi
if node "$DESK/host/mcp/mcp-call.mjs" "127.0.0.1:$FWD" hw_info "@$DESK/host/remote-desk/args/03-report.json" --token-file "$RUN_DIR/desk home/token.txt" >"$RUN_DIR/desk-oneline.txt" 2>&1 \
    && grep -q "BitsOS-Bericht" "$RUN_DIR/desk-oneline.txt"; then
    echo "  ok: the one-line node call with an arguments file answers the report"
else
    echo "  RED: the one-line node call:"; head -5 "$RUN_DIR/desk-oneline.txt"; FAILED=1
fi
# GP-R2: the firmware set as the remote desk pushes it (no listening port on Windows: --via write).
FWTREE="$RUN_DIR/fw tree"
rm -rf "$FWTREE"
mtype -i "$(img gp-round-r)" ::/BITSOS2/firmware/rtw89/rtw8852b_fw-2.bin >"$RUN_DIR/fw-kept-before.bin" 2>/dev/null || true
if cargo xtask stick --firmware gaming-pc --tree "$FWTREE" >"$RUN_DIR/fw-tree.txt" 2>&1 \
    && node "$DESK/host/mcp/push.mjs" "127.0.0.1:$FWD" --tree "$FWTREE/firmware" --name firmware --add --via write --token-file "$RUN_DIR/desk home/token.txt" >"$RUN_DIR/fw-push.txt" 2>&1; then
    echo "  ok: the firmware set went onto the running machine's stick ($(grep -c ' sha256 ' "$RUN_DIR/fw-tree.txt") files; $(tail -1 "$RUN_DIR/fw-push.txt" | cut -c1-160))"
else
    echo "  RED: the firmware push:"; tail -5 "$RUN_DIR/fw-tree.txt" "$RUN_DIR/fw-push.txt"; FAILED=1
fi
if python3 -I "$HERE/gp_round_check.py" "$FWD" "$TOKEN" "$QMP" "$RUN_DIR"; then :; else echo "  RED: the MCP and keyboard checks of r failed"; FAILED=1; fi
for _ in $(seq 1 60); do kill -0 "$QPID" 2>/dev/null || break; sleep 0.5; done
stop_own
want r "bitsboot profile.chosen name=gaming-pc reason=dmi" "the board's DMI names chose the profile"
want r "bitsboot profile.tiers net=on stick=on" "both tiers are on"
want_re r "I console desktop.mode on=false why=off" "the desktop stays off under the profile: the screen is the page, then the shell"
want_re r "I usb peer.checked|I pci bound driver=xhci function=81:00.0" "the second root's xHCI is driven"
want_re r "I console report.shown pages=[1-9] cols=1[0-9][0-9] rows=[4-9][0-9]" "the page was shown on the 2560x1440 console grid"
want_re r "I kernel gp.report rows=[0-9]+ waited_s=[0-9]+ net_timed_out=false" "the page came once the network said where it stands"
want r "I kernel gp.report.file path=/boot/BITSOS2/REPORT/GP000001.TXT" "the report file was written below \\BITSOS2\\REPORT"
want_re r "I console report.ended reason=key" "a key ended the page"
want r "I kernel gp.report.file_row file=\"Bericht auf dem Stick: \\\\BITSOS2\\\\REPORT\\\\GP000001.TXT\"" "the file thread's answer renewed the page's file row"
page_in_time r
want_re r "I input device.registered number=[0-9]+ name=\"USB HID 0627:0001\" phys=xhci1-" "the keyboard is the USB one behind the second root"
want_re r "I usb xhci.probe ctrl=xhci1 function=81:00.0" "that controller is 81:00.0, on the second root"
want_re r "I smp done cpus=20" "the profile's raised lines are in the log (smp done)"
if grep -aqF "$TOKEN" "$(logf r)"; then echo "  RED: the token appears in the log of r"; FAILED=1; fi
report_file gp-round-r GP000001.TXT >"$RUN_DIR/file-r.txt"
if [ -s "$RUN_DIR/file-r.txt" ]; then echo "  ok: GP000001.TXT is on the stick image"; else echo "  RED: no GP000001.TXT on the stick image"; FAILED=1; fi
# The file says what MCP said (MCP's answer starts with its section line; the rows are the same).
sed '1d' "$RUN_DIR/mcp-report.txt" 2>/dev/null >"$RUN_DIR/mcp-r.txt" || true
if [ -s "$RUN_DIR/mcp-r.txt" ] && diff <(sed '1d' "$RUN_DIR/file-r.txt") <(sed '1d' "$RUN_DIR/mcp-r.txt") >"$RUN_DIR/r-file-vs-mcp.diff"; then
    echo "  ok: the file on the stick holds the same rows as MCP's answer"
else
    echo "  RED: file and MCP answer differ (r-file-vs-mcp.diff):"; head -20 "$RUN_DIR/r-file-vs-mcp.diff"; FAILED=1
fi
disks_unchanged "boot r"
# The set on the stick image, byte for byte, and the image's own firmware beside it unchanged.
fw_bad=0
while IFS= read -r f; do
    rel="${f#"$FWTREE/firmware/"}"
    if [ "$(sha256sum "$f" | cut -c1-64)" != "$(mtype -i "$(img gp-round-r)" "::/BITSOS2/firmware/$rel" 2>/dev/null | sha256sum | cut -c1-64)" ]; then fw_bad=1; echo "  RED: \\BITSOS2\\firmware\\$rel differs on the stick image"; fi
done < <(find "$FWTREE/firmware" -type f)
if [ "$fw_bad" = 0 ] && [ -n "$(find "$FWTREE/firmware" -type f)" ]; then echo "  ok: every file of the set is on the stick image byte for byte"; else FAILED=1; fi
if [ -s "$RUN_DIR/fw-kept-before.bin" ]; then
    if mtype -i "$(img gp-round-r)" ::/BITSOS2/firmware/rtw89/rtw8852b_fw-2.bin | cmp -s - "$RUN_DIR/fw-kept-before.bin"; then echo "  ok: the image's own firmware stayed beside the set (--add removes nothing)"; else echo "  RED: --add removed or changed the image's own firmware"; FAILED=1; fi
else
    echo "  RED: the image carries no rtw89 firmware to see --add keep it"; FAILED=1
fi

# N. No network tier: the rows say why; the file is the round's evidence; the newest three are kept.
make_plain_image gp-round-n "token=$TOKEN;mcp_port=8765;loglevel=info;gp_stick=on;data_img_mb=0"
for tag in n1 n2; do
    echo "--- boot $tag: no network tier"
    start_qemu gp-round-n "$tag" virtio
    if wait_for "$tag" 200 "I kernel gp.report.file path="; then :; else echo "  RED: no report file in $tag"; FAILED=1; fi
    quit_qemu
    want "$tag" "bitsboot profile.tiers net=off stick=on" "only the stick tier is on"
    want_re "$tag" "I console report.shown pages=[1-9]" "the page was shown"
    want_re "$tag" "net stack.start cards=0" "no network card is driven"
done
want n1 "I kernel gp.report.file path=/boot/BITSOS2/REPORT/GP000001.TXT" "the first boot writes GP000001.TXT"
want n2 "I kernel gp.report.file path=/boot/BITSOS2/REPORT/GP000002.TXT" "the second boot writes GP000002.TXT beside it"
report_file gp-round-n GP000002.TXT >"$RUN_DIR/file-n.txt"
for row in "Netzwerk: aus (Kabel und WLAN)." "Netzwerkkarten: vom sicheren Modus abgeschaltet (auf dem Stick steht kein gp_net=on)" \
           "MCP nicht erreichbar: das Netzwerk ist aus" "Bericht auf dem Stick: \\BITSOS2\\REPORT\\GP000002.TXT"; do
    if grep -qF -- "$row" "$RUN_DIR/file-n.txt"; then echo "  ok: the file says: $row"; else echo "  RED: the file of n2 lacks: $row"; FAILED=1; fi
done
if report_file gp-round-n GP000001.TXT | grep -qF "Netzwerk: aus"; then echo "  ok: the first boot's file is kept"; else echo "  RED: GP000001.TXT is gone after the second boot"; FAILED=1; fi
disks_unchanged "boots n1, n2"
for tag in n1 n2; do page_in_time "$tag"; done

# W. The stick with its clone beside it: only read; the page comes anyway and says why no file was written.
echo "--- boot w: the stick is only read (its clone is plugged in)"
CLONE="$RUN_DIR/clone.img"
cp "$(img gp-round-n)" "$CLONE"
CLONE_SUM="$(sum_of "$CLONE")"
EXTRA=(-drive "if=none,id=gpclone,format=raw,file=$CLONE" -device "usb-storage,bus=x80.0,port=3,drive=gpclone")
start_qemu gp-round-n w virtio
if wait_for w 200 "I kernel gp.report.file_row"; then :; else echo "  RED: no file row answer in w"; FAILED=1; fi
quit_qemu
EXTRA=()
want_re w "W fat stick.read_only|I fat stick.decided .*rw=false" "the clone turned the stick's writing off"
want_re w "I console report.shown pages=[1-9]" "the page was shown"
want w "W kernel gp.report.file_failed why=read_only" "the file thread found the stick only read"
want w "I kernel gp.report.file_row file=\"Bericht auf dem Stick: nicht geschrieben (der Stick wird nur gelesen)\"" "the page's row says why"
page_in_time w
if mtype -i "$(img gp-round-n)" ::/BITSOS2/REPORT/GP000003.TXT >/dev/null 2>&1; then echo "  RED: w wrote a file to a stick it may only read"; FAILED=1; else echo "  ok: w wrote no file"; fi
if [ "$(sum_of "$CLONE")" = "$CLONE_SUM" ]; then echo "  ok: the clone is byte for byte the same after w"; else echo "  RED: the clone changed in w"; FAILED=1; fi
disks_unchanged "boot w"
finish
