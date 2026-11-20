#!/bin/bash
# slow
# GP-GUARD: the Gaming-PC profile's disk rule and its tiers (plan/spec/gaming-pc.md §2, gp_stick). OVMF with the
# board's DMI names (as 60-safemode.sh), an emulated NVMe controller, a second USB disk beside the boot stick and a
# virtio network card as the stand-in for the cable card. Image files of this run only.
# Every boot checks the NVMe and q35's own AHCI controller (untouched, never offered to a driver) and that the ACPI
# register guard still answers (its PCI snapshot holds their register ranges, read from the header, not sized).
#   A  no tier, hostile stick lines (profile=normal, nvme_write=on, nvme_bind=on, ...): the NVMe and the network
#      card stay untouched (no driver, no BAR sizing, command register as the firmware left it), the full-access
#      tick is refused, the stick and the other USB disk are read-only, every image is byte for byte what it was.
#   B  gp_net=on: the network card is driven and MCP answers from the host with the stick's token; the stick is
#      still read-only (a write over MCP fails), the NVMe still untouched.
#   C  gp_stick=on: bitsboot hands STICK.ID over and follows the normal marker rule (generation 2 gets its one
#      try), the kernel writes the stick below \BITSOS2\ (fs.fat.write, fs.fat.sysapi), the other USB disk still
#      refuses; CONFIG.TXT takes a free line but no tier line (fs.fat.tier_keys: gp_net and gp_stick are guarded
#      keys); a second boot of the unconfirmed generation falls back to generation 1, as on the Mini-PC.
#   D  gp_net=on + gp_stick=on, a normal boot: a file written over MCP lands on the stick, and the trial
#      generation confirms itself (OK) once MCP answered long enough.
#   E  gp_stick=on with a new generation whose KERNEL lacks the disk guard's mark (a kernel built before
#      GP-GUARD would bind the NVMe driver): bitsboot does not start it, does not spend its try, and starts the
#      confirmed generation instead.
#   F  the only generation lacks the mark: bitsboot stops with its error screen and says why; the same stick on a
#      machine without the profile (no DMI names) starts normally.
#   G  a loader older than the profiles (test hook old_loader: no profile, the stick's file as it is, STICK.ID
#      handed over) with profile=normal and every switch asked for: the kernel recognises the board by its DMI
#      names itself and imposes the profile without any tier.
. "$(dirname "$0")/lib.bash"
export BITSOS_KTEST_RUN="bt/65-gp-tiers"
HERE="$ROOT/host/tests/bt"
LO="${BITSOS_PORT_LO:-20500}"
FWD=$((LO + 65))
TOKEN="gp-guard-scenario-token-41c7"

INTEL=(-cpu SierraForest -smp 20,sockets=1,dies=1,modules=5,cores=4,threads=1)
GPC_DMI=(-smbios "type=1,manufacturer=ASRock,product=Z890 Taichi OCF" -smbios "type=2,manufacturer=ASRock,product=Z890 Taichi OCF")
NVME_IMG="$RUN_DIR/nvme.img"
USB_IMG="$RUN_DIR/usb2.img"
DISKS=(-drive "if=none,id=gpnv,format=raw,file=$NVME_IMG" -device "nvme,drive=gpnv,serial=GPGUARD01"
       -drive "if=none,id=gpusb,format=raw,file=$USB_IMG" -device "usb-storage,bus=xhci.0,drive=gpusb")
KTESTS="ktest=pc.optin.command_unchanged,block.guard.,block.usb.stick,fs.fat.tree"

# A 16 MiB "internal disk" and an 8 MiB second USB disk: an MBR with one Linux partition (a clean table, so only
# the profile's rule keeps them read-only) over a byte pattern.
python3 -I - "$NVME_IMG" 16 "$USB_IMG" 8 <<'PY'
import struct, sys
for path, mib in zip(sys.argv[1::2], sys.argv[2::2]):
    size = int(mib) << 20
    body = bytes((i * 7 + (i >> 9)) & 0xFF for i in range(1 << 16)) * (size >> 16)
    mbr = bytearray(body[:512])
    mbr[440:446] = b"GPGD\0\0"
    mbr[446:510] = bytes(64)
    mbr[446:462] = struct.pack("<B3sB3sII", 0, b"\xfe\xff\xff", 0x83, b"\xfe\xff\xff", 2048, (size >> 9) - 2048)
    mbr[510:512] = b"\x55\xaa"
    open(path, "wb").write(bytes(mbr) + body[512:])
PY
sum_of() { sha256sum "$1" | cut -d' ' -f1; }
NVME_SUM="$(sum_of "$NVME_IMG")"
USB_SUM="$(sum_of "$USB_IMG")"

# The two extra disks are what they were (after every boot).
disks_unchanged() {
    if [ "$(sum_of "$NVME_IMG")" = "$NVME_SUM" ]; then echo "  ok: the NVMe image is byte for byte the same after $1"; else echo "  RED: the NVMe image changed in $1"; FAILED=1; fi
    if [ "$(sum_of "$USB_IMG")" = "$USB_SUM" ]; then echo "  ok: the second USB disk is byte for byte the same after $1"; else echo "  RED: the second USB disk changed in $1"; FAILED=1; fi
}

# $1 = run tag, $2 = extended regular expression that must match no line, $3 = what it proves
refuse_re() {
    if grep -aqE -- "$2" "$(logf "$1")"; then echo "  RED: $3 (a line matches: $2)"; FAILED=1; else echo "  ok: $3"; fi
}

# $1 = run tag, $2 = who chose the profile (dmi: bitsboot, the default; kernel: the kernel itself, boot G).
# The NVMe controller is in the tree, untouched by nvme_bind, and no driver ever looked at it.
nvme_untouched() {
    local f
    f="$(grep -aoE "pci function function=[0-9a-f:.]+ vendor=0x1b36 device=0x10 class=0x10802" "$(logf "$1")" | head -1 | sed 's/.*function=\([0-9a-f:.]*\) .*/\1/')"
    if [ -z "$f" ]; then echo "  RED: the NVMe controller is not in the PCI tree of $1"; FAILED=1; return; fi
    want "$1" "pci untouched function=$f key=nvme_bind" "the NVMe controller $f is untouched by nvme_bind"
    want "$1" "pci untouched function=00:1f.2 key=nvme_bind" "q35's AHCI controller is untouched too (class 0106)"
    want_re "$1" "acpi pci.snapshot storage_bars=[1-9][0-9]* from=\"pci view\"" "the ACPI guard knows the untouched controllers' registers without sizing them"
    refuse_re "$1" "nvme probe(_refused)? function=" "the NVMe driver was never offered a controller"
    refuse_re "$1" "(ctrl|disk|name)=\"?nvme[0-9]" "no NVMe controller or disk has a name"
    want "$1" "block internal_disks.off" "the block layer knows the internal disks are off limits"
    want_re "$1" "kernel config values=\".*nvme_bind=off nvme_write=off.*profile=gaming-pc profile_reason=${2:-dmi}.*stick_write=off.*usb_msd_write=off" "the kernel holds every never-relaxed line"
    refuse_re "$1" "kernel config values=\".*nvme_(data|boot)=" "no window into an internal disk is left"
    if [ "${2:-dmi}" = dmi ]; then
        refuse "$1" "kernel profile.guard" "the loader forced everything itself (the kernel had nothing to add)"
    fi
}

# The KERNEL of generation $2 in image $1 without the disk guard's mark (one letter of every copy changed): what
# a kernel built before GP-GUARD is to bitsboot.
unmark_kernel() {
    local k="$RUN_DIR/unmarked-KERNEL"
    mcopy -o -i "$(img "$1")" "::/BITSOS2/SYS/$2/KERNEL" "$k"
    python3 -I - "$k" <<'PY2' || { echo "RED: cannot take the mark out of generation $2 of $1"; exit 1; }
import sys
mark = b"BITSOS-DISK-GUARD level=1 \xb7\x0a"
d = open(sys.argv[1], "rb").read()
n = d.count(mark)
if n == 0:
    sys.exit("the kernel carries no disk guard mark at all")
open(sys.argv[1], "wb").write(d.replace(mark, b"bitsos" + mark[6:]))
print(f"  the mark was in the kernel {n} times; this copy has none")
PY2
    mcopy -o -i "$(img "$1")" "$k" "::/BITSOS2/SYS/$2/KERNEL"
    rm -f "$k"
}

# $1 = run tag, rest = ktest names that must have passed; no test may have failed.
want_ktests() {
    local tag="$1" t; shift
    for t in "$@"; do want_re "$tag" "KTEST ok $t \(" "ktest $t passed"; done
    want_re "$tag" "KTEST done passed=[0-9]+ failed=0 " "no ktest failed"
    refuse "$tag" "KTEST FAIL" "no ktest failed (no FAIL line)"
}

# A normal (not ktest) image. $1 = name, $2 = config lines.
make_plain_image() {
    cargo xtask image --name "$1" --config-extra "$2" >"$RUN_DIR/image-$1.txt" 2>&1 \
        || { cat "$RUN_DIR/image-$1.txt"; echo "RED: cannot build image $1"; exit 1; }
}

# A normal boot reached over MCP. $1 = image, $2 = run tag, $3... = gp_mcp.py mode and arguments.
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
trap stop_own EXIT
mcp_boot() {
    local name="$1" tag="$2"; shift 2
    local log; log="$(logf "$tag")"
    echo "--- boot $tag: image $name over MCP ($*)"
    rm -f "$log"
    QNAME="$name"
    cargo xtask qemu --name "$name" --boot-retries 9 --timeout 420 --serial-log "$log" --smp 20 --net virtio --hostfwd "$FWD:8765" \
        -- "${INTEL[@]}" "${GPC_DMI[@]}" "${DISKS[@]}" >"$RUN_DIR/$tag.out" 2>&1 &
    QPID=$!
    if python3 -I "$HERE/gp_mcp.py" "$FWD" "$TOKEN" "$@"; then :; else echo "  RED: the MCP checks of $tag failed"; FAILED=1; fi
    for _ in $(seq 1 60); do kill -0 "$QPID" 2>/dev/null || break; sleep 0.5; done
    stop_own
    if grep -aqF "$TOKEN" "$log"; then echo "  RED: the token appears in the log of $tag"; FAILED=1; fi
}

# A. No tier; the stick's file asks for everything.
make_image bt-gp-a "$KTESTS;profile=normal;nvme_write=on;nvme_bind=on;nvme_data=GPGUARD01:00000000-0000-0000-0000-000000000000:2048:1000;usb_msd_write=on;stick_write=on"
mark bt-gp-a 1 OK
copy_gen bt-gp-a 2
mark bt-gp-a 2 NEW
snapshot bt-gp-a before >"$RUN_DIR/a-before.sum"
boot bt-gp-a a 120 --net virtio --smp 20 -- "${INTEL[@]}" "${GPC_DMI[@]}" "${DISKS[@]}"
want a "bitsboot profile.chosen name=gaming-pc reason=dmi: DMI names the Gaming-PC (ASRock Z890 Taichi OCF)" "the board's DMI names chose the profile"
want a "bitsboot profile.normal_refused" "CONFIG profile=normal is refused on this board, said on the screen"
want a "bitsboot profile.tiers net=off stick=off" "no tier is on"
want a "bitsboot stick.id_withheld" "STICK.ID is withheld"
want_gen a 1 FALLBACK_AFTER_TRY 2 SAFE_MODE "the confirmed generation starts; the new one keeps its try"
nvme_untouched a
want_re a "pci untouched function=[0-9a-f:.]+ key=gp_net" "the network card is untouched until gp_net=on"
want_re a "net stack.start cards=0" "no network card is driven"
want_re a "block configured .*stick_id=false stick_write=false usb_msd_write=false nvme_write=false" "every write switch is off, no stick identity"
want a "block nvme.runtime_refused" "the full-access NVMe tick is refused"
want_re a "block ktest.guard others=1 .*gp_stick=false stick_id=false" "the second USB disk and the stick refuse writes"
want_re a "fat stick.decided .*rw=false.*bitsboot handed over no" "the stick is mounted read-only"
want_ktests a pc.optin.command_unchanged block.guard.internal_off block.guard.usb_other block.usb.stick fs.fat.tree
refuse a "marker.written" "bitsboot wrote no marker"
snapshot bt-gp-a after >"$RUN_DIR/a-after.sum"
if diff "$RUN_DIR/a-before.sum" "$RUN_DIR/a-after.sum" >"$RUN_DIR/a-sum.diff"; then
    echo "  ok: the stick is byte for byte the same after boot A"
else
    echo "  RED: boot A changed the stick:"; cat "$RUN_DIR/a-sum.diff"; FAILED=1
fi
disks_unchanged "boot A"

# B. gp_net=on: the network and MCP, nothing else.
make_plain_image bt-gp-b "token=$TOKEN;mcp_port=8765;loglevel=info;gp_net=on;nvme_write=on;nvme_bind=on;usb_msd_write=on;stick_write=on"
snapshot bt-gp-b before >"$RUN_DIR/b-before.sum"
mcp_boot bt-gp-b b readonly
want b "bitsboot profile.tiers net=on stick=off" "only the network tier is on"
want b "bitsboot stick.id_withheld" "STICK.ID is still withheld"
want_re b "kernel config values=\".*gp_net=on gp_stick=off" "the kernel reads the loader's tier lines"
want_re b "net stack.start cards=1" "the network card is driven"
refuse b "key=gp_net" "no function is held back by the network rule"
nvme_untouched b
want_re b "fat stick.decided .*rw=false" "the stick is read-only"
want_re b "block configured .*stick_id=false stick_write=false usb_msd_write=false nvme_write=false" "every write switch is off"
snapshot bt-gp-b after >"$RUN_DIR/b-after.sum"
if diff "$RUN_DIR/b-before.sum" "$RUN_DIR/b-after.sum" >"$RUN_DIR/b-sum.diff"; then
    echo "  ok: the stick is byte for byte the same after boot B"
else
    echo "  RED: boot B changed the stick:"; cat "$RUN_DIR/b-sum.diff"; FAILED=1
fi
disks_unchanged "boot B"

# C. gp_stick=on: only the identified boot stick is writable; the marker rule is the Mini-PC's.
make_image bt-gp-c "ktest=pc.optin.command_unchanged,block.guard.,block.usb.stick,fs.fat.write,fs.fat.sysapi,fs.fat.tier_keys;gp_stick=on;nvme_write=on;nvme_bind=on;usb_msd_write=on;stick_write=on"
mark bt-gp-c 1 OK
copy_gen bt-gp-c 2
mark bt-gp-c 2 NEW
boot bt-gp-c c 180 --net virtio --smp 20 -- "${INTEL[@]}" "${GPC_DMI[@]}" "${DISKS[@]}"
want c "bitsboot profile.tiers net=off stick=on" "only the stick tier is on"
want c "bitsboot stick.id_handed" "STICK.ID is handed over, said on the screen"
refuse c "stick.id_withheld" "STICK.ID is not withheld"
want c "bitsboot marker.written: \\BITSOS2\\SYS\\2\\TRY" "bitsboot spent generation 2's one try"
want_gen c 2 NEWEST_TRY 0 - "the new generation gets its one try, as on the Mini-PC"
want_re c "kernel config values=\".*gp_net=off gp_stick=on.*nvme_bind=off nvme_write=off.*stick_write=off.*usb_msd_write=off" "only the tier line differs; every switch stays off"
want_re c "block configured .*stick_id=true stick_write=false usb_msd_write=false nvme_write=false" "the stick's identity is there, every switch off"
want_re c "fat stick.decided .*rw=true" "the identified stick is writable"
want_re c "block ktest.guard others=1 .*gp_stick=true stick_id=true" "the second USB disk still refuses writes"
want_re c "net stack.start cards=0" "the network stays off without gp_net"
nvme_untouched c
want_ktests c pc.optin.command_unchanged block.guard.internal_off block.guard.usb_other block.usb.stick fs.fat.write fs.fat.sysapi fs.fat.tier_keys
want c "fat config.refused key=gp_net" "CONFIG.TXT on the writable stick refuses a network tier line"
want c "fat config.refused key=gp_stick" "and a changed stick tier line"
if mtype -i "$(img bt-gp-c)" "::/BITSOS2/CONFIG.TXT" 2>/dev/null | grep -qE "^[[:space:]]*gp_net[[:space:]]*="; then
    echo "  RED: CONFIG.TXT on the stick image gained a gp_net line"; FAILED=1
else
    echo "  ok: CONFIG.TXT on the stick image has no gp_net line"
fi
want_files bt-gp-c 2 "INITRD KERNEL NEW TRY" "generation 2 carries TRY now"
if mtype -i "$(img bt-gp-c)" "::/BITSOS2/FS7CHECK.TXT" 2>/dev/null | grep -q "written by BitsOS V2 on its boot stick"; then
    echo "  ok: the file the kernel wrote is on the stick image"
else
    echo "  RED: FS7CHECK.TXT is not on the stick image"; FAILED=1
fi
disks_unchanged "boot C"
# The same stick again: generation 2 was never confirmed, so generation 1 starts.
boot bt-gp-c c2 180 --net virtio --smp 20 -- "${INTEL[@]}" "${GPC_DMI[@]}" "${DISKS[@]}"
want_gen c2 1 FALLBACK_AFTER_TRY 2 - "an unconfirmed generation falls back to the confirmed one"
refuse c2 "marker.written" "no second try is spent"
want_re c2 "fat stick.decided .*rw=true" "the stick stays writable in the fallback"
disks_unchanged "boot C2"

# D. Both tiers, a normal boot: write over MCP, and the trial generation confirms itself.
make_plain_image bt-gp-d "token=$TOKEN;mcp_port=8765;loglevel=info;gp_net=on;gp_stick=on;boot_confirm_s=10;nvme_write=on;usb_msd_write=on"
mark bt-gp-d 1 OK
copy_gen bt-gp-d 2
mark bt-gp-d 2 NEW
mcp_boot bt-gp-d d stick 30
want d "bitsboot profile.tiers net=on stick=on" "both tiers are on"
want_gen d 2 NEWEST_TRY 0 - "generation 2 starts on trial"
want_re d "fat stick.decided .*rw=true" "the stick is writable"
want_re d "kexec boot.confirmed generation=2" "the trial generation confirmed itself after MCP answered"
nvme_untouched d
want_files bt-gp-d 2 "INITRD KERNEL OK" "generation 2 is OK on the stick"
if mtype -i "$(img bt-gp-d)" "::/BITSOS2/GPTEST.TXT" 2>/dev/null | grep -q "GP-GUARD: written over MCP"; then
    echo "  ok: the file written over MCP is on the stick image"
else
    echo "  RED: GPTEST.TXT is not on the stick image"; FAILED=1
fi
disks_unchanged "boot D"

# E. A generation older than the disk guard is not started under the profile (gp_stick=on, so it would get its try).
make_image bt-gp-e "ktest=pc.optin.command_unchanged,block.guard.;gp_stick=on"
mark bt-gp-e 1 OK
copy_gen bt-gp-e 2
unmark_kernel bt-gp-e 2
mark bt-gp-e 2 NEW
boot bt-gp-e e 180 --net virtio --smp 20 -- "${INTEL[@]}" "${GPC_DMI[@]}" "${DISKS[@]}"
want e "bitsboot generation.unguarded number=2" "bitsboot sees that generation 2's kernel is older than the disk guard"
want_re e "bitsboot generation.bad_files number=2: .*older than the disk guard, not started on this machine" "and says so in plain words"
want_gen e 1 FALLBACK_BAD_FILES 2 - "the confirmed generation starts instead"
refuse e "marker.written" "the unguarded generation's try is not spent"
want_files bt-gp-e 2 "INITRD KERNEL NEW" "generation 2 is untouched on the stick"
nvme_untouched e
want_ktests e pc.optin.command_unchanged block.guard.internal_off block.guard.usb_other
disks_unchanged "boot E"

# F. Nothing but an unguarded kernel on the stick: no start at all on the board, a normal start elsewhere.
make_image bt-gp-f ""
unmark_kernel bt-gp-f 1
snapshot bt-gp-f before >"$RUN_DIR/f-before.sum"
boot bt-gp-f f 60 --net virtio --smp 20 -- "${INTEL[@]}" "${GPC_DMI[@]}" "${DISKS[@]}"
want f "bitsboot generation.unguarded number=1" "the only generation is older than the disk guard"
want f "older than the disk guard, not started on this machine" "the error screen says why"
want f "eingebauten Platten" "and says it in German for Georg"
want_refusal f
refuse_re f "pci (function|untouched) " "no kernel ever looked at the PCI bus"
snapshot bt-gp-f after >"$RUN_DIR/f-after.sum"
if diff "$RUN_DIR/f-before.sum" "$RUN_DIR/f-after.sum" >"$RUN_DIR/f-sum.diff"; then
    echo "  ok: the stick is byte for byte the same after the refused start"
else
    echo "  RED: the refused start changed the stick:"; cat "$RUN_DIR/f-sum.diff"; FAILED=1
fi
disks_unchanged "boot F"
boot bt-gp-f f2 90 --net none
want_re f2 "bitsboot profile.chosen name=normal reason=hypervisor" "without the board's names the profile does not apply"
refuse f2 "generation.unguarded" "the mark is asked for only under the profile"
refuse f2 "kernel profile.guard" "the kernel imposes nothing on another machine"
refuse f2 "kernel profile.missing" "nor does it take QEMU for a loader without a profile"
want_kernel_ok f2

# G. A loader that knows no profile, the board's DMI names, profile=normal and every switch on the stick: the
#    kernel does not take the profile= line for the loader's word.
make_image bt-gp-g "$KTESTS;bitsboot.test=old_loader;profile=normal;nvme_write=on;nvme_bind=on;nvme_data=GPGUARD01:00000000-0000-0000-0000-000000000000:2048:1000;usb_msd_write=on;stick_write=on;gp_net=on;gp_stick=on"
mark bt-gp-g 1 OK
snapshot bt-gp-g before >"$RUN_DIR/g-before.sum"
boot bt-gp-g g 120 --net virtio --smp 20 -- "${INTEL[@]}" "${GPC_DMI[@]}" "${DISKS[@]}"
want g "bitsboot profile.skipped" "bitsboot chose no profile (the stand-in for an older loader)"
refuse g "bitsboot profile.chosen" "no profile line came from the loader"
refuse g "bitsboot stick.id_withheld" "the loader handed STICK.ID over, as an older one does"
want_re g "kernel profile.guard reason=dmi record=normal" "the kernel recognised the board by its DMI names and imposed the profile"
want_re g "kernel config values=\".*gp_net=off gp_stick=off.*nvme_bind=off nvme_write=off.*profile=gaming-pc profile_reason=kernel profile_refused=normal.*stick_write=off.*usb_msd_write=off" "everything is off, no tier, whatever the stick asked for"
nvme_untouched g kernel
want_re g "pci untouched function=[0-9a-f:.]+ key=gp_net" "the network card is untouched"
want_re g "net stack.start cards=0" "no network card is driven"
want_re g "block configured .*stick_id=false stick_write=false usb_msd_write=false nvme_write=false" "every write switch is off and the handed-over STICK.ID is dropped"
want g "block nvme.runtime_refused" "the full-access NVMe tick is refused"
want_re g "fat stick.decided .*rw=false" "the stick is mounted read-only"
want_ktests g pc.optin.command_unchanged block.guard.internal_off block.guard.usb_other block.usb.stick fs.fat.tree
snapshot bt-gp-g after >"$RUN_DIR/g-after.sum"
if diff "$RUN_DIR/g-before.sum" "$RUN_DIR/g-after.sum" >"$RUN_DIR/g-sum.diff"; then
    echo "  ok: the stick is byte for byte the same after boot G"
else
    echo "  RED: boot G changed the stick:"; cat "$RUN_DIR/g-sum.diff"; FAILED=1
fi
disks_unchanged "boot G"
finish
