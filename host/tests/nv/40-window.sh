#!/bin/bash
# time-limit: 2400
# slow
# NV-2p: one partition of a disk with Windows on it (Georg 5.11.2026). A QEMU NVMe image built like his Lexar SSD: GPT,
# Microsoft reserved, Windows data (an NTFS boot sector), a raw partition, a free rest. Never a real disk.
#  1. Refusals, one boot each: no nvme_write, no nvme_data, wrong serial, wrong GUID, moved, resized (in the key and in
#     the table), overlap, backup GPT differs, EFI partition on the disk, NTFS signature in the partition. Each: the
#     disk fully read-only (ktest nv.window.refused), /data on the stick's data.img, the image byte for byte unchanged.
#  2. The raw window: edges at first-1, first, last, last+1 and across both ends at every layer, a burst, discards,
#     format_pool (ktest nv.window.raw); every byte outside the window as it was, GPTs included. Also a boot with no
#     NVMe controller at all (nv.window.no_controller) and bitsdisk refused a second partition (nv.window.busy).
# The rest: 42-window-setup.sh, 41-window-reload.sh. Ports: NV's range (BITSOS_PORT_LO, default 19000).
set -eu
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/nv-40-window}"
mkdir -p "$RUN_DIR"
RUN_DIR="$(realpath "$RUN_DIR")"
NV="$(pwd)/host/tests/nv"
NV_RUN=nv/40-window
. "$NV/window_lib.bash"

# ---- 1. refusals, a small image: 64 MiB, MSR 2048-8191, Windows data 8192-40000, raw 40001-120000
SMALL="$RUN_DIR/small.img"
BLOCKS=131072
FIRST=40001
N=80000
OK="$SERIAL:$GUID3:$FIRST:$N"
good_small() { MKIMG real "$SMALL" $BLOCKS 512 5 msr:2048:8191 msdata:8192:40000:ntfs msdata:$FIRST:120000; }
# refusal NAME EXPECT "CONFIG" [image maker words...]
refusal() {
    local name="$1" expect="$2" cfg="$3"; shift 3
    if [ $# -gt 0 ]; then MKIMG real "$SMALL" $BLOCKS 512 5 "$@"; else good_small; fi
    local before; before=$(sum "$SMALL")
    ktests "refused-$name" "$SMALL" "nv_expect=$expect;$cfg" refused
    [ "$(sum "$SMALL")" = "$before" ] || fail "refused-$name: the image changed"
}
refusal no-write   no_nvme_write "nvme_data=$OK"
refusal no-data    no_nvme_data  "nvme_write=on"
refusal serial     serial        "nvme_write=on;nvme_data=OTHERSERIAL:$GUID3:$FIRST:$N"
refusal guid       guid          "nvme_write=on;nvme_data=$SERIAL:00000000-0000-0000-0000-00000000DEAD:$FIRST:$N"
refusal key-moved  moved         "nvme_write=on;nvme_data=$SERIAL:$GUID3:$((FIRST + 8)):$N"
refusal key-size   resized       "nvme_write=on;nvme_data=$SERIAL:$GUID3:$FIRST:$((N - 8))"
refusal img-moved  moved         "nvme_write=on;nvme_data=$OK" msr:2048:8191 msdata:8192:40000:ntfs msdata:$((FIRST + 8)):120000
refusal img-size   resized       "nvme_write=on;nvme_data=$OK" msr:2048:8191 msdata:8192:40000:ntfs msdata:$FIRST:119000
refusal overlap    overlap       "nvme_write=on;nvme_data=$OK" msr:2048:8191 msdata:8192:50000:ntfs msdata:$FIRST:120000
refusal backup     backup_gpt    "nvme_write=on;nvme_data=$OK" --backup-differs msr:2048:8191 msdata:8192:40000:ntfs msdata:$FIRST:120000
refusal efi        efi           "nvme_write=on;nvme_data=$OK" efi:2048:8191 msdata:8192:40000:ntfs msdata:$FIRST:120000
refusal ntfs       signature     "nvme_write=on;nvme_data=$OK" msr:2048:8191 msdata:8192:40000:ntfs msdata:$FIRST:120000:ntfs

# Keys set, no NVMe controller at all: /data does not wait out its bound for a word nobody would say.
good_small
NV_NO_DISK=1 ktests no-controller "$SMALL" "nvme_write=on;nvme_data=$OK" no_controller
if grep -aq "data.window_refused why=timeout" "$RUN_DIR/no-controller.log"; then fail "no-controller: /data waited out its bound"; fi

# ---- 2. the raw window on the small image
# A second raw partition with the window armed on the first: bitsdisk refuses it, /data's partition keeps its writes.
MKIMG real "$SMALL" $BLOCKS 512 5 msr:2048:8191 msdata:8192:40000:ntfs msdata:$FIRST:120000 msdata:120001:130000
OUT=$(MKIMG outside "$SMALL" $FIRST $N 512)
ktests busy "$SMALL" "nvme_write=on;nvme_data=$OK" busy
[ "$(MKIMG outside "$SMALL" $FIRST $N 512)" = "$OUT" ] || fail "busy: a byte outside the window changed"
good_small
OUT=$(MKIMG outside "$SMALL" $FIRST $N 512)
ktests raw "$SMALL" "nvme_write=on;nvme_data=$OK" raw no_key_over_files mcp_root_refused
[ "$(MKIMG outside "$SMALL" $FIRST $N 512)" = "$OUT" ] || fail "raw: a byte outside the window changed"
python3 -I - "$SMALL" $FIRST $N <<'PY' || fail "raw: nothing was written inside the window"
import hashlib, sys
f = open(sys.argv[1], "rb"); f.seek(int(sys.argv[2]) * 512)
assert f.read(8) == b"BITSVOL1", "no pool at the window's first block"
PY

echo "GREEN: refusals and the raw window; outside the window unchanged"
