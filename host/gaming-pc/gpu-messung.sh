#!/bin/bash
# GT: the RX 9070 XT capture under CachyOS on the Gaming-PC (plan/hw/gxtrace-kit.md, "Gaming-PC: the capture again").
# Only after a cold start with  modprobe.blacklist=amdgpu systemd.unit=multi-user.target  on the boot line.
# Georg runs it on the text console:  sudo bash <path of this file>
# Reads only; Linux's own amdgpu then starts the card as at every boot, with the register trace on. Before the card
# starts the events are checked to be on (if not, the card is left alone and the same boot may try again); after it
# the trace is checked (every pipe stage, the packed file, register lines in it). The last line says FERTIG only with
# a whole, good trace, else FEHLER; the Mitschrift line right above it says what is missing. A bind that runs over
# 180 s or fails, or events the full buffer dropped, make the trace UNVOLLSTÄNDIG: kept, but never FERTIG.
# Tests (no root, no GPU): GX_DRYRUN=1 GX_TRACEFS=<fake tracefs> GX_OUT=<dir> [GX_DRY_LOAD=<trace text>]
# [GX_DRY_BIND=timeout|<rc>] leaves out every step that touches the card, the PCI core or the module; GX_DRY_LOAD
# stands for what the load writes into the trace, GX_DRY_BIND for a bind that hangs or fails
# (host/tests/gx/30-trace-kit.sh).
set -u -o pipefail
here=$(dirname "$(readlink -f "${BASH_SOURCE[0]}")")
. "$here/gx-trace-check.sh"
dry=${GX_DRYRUN:-0}
t=${GX_TRACEFS:-/sys/kernel/tracing}
dev=0000:04:00.0
d=/sys/bus/pci/devices/$dev
user=${SUDO_USER:-$(id -un)}
base=${GX_OUT:-$(getent passwd "$user" | cut -d: -f6)}
stop() { echo "STOPP: $*"; exit 1; }

if [ "$dry" != 1 ]; then
	[ "$(id -u)" = 0 ] || stop "bitte mit sudo starten: sudo bash $0"
	[ "$(cat $d/device 2>/dev/null)" = 0x7550 ] || stop "die RX 9070 XT ist nicht unter $dev"
	[ -e $d/driver ] && stop "die Karte hat schon einen Treiber. Bitte mit modprobe.blacklist=amdgpu neu starten."
	# amdgpu loaded with the card still free happens only after this script stopped before the start (override none).
	if lsmod | grep -q '^amdgpu ' && [ "$(cat $d/driver_override)" != none ]; then
		stop "amdgpu ist schon geladen. Bitte mit modprobe.blacklist=amdgpu neu starten."
	fi
	[ -e "$t/trace" ] || mount -t tracefs nodev "$t"
fi
[ -e "$t/trace" ] || stop "die Mitschrift ist nicht erreichbar ($t/trace fehlt)"
# A retry in the same boot gets its own folder and tar, never a mix with the run before.
stem=$base/gp-gpu-messung-$(date +%Y%m%d-%H%M%S)
out=$stem
n=1
while [ -e "$out" ] || [ -e "$out.tar" ]; do n=$((n + 1)) && out=$stem-r$n; done
mkdir "$out" && cd "$out" || stop "kann $out nicht anlegen"
log() { echo "$*" >> trace-setup.txt; }

echo "1/4 vorher lesen"
if [ "$dry" != 1 ]; then
	lspci -nnvvv -s $dev > lspci-before.txt
	cat $d/resource > resource-before.txt
	python3 - "$d/resource5" > regs-before.txt <<'PY'
import mmap, struct, sys
f = open(sys.argv[1], "rb"); m = mmap.mmap(f.fileno(), 0x80000, prot=mmap.PROT_READ)
r = lambda b: struct.unpack_from("<I", m, b)[0]
for n, b in [("C2PMSG_33",0x58184),("C2PMSG_35",0x5818C),("C2PMSG_64",0x58200),("C2PMSG_67",0x5820C),("C2PMSG_69",0x58214),("C2PMSG_70",0x58218),("C2PMSG_71",0x5821C),("C2PMSG_81",0x58244),("SMU_C2PMSG_90",0x58A68),("RSMU_INDEX",0x50),("FB_LOCATION_BASE",0x69550),("FB_LOCATION_TOP",0x69554),("FB_OFFSET",0x6931C),("MEMSIZE",0x378C)]:
    print(n, hex(b), hex(r(b)))
PY
fi

echo "2/4 Linux startet die Karte, mit Mitschrift"
started=0
bindbad=()
if [ "$dry" != 1 ]; then
	echo none > $d/driver_override || stop "die Karte lässt sich nicht zurückhalten (driver_override)"
	modprobe amdgpu runpm=0 || stop "amdgpu lässt sich nicht laden"
	[ -e $d/driver ] && stop "amdgpu hat die Karte trotz Sperre ohne Mitschrift gestartet"
fi
if gx_reset "$t" "$(gx_buf_kb)" >> trace-setup.txt && gx_enable "$t" amdgpu amdgpu_dm gpu_scheduler >> trace-setup.txt &&
	gx_write "$t/tracing_on" 1 >> trace-setup.txt && gx_events_on "$t" >> trace-setup.txt; then
	cat "$t/set_event" >> trace-setup.txt
	if [ "$dry" = 1 ]; then
		log "dry run: no bind; the load's trace is ${GX_DRY_LOAD:-nothing}, the bind ${GX_DRY_BIND:-0}"
		[ -n "${GX_DRY_LOAD:-}" ] && cat "$GX_DRY_LOAD" > "$t/trace"
		case "${GX_DRY_BIND:-0}" in
		0) ;;
		timeout) bindbad+=("der Start lief nach 180 s noch") ;;
		*) bindbad+=("der Start schlug fehl (bind rc=$GX_DRY_BIND, bind.err)") ;;
		esac
	else
		echo > $d/driver_override
		t0=$(date +%s%N)
		echo $dev > /sys/bus/pci/drivers/amdgpu/bind 2> bind.err & # in the background, in case it never returns
		b=$!
		for _ in $(seq 1 180); do kill -0 $b 2>/dev/null || break; sleep 1; done
		# A hung init is saved as it stands after 180 s (Georg does not wait longer), marked UNVOLLSTÄNDIG.
		if kill -0 $b 2>/dev/null; then
			log "bind still running after 180 s"
			bindbad+=("der Start lief nach 180 s noch")
		else
			wait $b
			brc=$?
			log "bind rc=$brc in $((($(date +%s%N) - t0) / 1000000)) ms, driver $(basename "$(readlink $d/driver)")"
			[ $brc = 0 ] || bindbad+=("der Start schlug fehl (bind rc=$brc, bind.err)")
		fi
		sleep 8 # the IB tests run from delayed work two seconds after the init; they belong in the trace
	fi
	started=1
	gx_write "$t/tracing_on" 0 >> trace-setup.txt
	gx_trace_dump "$t" trace-load.txt.zst > trace-status.txt
	ok=$?
	[ $ok = 0 ] && ! gx_incomplete trace-status.txt "${bindbad[@]}" && ok=2
else
	gx_write "$t/tracing_on" 0 >> trace-setup.txt
	[ -n "$GX_STATUS" ] || GX_STATUS="Mitschrift nicht scharf, es fehlt: die Aufnahme ließ sich nicht einschalten (trace-setup.txt)"
	echo "$GX_STATUS" > trace-status.txt
	ok=1
fi

if [ "$dry" != 1 ] && [ $started = 1 ]; then
	echo "3/4 was Linux danach weiss"
	g=/sys/kernel/debug/dri/$dev
	[ -d $g ] || g=$(for x in /sys/kernel/debug/dri/*; do grep -q $dev $x/name 2>/dev/null && echo $x; done | head -1)
	for f in amdgpu_firmware_info amdgpu_pm_info amdgpu_gca_config amdgpu_vram_mm amdgpu_gtt_mm amdgpu_fence_info; do
		cat $g/$f > $f.txt 2>&1
	done
	cp $g/amdgpu_discovery discovery.bin 2>/dev/null
	lspci -nnvvv -s $dev > lspci-after.txt
fi
if [ "$dry" != 1 ]; then
	dmesg | grep -E "04:00.0.*(MODE1 reset|BACO reset|PCI reset|MODE2 reset|asic reset)" > reset-on-init.txt
	dmesg | grep -i amdgpu | sed -E "s/([0-9a-f]{2}:){5}[0-9a-f]{2}/xx:xx:xx:xx:xx:xx/g" > dmesg-amdgpu.txt
fi

echo "4/4 einpacken"
cd .. && tar -cf "$out.tar" "$(basename "$out")" || stop "einpacken fehlgeschlagen"
[ "$(id -u)" = 0 ] && chown -R "$user:$user" "$out" "$out.tar"
# The Mitschrift line goes right above the last line, where plan/hw/gxtrace-kit.md tells Georg to read it.
head -1 "$out/trace-status.txt"
if [ $ok = 0 ]; then
	echo "FERTIG: $out.tar  (jetzt: reboot)"
elif [ $started = 0 ]; then
	echo "FEHLER: Mitschrift nicht scharf, die Karte wurde nicht gestartet. Bitte die Mitschrift-Zeile darüber dem Desk sagen (kein Neustart nötig). Was da ist: $out.tar"
	exit 1
elif [ $ok = 2 ]; then
	echo "FEHLER: Mitschrift unvollständig, bitte die Mitschrift-Zeile darüber dem Desk sagen. Was da ist: $out.tar  (jetzt: reboot)"
	exit 1
else
	echo "FEHLER: Mitschrift leer, bitte die Mitschrift-Zeile darüber dem Desk sagen. Die anderen Dateien: $out.tar  (jetzt: reboot)"
	exit 1
fi
