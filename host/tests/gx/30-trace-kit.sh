#!/bin/bash
# GT: the GPU capture's register-trace checks (host/gaming-pc/gx-trace-check.sh, used by host/gaming-pc/gpu-messung.sh
# and the Mini-PC kit) against a fake tracefs, no root, no GPU, no QEMU: a good trace packs and passes; a header-only
# trace, events that are not on, a failing pipe stage and a size-0 stat() source each give their one plain line and
# never a silent empty file; lost events and a hung or failed bind give UNVOLLSTÄNDIG, never FERTIG; the plain line
# sits right above the last line; a retry gets its own folder. The decoder (gx-trace-decode.py) and the fixture generator
# of rdna4::linuxseq (gx-trace-seq.py) on synthetic traces. The measurement run's evaluator (gx-messung-eval.py) on protocols
# the program wrote on the fake card (host/tests/gx/fixtures/gpu-messung-*.txt). In every gate through the amdgpu host test `trace_kit`.
# Case e's first step (the 16.11. command fails on a size-0 file) is evidence only: it depends on the host's zstd
# (1.5.7 fails), so a zstd that packs it prints a note; the checked form's steps are asserted on every host.
set -u
ROOT=${BITSOS_ROOT:-$(cd "$(dirname "$0")/../../.." && pwd)}
if [ -n "${BITSOS_RUN_DIR:-}" ]; then RUN=$BITSOS_RUN_DIR; else
	RUN=$(mktemp -d)
	trap 'rm -rf "$RUN"' EXIT
fi
CAP=$ROOT/host/gaming-pc/gpu-messung.sh
SAMPLE=$ROOT/host/tests/gx/fixtures/trace-sample.txt
for tool in bash zstd tar awk python3 sha256sum; do
	command -v $tool > /dev/null || { echo "RED: $tool missing (install $tool)"; exit 1; }
done
[ -f "$SAMPLE" ] || { echo "RED: $SAMPLE missing"; exit 1; }
. "$ROOT/host/gaming-pc/gx-trace-check.sh"
fail=0
ok() { echo "ok   $1"; }
bad() { echo "FAIL $1"; fail=1; }
check() { if eval "$2"; then ok "$1"; else bad "$1"; fi; }

# fake <dir> <set_event lines> [lost on cpu1]: what tracefs shows after amdgpu's module is loaded.
fake() {
	local t=$1 lost=${3:-0}
	rm -rf "$t"
	mkdir -p "$t/options" "$t/per_cpu/cpu0" "$t/per_cpu/cpu1" "$t/events/amdgpu" "$t/events/amdgpu_dm" "$t/events/gpu_scheduler"
	echo 1 > "$t/tracing_on"
	echo 7 > "$t/buffer_size_kb"
	echo 1 > "$t/options/overwrite"
	echo "stale line from before" > "$t/trace"
	printf '%s\n' "$2" > "$t/set_event"
	printf 'entries: 0\noverrun: 0\ncommit overrun: 0\ndropped events: 0\n' > "$t/per_cpu/cpu0/stats"
	printf 'entries: 0\noverrun: 0\ncommit overrun: 0\ndropped events: %d\n' "$lost" > "$t/per_cpu/cpu1/stats"
}
EVENTS=$'amdgpu:amdgpu_device_rreg\namdgpu:amdgpu_device_wreg\namdgpu:amdgpu_iv'

# capture <name> <set_event> <load> [lost]: the Gaming-PC script in its dry run (GX_DRY_BIND from the caller); sets
# OUT (its folder), LOG and MLINE (the line above the last one, where Georg reads the Mitschrift line).
capture() {
	fake "$RUN/$1/tracefs" "$2" "${4:-0}"
	mkdir -p "$RUN/$1/out"
	LOG=$RUN/$1/log.txt
	GX_DRYRUN=1 GX_TRACEFS="$RUN/$1/tracefs" GX_OUT="$RUN/$1/out" GX_DRY_LOAD="$3" GX_DRY_BIND="${GX_DRY_BIND:-0}" \
		PATH="${CAP_PATH:-$PATH}" bash "$CAP" > "$LOG" 2>&1 < /dev/null
	RC=$?
	OUT=$(ls -d "$RUN/$1/out"/gp-gpu-messung-*/ 2>/dev/null | head -1)
	MLINE=$(tail -2 "$LOG" | head -1)
	sed 's/^/     | /' "$LOG"
}

echo "== a: a good trace"
capture a "$EVENTS" "$SAMPLE"
check "a: exit 0" '[ $RC = 0 ]'
check "a: last line FERTIG" 'tail -1 "$LOG" | grep -q "^FERTIG: "'
check "a: the plain line counts 24 lines, 11 writes, 13 reads, 0 lost" \
	'grep -qx "Mitschrift: OK, 24 Zeilen (11 Schreib-, 13 Lesezugriffe), verloren 0" "$OUT/trace-status.txt"'
check "a: the line above the last one is the plain line" '[ "$MLINE" = "$(head -1 "$OUT/trace-status.txt")" ]'
check "a: the packed trace is the load's text" 'zstd -qdc "$OUT/trace-load.txt.zst" | cmp -s - "$SAMPLE"'
check "a: the tar holds the trace and its status" \
	'tar -tf "${OUT%/}.tar" | grep -q "trace-load.txt.zst" && tar -tf "${OUT%/}.tar" | grep -q "trace-status.txt"'
check "a: the arm wrote overwrite 0 and a buffer size" \
	'[ "$(cat "$RUN/a/tracefs/options/overwrite")" = 0 ] && [ "$(cat "$RUN/a/tracefs/buffer_size_kb")" -ge 16384 ]'
check "a: every system's events were switched on" \
	'[ "$(cat "$RUN"/a/tracefs/events/{amdgpu,amdgpu_dm,gpu_scheduler}/enable | tr -d "\n")" = 111 ]'
check "a: tracing is off after the dump" '[ "$(cat "$RUN/a/tracefs/tracing_on")" = 0 ]'

echo "== b: only the header"
grep '^#' "$SAMPLE" > "$RUN/header.txt"
capture b "$EVENTS" "$RUN/header.txt"
check "b: exit 1" '[ $RC = 1 ]'
check "b: the LEER line, right above the last line" '[[ $MLINE == "Mitschrift LEER: keine einzige Zeile"* ]]'
check "b: last line FEHLER, no FERTIG" 'tail -1 "$LOG" | grep -q "^FEHLER: Mitschrift leer" && ! grep -q "^FERTIG" "$LOG"'
check "b: the rest is still packed" 'tar -tf "${OUT%/}.tar" | grep -q "trace-setup.txt"'

echo "== c: the register events are not on"
capture c "amdgpu_dm:amdgpu_dc_rreg" "$SAMPLE"
check "c: exit 1" '[ $RC = 1 ]'
check "c: the nicht-scharf line names both register events, right above the last line" \
	'[ "$MLINE" = "Mitschrift nicht scharf, es fehlt: Lesezugriffe (amdgpu:amdgpu_device_rreg), Schreibzugriffe (amdgpu:amdgpu_device_wreg)" ]'
check "c: the card is not started (no bind, no trace taken)" \
	'! grep -q "no bind" "$OUT/trace-setup.txt" && [ ! -e "$OUT/trace-load.txt.zst" ] && [ "$(cat "$RUN/c/tracefs/trace")" = "" ]'
check "c: last line says the card was not started" 'tail -1 "$LOG" | grep -q "^FEHLER: Mitschrift nicht scharf, die Karte wurde nicht gestartet"'
check "c: tracing is switched off again" '[ "$(cat "$RUN/c/tracefs/tracing_on")" = 0 ]'

echo "== c2: tracing_on does not stay on, no buffer"
fake "$RUN/c2" "$EVENTS"
echo 0 > "$RUN/c2/tracing_on"
echo 0 > "$RUN/c2/buffer_size_kb"
gx_events_on "$RUN/c2" > /dev/null
rc=$?
check "c2: refused, both named" '[ $rc = 1 ] && [ "$GX_STATUS" = "Mitschrift nicht scharf, es fehlt: der Puffer (buffer_size_kb), die Aufnahme (tracing_on)" ]'
fake "$RUN/c3" $'gx:gx_rreg\ngx:gx_rreg_ret\ngx:gx_wreg'
gx_events_on "$RUN/c3" > /dev/null
rc=$?
check "c3: the kit's kprobes count as on" '[ $rc = 0 ]'

echo "== d: zstd fails (a stand-in that exits 11, as zstd 1.5.7 does on a size-0 file)"
mkdir -p "$RUN/stub"
printf '#!/bin/sh\ncat > /dev/null\necho "zstd: error 11 : Src size is incorrect" >&2\nexit 11\n' > "$RUN/stub/zstd"
chmod +x "$RUN/stub/zstd"
CAP_PATH="$RUN/stub:$PATH" capture d "$EVENTS" "$SAMPLE"
check "d: exit 1" '[ $RC = 1 ]'
check "d: the packing stage is named, right above the last line" '[ "$MLINE" = "Mitschrift LEER: Packen fehlgeschlagen (zstd rc=11)" ]'
check "d: no trace file is left, not even an empty one" '[ -z "$(ls "$OUT" | grep trace-load)" ]'
check "d: last line FEHLER" 'tail -1 "$LOG" | grep -q "^FEHLER: Mitschrift leer"'

echo "== f: the full buffer dropped events (the end of the init is missing)"
capture f "$EVENTS" "$SAMPLE" 3
check "f: exit 1" '[ $RC = 1 ]'
check "f: UNVOLLSTÄNDIG names the loss and keeps the counts, right above the last line" \
	'[ "$MLINE" = "Mitschrift UNVOLLSTÄNDIG: 3 Ereignisse verloren (Puffer voll, das Ende fehlt); aufgenommen 24 Zeilen (11 Schreib-, 13 Lesezugriffe), verloren 3" ] && [ "$MLINE" = "$(head -1 "$OUT/trace-status.txt")" ]'
check "f: last line FEHLER unvollständig, no FERTIG" \
	'tail -1 "$LOG" | grep -q "^FEHLER: Mitschrift unvollständig" && ! grep -q "^FERTIG" "$LOG"'
check "f: the trace is kept" 'zstd -qdc "$OUT/trace-load.txt.zst" | cmp -s - "$SAMPLE"'

echo "== g: the bind still runs after 180 s, or fails"
GX_DRY_BIND=timeout capture g "$EVENTS" "$SAMPLE"
check "g: exit 1, UNVOLLSTÄNDIG names the 180 s, FEHLER" \
	'[ $RC = 1 ] && [[ $MLINE == "Mitschrift UNVOLLSTÄNDIG: der Start lief nach 180 s noch; aufgenommen 24 Zeilen"* ]] && tail -1 "$LOG" | grep -q "^FEHLER: Mitschrift unvollständig"'
GX_DRY_BIND=5 capture g2 "$EVENTS" "$SAMPLE" 2
check "g2: a failed bind and lost events are both named" \
	'[ $RC = 1 ] && [[ $MLINE == "Mitschrift UNVOLLSTÄNDIG: der Start schlug fehl (bind rc=5, bind.err), 2 Ereignisse verloren"* ]]'

echo "== h: a retry in the same boot (and second) gets its own folder"
capture h "$EVENTS" "$SAMPLE"
first=$OUT
GX_DRYRUN=1 GX_TRACEFS="$RUN/h/tracefs" GX_OUT="$RUN/h/out" GX_DRY_LOAD="$SAMPLE" bash "$CAP" > "$RUN/h/log2.txt" 2>&1 < /dev/null
check "h: two folders and two tars, the first one's setup log written once" \
	'[ "$(ls -d "$RUN"/h/out/gp-gpu-messung-*/ | wc -l)" = 2 ] && [ "$(ls "$RUN"/h/out/gp-gpu-messung-*.tar | wc -l)" = 2 ] && [ "$(grep -c "^dry run" "$first/trace-setup.txt")" = 1 ]'

echo "== d2: reading the trace fails"
fake "$RUN/d2" "$EVENTS"
rm "$RUN/d2/trace" && mkdir "$RUN/d2/trace"
gx_trace_dump "$RUN/d2" "$RUN/d2.zst" > "$RUN/d2.txt" 2>&1
rc=$?
check "d2: refused, the reading stage named, no file" \
	'[ $rc = 1 ] && grep -q "^Mitschrift LEER: Lesen der Mitschrift fehlgeschlagen (cat .* rc=1)" "$RUN/d2.txt" && [ ! -e "$RUN/d2.zst" ] && [ ! -e "$RUN/d2.zst.part" ]'

echo "== e: a source whose stat() says size 0 (tracefs, procfs, sysfs), here /proc/version"
check "e: /proc/version has stat size 0" '[ "$(stat -c %s /proc/version)" = 0 ]'
rm -f "$RUN/old.zst"
zstd -q -o "$RUN/old.zst" /proc/version 2> /dev/null
old=$?
if [ $old != 0 ] && [ ! -s "$RUN/old.zst" ]; then
	ok "e: the 16.11. form (zstd -q -o out <file>) fails (rc=$old) and leaves an empty file: the cause of the empty trace"
else
	echo "note e (evidence only, see the header): this zstd ($(zstd -V)) packs a size-0 file by name (rc=$old); the checked form does not depend on it"
fi
fake "$RUN/e" "$EVENTS"
rm "$RUN/e/trace" && ln -s /proc/version "$RUN/e/trace"
gx_trace_dump "$RUN/e" "$RUN/e.zst" > "$RUN/e.txt" 2>&1
check "e: the checked form packs it (every stage passes) and the text survives" \
	'zstd -qdc "$RUN/e.zst" | cmp -s - /proc/version'
check "e: a text without register lines is still LEER" 'grep -q "^Mitschrift LEER: keine Registerzugriffe (0 Schreib-, 0 Lesezugriffe in 1 Zeilen)" "$RUN/e.txt"'

echo "== the decoder names a trace (host/gaming-pc/gx-trace-decode.py)"
DEC="$ROOT/host/gaming-pc/gx-trace-decode.py"
mkdir -p "$RUN/n"
{
	echo "# tracer: nop"
	echo "            bash-1       [000] .....     1.000001: amdgpu_device_rreg: 0x7550, 0x00016061, 0x80000000"
	echo "            bash-1       [000] d..1.     1.000002: amdgpu_device_wreg: 0x7550, 0x00000006, 0x00000007"
	echo "            bash-1       [000] d..1.     1.000002: amdgpu_device_wreg: 0x7550, 0x00000000, 0xfaff0000"
	echo "            bash-1       [000] d..1.     1.000003: amdgpu_device_rreg: 0x7550, 0x00000001, 0x28211407"
	echo "            bash-1       [000] d..1.     1.000004: amdgpu_device_wreg: 0x7550, 0x00000000, 0xfaff0004"
	echo "            bash-1       [000] d..1.     1.000005: amdgpu_device_rreg: 0x7550, 0x00000001, 0x00030001"
	echo "            bash-1       [000] .....     1.000006: amdgpu_device_wreg: 0x7550, 0x00001120, 0xc0310120"
	echo "            bash-1       [000] .....     1.000006: amdgpu_dc_wreg: reg=0x00001120, value=0xc0310120"
	for t in 7 8 9; do echo "            bash-1       [000] .....     1.00000$t: amdgpu_device_rreg: 0x7550, 0x00016063, 0x00000000"; done
	echo "            kwin-2       [001] .....     1.000010: amdgpu_device_rreg: 0x7550, 0x00016091, 0x0e5b5a34"
} > "$RUN/n/trace.txt"
{
	echo "1.000001 bash R MP:MPASP_SMN_C2PMSG_33(0x16061) 0x80000000"
	echo "1.000003 bash VRAM-READ 0x3faff0000..+2 dwords"
	echo "1.000006 bash W OSS:IH_RB_CNTL(0x1120) 0xc0310120"
	echo "1.000007 bash R MP:MPASP_SMN_C2PMSG_35(0x16063) 0x00000000"
	echo "      ... x2 more"
} > "$RUN/n/want-init.txt"
{ cat "$RUN/n/want-init.txt"; echo "1.000010 kwin R MP:MPASP_SMN_C2PMSG_81(0x16091) 0x0e5b5a34"; } > "$RUN/n/want-all.txt"
check "n: init names the binding task's accesses, folds the VRAM read and the poll" \
	'python3 "$DEC" init "$RUN/n/trace.txt" "$RUN/n/init.txt" && cmp -s "$RUN/n/init.txt" "$RUN/n/want-init.txt"'
check "n: all keeps the other tasks" 'python3 "$DEC" all "$RUN/n/trace.txt" "$RUN/n/all.txt" && cmp -s "$RUN/n/all.txt" "$RUN/n/want-all.txt"'
zstd -q -f "$RUN/n/trace.txt" -o "$RUN/n/trace.txt.zst"
check "n: a packed trace decodes the same" 'python3 "$DEC" init "$RUN/n/trace.txt.zst" "$RUN/n/z.txt" && cmp -s "$RUN/n/z.txt" "$RUN/n/want-init.txt"'

echo "== the generator cuts an init into sections (host/gaming-pc/gx-trace-seq.py)"
SEQ="$ROOT/host/gaming-pc/gx-trace-seq.py"
mkdir -p "$RUN/s"
# One init with every section's anchor, the forms the generator folds, and an event in between.
cat > "$RUN/s/init.txt" << 'INIT'
1.000001 bash R MP:MPASP_SMN_C2PMSG_33(0x16061) 0x80000000
1.000002 bash VRAM-READ 0x3faff0000..+2 dwords
1.000003 bash R NBIF:RCC_DEV0_EPF2_STRAP2(0x404d102) 0x03132000
1.000004 bash W NBIF:BIF_BX0_REMAP_HDP_MEM_FLUSH_CNTL(0xe4d) 0x0007f000
1.000005 bash W NBIF:RCC_DEV0_EPF0_RCC_DOORBELL_APER_EN|+24(0xde0) 0x00000001
1.000006 bash W MMHUB:MMVM_L2_CNTL3(0x1a4e6) 0x80130009
1.000007 bash W ?(0x1fc00) 0x00000000
1.000007 bash R NBIF:RCC_DEV0_EPF0_RCC_CONFIG_MEMSIZE|+24(0xde3) 0x00003fb0
1.000008 bash R MMHUB:MMVM_INVALIDATE_ENG17_SEM(0x1a586) 0x00000000
      ... x2 more
1.000011 bash R MMHUB:MMVM_INVALIDATE_ENG17_SEM(0x1a586) 0x00000001
1.000012 bash W MMHUB:MMVM_INVALIDATE_ENG17_REQ(0x1a598) 0x00f80001
1.000013 bash R MMHUB:MMVM_INVALIDATE_ENG17_ACK(0x1a5aa) 0x00010001
1.000014 bash W MMHUB:MMVM_L2_BANK_SELECT_RESERVED_CID2(0x1a500) 0x12105c17
1.000015 bash R MMHUB:MMVM_L2_BANK_SELECT_RESERVED_CID2(0x1a500) 0x10105c17
1.000016 bash EV amdgpu_bo_create bo=1, pages=1
1.000017 bash R ?(0x16e37) 0x00000007
1.000018 bash W OSS:IH_RB_CNTL(0x1120) 0xc0310120
1.000019 bash R MP:MPASP_SMN_C2PMSG_81(0x16091) 0x00000000
1.000020 bash R MP:MPASP_SMN_C2PMSG_35(0x16063) 0x80000000
      ... x1 more
1.000021 bash W MP:MPASP_SMN_C2PMSG_35(0x16063) 0x00020000
1.020030 bash R MP:MPASP_SMN_C2PMSG_81(0x16091) 0x00000000
      ... x3 more
1.020050 bash R MP:MPASP_SMN_C2PMSG_81(0x16091) 0x0e5b5a34
1.020060 bash R MP:MPASP_SMN_C2PMSG_64(0x16080) 0x80c00000
1.020061 bash W MP:MPASP_SMN_C2PMSG_67(0x16083) 0x00000010
1.020062 bash W MP:MP1_SMN_C2PMSG_66(0x16282) 0x00000003
1.020063 bash R MP:MP1_SMN_C2PMSG_90(0x1629a) 0x00000000
      ... x4 more
1.020070 bash R MP:MP1_SMN_C2PMSG_90(0x1629a) 0x00000001
1.020071 bash W DCN:DP_AUX0_AUX_SW_DATA(0x4b78) 0x00000300
      ... x1 more
1.020072 bash R GC:CP_STAT(0xbdad) 0x00000000
1.020073 bash W GC:GCVM_CONTEXT0_PAGE_TABLE_BASE_ADDR_LO32(0x2a4b) 0xdab00001
1.020073 bash W GC:GCVM_INVALIDATE_ENG17_REQ(0x28b8) 0x00f80001
1.020074 bash R GC:GCVM_INVALIDATE_ENG17_ACK(0x2a26) 0x00000001
1.020075 bash W GC:GRBM_GFX_INDEX(0xa900) 0xe0000000
1.020075 bash R GC:CP_STAT(0xbdad) 0x88028000
      ... x2 more
1.020075 bash R GC:CP_STAT(0xbdad) 0x00000000
1.020076 bash W GC:GRBM_GFX_CNTL(0xa900) 0x0000000c
1.020077 bash R GC:CP_MES_CNTL(0x2807) 0x40000000
1.020078 bash W GC:CP_MAX_CONTEXT(0x3aee) 0x00000007
1.020079 bash R GC:SDMA0_MCU_CNTL(0x1260) 0x00000000
1.020080 bash W NBIF:GDC_S2A0_S2A_DOORBELL_ENTRY_4_CTRL(0xeef) 0x40118809
1.020080 bash R MP:MP1_SMN_C2PMSG_90(0x1629a) 0x00000001
      ... x2 more
1.020081 bash W NBIF:GDC_S2A0_S2A_DOORBELL_ENTRY_4_CTRL(0xeef) 0x40118809
1.020082 bash W GC:RLC_SAFE_MODE(0xa980) 0x00000001
INIT
python3 "$SEQ" "$RUN/s/init.txt" "$RUN/s/out" > "$RUN/s/said.txt" 2>&1
check "s: the generator runs and names seventeen sections in Linux's order" \
	'[ $? = 0 ] && [ "$(awk "{print \$1}" "$RUN/s/said.txt" | tr "\n" " ")" = "discovery nbif mmhub sw-init ih psp-boot psp-ring smu display gc-boot gc-hub gc-cp mes gc-ring sdma vcn late " ] && [ "$(ls "$RUN/s/out" | wc -l)" = 17 ]'
SHA=$(sha256sum "$RUN/s/init.txt" | cut -d' ' -f1)
# The header names a trace inside the repository by its path there (this run's folder may be one), else by its name.
LABEL=$(realpath "$RUN/s/init.txt")
case "$LABEL" in "$(cd "$ROOT" && pwd -P)"/*) LABEL=${LABEL#"$(cd "$ROOT" && pwd -P)"/} ;; *) LABEL=init.txt ;; esac
HEAD_END="by host/gaming-pc/gx-trace-seq.py; regenerate: python3 host/gaming-pc/gx-trace-seq.py $LABEL crates/drivers/amdgpu/src/rdna4/linuxseq"
{
	echo "# linuxseq mmhub: $LABEL sha256 $SHA, init lines 6-15, $HEAD_END"
	echo "# wait MMVM_INVALIDATE_ENG17_SEM: until (value & 0x00000001) == 0x00000001; after 100000 reads 1 us apart Linux logs it and goes on; gmc_v12_0.c:238-250"
	echo "# wait MMVM_INVALIDATE_ENG17_ACK: until (value & 0x00000001) == 0x00000001; after 100000 reads 1 us apart Linux logs it and goes on; gmc_v12_0.c:255-263 and :289-292"
	echo "6 W MMVM_L2_CNTL3 0x1a4e6 0x80130009"
	echo "7 HDP 0x1fc00 RCC_DEV0_EPF0_RCC_CONFIG_MEMSIZE 0xde3 0x00003fb0"
	echo "9 WAIT MMVM_INVALIDATE_ENG17_SEM 0x1a586 mask=0x00000001 is=0x00000001 max=100000 last=0x00000001 reads=4 us=3 seen=0x00000000x3,0x00000001x1"
	echo "12 W MMVM_INVALIDATE_ENG17_REQ 0x1a598 0x00f80001"
	echo "13 WAIT MMVM_INVALIDATE_ENG17_ACK 0x1a5aa mask=0x00000001 is=0x00000001 max=100000 last=0x00010001 reads=1 us=0 seen=0x00010001x1"
	echo "14 W MMVM_L2_BANK_SELECT_RESERVED_CID2 0x1a500 0x12105c17"
	echo "15 R MMVM_L2_BANK_SELECT_RESERVED_CID2 0x1a500 0x10105c17"
} > "$RUN/s/want-mmhub.txt"
check "s: mmhub: the header, the flush pair as one line, a poll folded into a wait with its bound, its answers and its time" \
	'cmp -s "$RUN/s/out/mmhub.txt" "$RUN/s/want-mmhub.txt"'
{
	echo "# linuxseq psp-boot: $LABEL sha256 $SHA, init lines 19-25, $HEAD_END"
	echo "# wait MPASP_SMN_C2PMSG_35: until (value & 0x80000000) == 0x80000000; after 1000000 reads 1 us apart Linux fails; psp_v14_0.c:113-121"
	echo "# wait MPASP_SMN_C2PMSG_81: until value != the read before the wait; after 100000 reads 1 us apart Linux fails; psp_v14_0.c:230-235"
	echo "# sleep after a write of MPASP_SMN_C2PMSG_35: 20000 us, mdelay(20) after PSP_BL__LOAD_SOSDRV, psp_v14_0.c:229-230"
	echo "19 R MPASP_SMN_C2PMSG_81 0x16091 0x00000000"
	echo "20 WAIT MPASP_SMN_C2PMSG_35 0x16063 mask=0x80000000 is=0x80000000 max=1000000 last=0x80000000 reads=1 us=0 seen=0x80000000x1"
	echo "20 WAIT MPASP_SMN_C2PMSG_35 0x16063 mask=0x80000000 is=0x80000000 max=1000000 last=0x80000000 reads=1 us=0 seen=0x80000000x1"
	echo "22 W MPASP_SMN_C2PMSG_35 0x16063 0x00020000"
	echo "22 SLEEP us=20000 gap=20009"
	echo "23 R MPASP_SMN_C2PMSG_81 0x16091 0x00000000"
	echo "23 WAIT MPASP_SMN_C2PMSG_81 0x16091 changed from=0x00000000 max=100000 last=0x0e5b5a34 reads=4 us=20 seen=0x00000000x3,0x0e5b5a34x1"
} > "$RUN/s/want-boot.txt"
check "s: psp-boot: a read that meets the condition ends its wait, the secure OS's sleep and its wait for a change after a plain read" \
	'cmp -s "$RUN/s/out/psp-boot.txt" "$RUN/s/want-boot.txt"'
{
	echo "# linuxseq smu: $LABEL sha256 $SHA, init lines 28-31, $HEAD_END"
	echo "# wait MP1_SMN_C2PMSG_90: until value != 0; after 2000000 reads 1 us apart Linux sends nothing more (before a message) or fails the message (after it); smu_cmn.c:294-309, :514-522 and :532-533"
	echo "28 W MP1_SMN_C2PMSG_66 0x16282 0x00000003"
	echo "29 WAIT MP1_SMN_C2PMSG_90 0x1629a changed from=0x00000000 max=2000000 last=0x00000001 reads=6 us=7 seen=0x00000000x5,0x00000001x1"
} > "$RUN/s/want-smu.txt"
check "s: smu: the answer register's poll is a wait for a value that is not 0, with no plain read before it and Linux's 2000000 reads as its bound" \
	'cmp -s "$RUN/s/out/smu.txt" "$RUN/s/want-smu.txt"'
check "s: that wait rule holds in smu only: the same register's run in vcn stays plain reads; a repeated write stays writes, an unnamed dword keeps its number" \
	'grep -qx "47 R MP1_SMN_C2PMSG_90 0x1629a 0x00000001 x3" "$RUN/s/out/vcn.txt" && ! grep -q "^# wait" "$RUN/s/out/vcn.txt" && [ "$(grep -c "^32 W DP_AUX0_AUX_SW_DATA 0x4b78 0x00000300$" "$RUN/s/out/display.txt")" = 2 ] && grep -qx "17 R UNNAMED_16e37 0x16e37 0x00000007" "$RUN/s/out/sw-init.txt"'
check "s: discovery keeps the VRAM read; mes takes the pipe select before it" \
	'grep -qx "2 MEM 0x3faff0000 2" "$RUN/s/out/discovery.txt" && [ "$(sed -n 2p "$RUN/s/out/mes.txt")" = "42 W GRBM_GFX_CNTL 0xa900 0x0000000c" ]'
check "s: gc-cp: CP_STAT's run after the unhalt is one wait with Linux's bound; gc-boot's CP_STAT stays a plain read (half of the autoload's pair poll)" \
	'grep -qx "39 WAIT CP_STAT 0xbdad mask=0xffffffff is=0x00000000 max=100000 last=0x00000000 reads=4 us=0 seen=0x88028000x3,0x00000000x1" "$RUN/s/out/gc-cp.txt" && grep -qx "38 W GRBM_GFX_INDEX 0xa900 0xe0000000" "$RUN/s/out/gc-cp.txt" && [ "$(grep -vc "^#" "$RUN/s/out/gc-cp.txt")" = 2 ] && grep -q "^# wait CP_STAT: until (value & 0xffffffff) == 0x00000000; after 100000 reads 1 us apart Linux logs it and goes on; gfx_v12_0.c:2371-2382$" "$RUN/s/out/gc-cp.txt" && grep -qx "34 R CP_STAT 0xbdad 0x00000000" "$RUN/s/out/gc-boot.txt" && ! grep -q "^# wait" "$RUN/s/out/gc-boot.txt"'
zstd -q -f "$RUN/s/init.txt" -o "$RUN/s/init.txt.zst"
check "s: a packed init gives the same lines (the header names the file it was made from)" \
	'python3 "$SEQ" "$RUN/s/init.txt.zst" "$RUN/s/z" > /dev/null && [ "$(tail -n +2 "$RUN/s/z/mmhub.txt")" = "$(tail -n +2 "$RUN/s/out/mmhub.txt")" ] && head -1 "$RUN/s/z/mmhub.txt" | grep -q "^# linuxseq mmhub: $LABEL.zst sha256 $(sha256sum "$RUN/s/init.txt.zst" | cut -d" " -f1), init lines 6-15, "'

# refuse <name> <sed script> <the one line>: a changed init stops the generator with exactly that line, no file.
refuse() {
	local name=$1 want=$3 rc
	sed "$2" "$RUN/s/init.txt" > "$RUN/s/$name.txt"
	python3 "$SEQ" "$RUN/s/$name.txt" "$RUN/s/$name-out" > "$RUN/s/$name-said.txt" 2>&1
	rc=$?
	check "s: $name: stops with one plain line and writes nothing" \
		'[ $rc != 0 ] && [ "$(cat "$RUN/s/$name-said.txt")" = "$want" ] && [ ! -e "$RUN/s/$name-out" ]'
	[ "$(cat "$RUN/s/$name-said.txt")" = "$want" ] || sed 's/^/     | /' "$RUN/s/$name-said.txt"
}
refuse no-anchor '/IH_RB_CNTL/d' "gx-trace-seq: section ih: no access of the OSS block (ih_v7_0.c:310) after init line 17"
refuse empty-section '/0x16e37/d' "gx-trace-seq: section sw-init is empty: ih begins at init line 17 with the access of the OSS block (ih_v7_0.c:310), where sw-init would begin"
refuse last-empty '/RLC_SAFE_MODE/d' "gx-trace-seq: section late is empty: nothing follows the second write of GDC_S2A0_S2A_DOORBELL_ENTRY_4_CTRL"
refuse unknown-line 's/^1.000016 bash EV .*/something else/' "gx-trace-seq: line 16 is no access, event or repeat line: something else"
refuse flush-alone '/CONFIG_MEMSIZE/d' "gx-trace-seq: init line 7: a write of the HDP flush register (dword 0x1fc00) without the memory size read after it"
refuse poll-without-rule 's/^1.000018 bash W OSS:IH_RB_CNTL(0x1120) 0xc0310120$/1.000018 bash R OSS:IH_RB_CNTL(0x1120) 0x40610000\n      ... x2 more/' \
	"gx-trace-seq: init line 18: 3 reads of IH_RB_CNTL in a row and no wait rule for it (section ih)"
refuse wait-never-met 's/^1.000013 bash R MMHUB:MMVM_INVALIDATE_ENG17_ACK(0x1a5aa) 0x00010001$/1.000013 bash R MMHUB:MMVM_INVALIDATE_ENG17_ACK(0x1a5aa) 0x00010000/' \
	"gx-trace-seq: init line 13: the reads of MMVM_INVALIDATE_ENG17_ACK never meet Linux's condition (gmc_v12_0.c:255-263 and :289-292)"
refuse ack-without-request '/GCVM_INVALIDATE_ENG17_REQ/d' "gx-trace-seq: init line 36: a read of GCVM_INVALIDATE_ENG17_ACK that does not follow a request for one VMID (gmc_v12_0.c:255-263 and :289-292)"
# Time: Linux's sleep must show as silence, any other silent millisecond of a consumed section must be named, and a
# wait cannot hold more reads than Linux makes before it gives up.
refuse sleep-too-short 's/^1.020030 bash R MP:MPASP_SMN_C2PMSG_81/1.019000 bash R MP:MPASP_SMN_C2PMSG_81/' \
	"gx-trace-seq: init line 22: Linux sleeps 20000 us after this write of MPASP_SMN_C2PMSG_35 (mdelay(20) after PSP_BL__LOAD_SOSDRV, psp_v14_0.c:229-230); the trace shows 18979 us"
refuse silence-unnamed 's/^1.000018 bash W OSS:IH_RB_CNTL(0x1120) 0xc0310120$/1.000018 bash W OSS:IH_RB_CNTL(0x1120) 0xc0310120\n1.005018 bash R OSS:IH_RB_CNTL(0x1120) 0xc0310120/' \
	"gx-trace-seq: init line 18: 5000 us without a register access after IH_RB_CNTL, and neither SLEEPS nor QUIET says why (section ih)"
refuse wait-past-the-bound 's/^      \.\.\. x3 more$/      ... x100000 more/' \
	"gx-trace-seq: init line 23: 100001 reads of MPASP_SMN_C2PMSG_81 in one wait, more than the 100000 after which Linux gives up (psp_v14_0.c:230-235)"
refuse smu-wait-past-the-bound 's/^      \.\.\. x4 more$/      ... x1999999 more/' \
	"gx-trace-seq: init line 29: 2000001 reads of MP1_SMN_C2PMSG_90 in one wait, more than the 2000000 after which Linux gives up (smu_cmn.c:294-309, :514-522 and :532-533)"
refuse smu-answer-never-comes 's/^1.020070 bash R MP:MP1_SMN_C2PMSG_90(0x1629a) 0x00000001$/1.020070 bash R MP:MP1_SMN_C2PMSG_90(0x1629a) 0x00000000/' \
	"gx-trace-seq: init line 29: the reads of MP1_SMN_C2PMSG_90 never meet Linux's condition (smu_cmn.c:294-309, :514-522 and :532-533)"
# A silence after the answer's reads is named only behind TransferTableSmu2Dram (Linux copies the table out of VRAM).
refuse smu-silence-unnamed 's/^1.020071 bash W DCN/1.022071 bash W DCN/' \
	"gx-trace-seq: init line 31: 2001 us without a register access after MP1_SMN_C2PMSG_90, and neither SLEEPS nor QUIET says why (section smu)"
sed 's/^1.020071 bash W DCN/1.022071 bash W DCN/; s/(0x16282) 0x00000003$/(0x16282) 0x00000012/' "$RUN/s/init.txt" > "$RUN/s/copy.txt"
check "s: the silence behind TransferTableSmu2Dram's answer is the table's copy out of VRAM: no sleep and no stop" \
	'python3 "$SEQ" "$RUN/s/copy.txt" "$RUN/s/copy-out" > /dev/null 2>&1 && [ "$(sed -n 4p "$RUN/s/copy-out/smu.txt")" = "$(sed -n 4p "$RUN/s/want-smu.txt")" ] && grep -qx "28 W MP1_SMN_C2PMSG_66 0x16282 0x00000012" "$RUN/s/copy-out/smu.txt" && ! grep -q SLEEP "$RUN/s/copy-out/smu.txt"'
sed 's/^1.020061 bash W MP:MPASP_SMN_C2PMSG_67/1.010061 bash W MP:MPASP_SMN_C2PMSG_67/' "$RUN/s/init.txt" > "$RUN/s/quiet.txt"
check "s: a silence QUIET names (the fence polled in memory after a ring command) is no sleep and no stop" \
	'python3 "$SEQ" "$RUN/s/quiet.txt" "$RUN/s/quiet-out" > /dev/null 2>&1 && [ "$(tail -n +2 "$RUN/s/quiet-out/psp-ring.txt")" = "$(tail -n +2 "$RUN/s/out/psp-ring.txt")" ] && ! grep -q SLEEP "$RUN/s/quiet-out/psp-ring.txt"'
# Lines that stand for accesses the generator cannot name.
refuse repeat-after-vram 's/^1.000002 bash VRAM-READ .*/&\n      ... x1 more/' "gx-trace-seq: line 3: a repeated VRAM read is not a form this script knows"
refuse repeat-after-event 's/^1.000016 bash EV .*/&\n      ... x2 more/' "gx-trace-seq: line 17: a repeat line with no access before it"
refuse repeat-first '1s/^/      ... x1 more\n/' "gx-trace-seq: line 1: a repeat line with no access before it"
grep ' EV ' "$RUN/s/init.txt" > "$RUN/s/events-only.txt"
python3 "$SEQ" "$RUN/s/events-only.txt" "$RUN/s/events-only-out" > "$RUN/s/events-only-said.txt" 2>&1
check "s: an init of events alone: stops with one plain line and writes nothing" \
	'[ $? != 0 ] && [ "$(cat "$RUN/s/events-only-said.txt")" = "gx-trace-seq: $RUN/s/events-only.txt holds no register access" ] && [ ! -e "$RUN/s/events-only-out" ]'
head -c 300 "$RUN/s/init.txt.zst" > "$RUN/s/broken.txt.zst"
python3 "$SEQ" "$RUN/s/broken.txt.zst" "$RUN/s/broken-out" > "$RUN/s/broken-said.txt" 2> "$RUN/s/broken-err.txt"
check "s: a packed init that zstd cannot read: stops with one plain line and writes nothing" \
	'[ $? != 0 ] && [ "$(tail -1 "$RUN/s/broken-err.txt")" = "gx-trace-seq: zstd could not read $RUN/s/broken.txt.zst" ] && [ ! -s "$RUN/s/broken-said.txt" ] && [ ! -e "$RUN/s/broken-out" ]'

echo "== m: the measurement run's evaluator (gx-messung-eval.py) on protocols the program wrote on the fake card"
# The fixtures are what the program writes now (amdgpu host test the_evaluators_fixtures_are_what_the_program_writes):
# the whole run, and one with a difference injected into gchub. The evaluator takes the fake's protocol as its own
# reference by default; here each protocol is also read against itself.
EVAL=$ROOT/host/gaming-pc/gx-messung-eval.py
GOOD=$ROOT/host/tests/gx/fixtures/gpu-messung-fake.txt
DIFF=$ROOT/host/tests/gx/fixtures/gpu-messung-differs.txt
mkdir -p "$RUN/m"
python3 "$EVAL" "$GOOD" > "$RUN/m/good.txt" 2>&1
check "m: the whole run on the fake card: exit 0, every stage equal for the card and for the evaluator, no disagreement" \
	'[ $? = 0 ] && [ "$(grep -c "^[a-z]* *PASS  card: \(equal\|so-far\) (answers 0" "$RUN/m/good.txt")" = 21 ] && [ "$(grep -c "evaluator: equal" "$RUN/m/good.txt")" = 21 ] && ! grep -q "DISAGREE\|NO CHECK\|no row of section 7\|refuted " "$RUN/m/good.txt"'
check "m: every row of section 7 has its key's verdict" \
	'[ "$(grep -c "^[a-z0-9-]* *\(confirmed\|confirmed in part\|refuted\|not measurable\) " "$RUN/m/good.txt")" = "$(sed -n "/^## 7\./,\$p" "$ROOT/plan/spec/gpu-rdna4.md" | grep -c "^| \`[a-z0-9-]*\` |")" ] && grep -q "^== 79 confirmed, 6 confirmed in part, 0 refuted, 39 not measurable" "$RUN/m/good.txt" && ! grep -q "EVALUATOR ERROR" "$RUN/m/good.txt"'
check "m: a row the run measures only in part says which part is not measured" \
	'grep -q "^atomics *confirmed in part .*not measured in this run: the \`pci\` tool.s Device Control 2 afterwards" "$RUN/m/good.txt" && grep -q "^pcie-override *confirmed in part " "$RUN/m/good.txt" && grep -q "^link-limits *confirmed in part " "$RUN/m/good.txt" && grep -q "^cmd-msi *confirmed in part " "$RUN/m/good.txt"'
check "m: every stage's stack figure is judged against the head's budget" \
	'[ "$(grep -c "stack [0-9]* of [0-9]* bytes: within the stage budget" "$RUN/m/good.txt")" = 21 ]'
check "m: the answers of the whole run are the fake card's own" \
	'[ "$(grep -c "fake card: the same answers as the fake" "$RUN/m/good.txt")" = 21 ]'
# Wave 44: MES's two stages share the section mes (the first equal so far, the second ends it, each with MES's bind
# by registers), the engines' bind is the flush through MES, the rungs' two flushes too, and MES's rows are judged.
check "m: MES: mes so far and mesq equal for both, the bind flushes through MES after it, MES's rows confirmed" \
	'grep -q "^mes *PASS  card: so-far (answers 0.* evaluator: equal so far (the bind.s TLB flush before GRBM_GFX_CNTL is Linux.s (12 lines))" "$RUN/m/good.txt" && grep -q "^mesq *PASS  card: equal (answers 0.* evaluator: equal " "$RUN/m/good.txt" && grep -q "^sdma *PASS .*evaluator: equal (the bind.s TLB flush before SDMA0_QUEUE0_RB_WPTR_POLL_ADDR_LO is Linux.s (3 lines))" "$RUN/m/good.txt" && grep -q "^sdmatest *PASS .*evaluator: equal: 2 flushes through MES" "$RUN/m/good.txt" && grep -q "^flush-mes *confirmed " "$RUN/m/good.txt" && grep -q "^mes-queues *confirmed *5 of 5 kernel queues" "$RUN/m/good.txt" && grep -q "^mes-answers *confirmed " "$RUN/m/good.txt"'
# Wave 44, the rings: gcring equal to gc-ring with no rule, ringtest held against Linux's forms, the rings' rows judged.
check "m: rings: gcring equal, ringtest in Linux's forms, the rings' rows confirmed" \
	'grep -q "^gcring *PASS  card: equal (answers 0.* evaluator: equal$" "$RUN/m/good.txt" && grep -q "^ringtest *PASS  card: equal (answers 0.* evaluator: equal: Linux.s forms of the rungs, line for line" "$RUN/m/good.txt" && grep -q "^ring-tests *confirmed *five ring tests in Linux.s order" "$RUN/m/good.txt" && grep -q "^ring-enables *confirmed " "$RUN/m/good.txt" && grep -q "^ring-ib-tests *confirmed " "$RUN/m/good.txt" && grep -q "^ring-eop *confirmed " "$RUN/m/good.txt" && grep -q "^ring-remap *confirmed " "$RUN/m/good.txt" && grep -q "^mes-unmap *confirmed " "$RUN/m/good.txt" && grep -q "^mqd-defaults *confirmed " "$RUN/m/good.txt"'
# The evaluator's own comparison of the rungs: the gfx pipe's end-of-pipe enable written with one bit only.
sed '0,/^\(SPUR ringtest W CP_INT_CNTL_RING0 0x[0-9a-f]*\) 0x85c00000$/s//\1 0x05c00000/' "$GOOD" > "$RUN/m/ring-edited.txt"
python3 "$EVAL" "$RUN/m/ring-edited.txt" > "$RUN/m/ring-edited-said.txt" 2>&1
check "m: the rungs' trail edited behind the card's line: named by the evaluator" \
	'[ $? = 1 ] && ! cmp -s "$GOOD" "$RUN/m/ring-edited.txt" && grep -q "^ringtest .*evaluator: differs: init.txt line 9818: Linux writes 0x85c00000 to CP_INT_CNTL_RING0; the stage writes 0x05c00000.*DISAGREE" "$RUN/m/ring-edited-said.txt"'
# The evaluator's own comparison in MES's section: the scheduler pipe's start written with another value.
sed '0,/^SPUR mes W CP_MES_CNTL \(0x[0-9a-f]*\) 0x04000000$/s//SPUR mes W CP_MES_CNTL \1 0x04000001/' "$GOOD" > "$RUN/m/mes-edited.txt"
python3 "$EVAL" "$RUN/m/mes-edited.txt" > "$RUN/m/mes-edited-said.txt" 2>&1
check "m: MES's trail edited behind the card's line: named by the evaluator, and mesq after it too" \
	'[ $? = 1 ] && ! cmp -s "$GOOD" "$RUN/m/mes-edited.txt" && grep -q "^mes .*evaluator: differs: init.txt line 9507: Linux writes 0x04000000 to CP_MES_CNTL; the stage writes 0x04000001.*DISAGREE" "$RUN/m/mes-edited-said.txt" && grep -q "^mesq .*evaluator: differs: init.txt line 9507" "$RUN/m/mes-edited-said.txt"'
python3 "$EVAL" "$DIFF" > "$RUN/m/differs.txt" 2>&1
check "m: a difference injected into gchub: exit 1, gchub differs for both, the run stopped there, the card's answer named against the fake's" \
	'[ $? = 1 ] && grep -q "^gchub *PASS  card: differs .* evaluator: differs: init.txt line 9214: Linux writes 0x00000003 to GCVM_L2_CNTL2; the stage writes 0x00000013" "$RUN/m/differs.txt" && grep -q "^stopped at gchub" "$RUN/m/differs.txt" && grep -q "^accesses *refuted *gchub" "$RUN/m/differs.txt" && grep -q "GCVM_L2_CNTL2: card 0x00000010, fake 0x00000000" "$RUN/m/differs.txt" && ! grep -q "^gccp \|^sdma " "$RUN/m/differs.txt"'
# Wave 44: the thermal taker's rows read the THERMO lines: cool on the fake; a hot read and an alert refute.
check "m: the thermal rows on the fake card: the temperatures read twice and cool, nothing taken" \
	'grep -q "^metrics-read *confirmed *TransferTableSmu2Dram (0x12, argument 5) answered OK twice" "$RUN/m/good.txt" && grep -q "^fans *confirmed in part no thermal alert during the run" "$RUN/m/good.txt" && grep -q "^thm-events *not measurable" "$RUN/m/good.txt" && grep -q "^thm-shutdown *not measurable *Georg decides" "$RUN/m/good.txt"'
sed 's/^\(THERMO ende temperatures .*\)hotspot 38 C/\1hotspot 109 C/' "$GOOD" > "$RUN/m/hot.txt"
python3 "$EVAL" "$RUN/m/hot.txt" > "$RUN/m/hot-said.txt" 2>&1
check "m: a hotspot at its limit refutes fans and metrics-read" \
	'[ $? = 1 ] && ! cmp -s "$GOOD" "$RUN/m/hot.txt" && grep -q "^fans *refuted *a read that failed or a temperature within 10 C of its limit (ende" "$RUN/m/hot-said.txt" && grep -q "^metrics-read *refuted *a temperature outside 15..90 C\|^metrics-read *refuted *a temperature at or above its limit" "$RUN/m/hot-said.txt"'
sed '0,/^THERMO sdma alarme=+0:+0:+0 /s//THERMO sdma alarme=+0:+0:+2 /' "$GOOD" > "$RUN/m/alert.txt"
python3 "$EVAL" "$RUN/m/alert.txt" > "$RUN/m/alert-said.txt" 2>&1
check "m: an alert the taker took refutes fans and is evidence for thm-events and thm-ack" \
	'[ $? = 1 ] && ! cmp -s "$GOOD" "$RUN/m/alert.txt" && grep -q "^fans *refuted *the thermal taker took alerts" "$RUN/m/alert-said.txt" && grep -q "^thm-events *confirmed in part the taker took 2 entries" "$RUN/m/alert-said.txt" && grep -q "^thm-ack *confirmed in part 2 power controller interrupts" "$RUN/m/alert-said.txt"'
# The evaluator's own comparison, independent of the card's line: one written value changed in mmhub's trail.
sed '0,/^SPUR gmc W MMMC_VM_MX_L1_TLB_CNTL \(0x[0-9a-f]*\) 0x00001859$/s//SPUR gmc W MMMC_VM_MX_L1_TLB_CNTL \1 0x00001858/' "$GOOD" > "$RUN/m/edited.txt"
python3 "$EVAL" "$RUN/m/edited.txt" > "$RUN/m/edited-said.txt" 2>&1
check "m: a trail edited behind the card's line: the evaluator names it and the disagreement" \
	'[ $? = 1 ] && ! cmp -s "$GOOD" "$RUN/m/edited.txt" && grep -q "^gmc .*evaluator: differs: .*MMMC_VM_MX_L1_TLB_CNTL.*DISAGREE" "$RUN/m/edited-said.txt"'
# A hang: the file ends at a stage's START (written and flushed before the stage ran).
sed '/^START fw /q' "$GOOD" > "$RUN/m/hung.txt"
python3 "$EVAL" "$RUN/m/hung.txt" > "$RUN/m/hung-said.txt" 2>&1
check "m: a protocol that ends in a stage: no end line, the hung stage named" \
	'[ $? = 1 ] && grep -q "^NO END LINE: the run hung or the power went off in the stage fw " "$RUN/m/hung-said.txt"'
# A hang in the look after a stage that passed: the look is named, not the stage before it.
sed '/^BLICK state nach=gmc START$/q' "$GOOD" > "$RUN/m/hung-look.txt"
python3 "$EVAL" "$RUN/m/hung-look.txt" > "$RUN/m/hung-look-said.txt" 2>&1
check "m: a protocol that ends in a look: the look and the stage before it named, gmc whole" \
	'[ $? = 1 ] && grep -q "^NO END LINE: the run hung or the power went off in the look state after gmc " "$RUN/m/hung-look-said.txt" && grep -q "^gmc *PASS  card: equal" "$RUN/m/hung-look-said.txt"'
sed '/^BLICK alive nach=vorher START$/q' "$GOOD" > "$RUN/m/hung-pre.txt"
sed '/^## Vorpr/q' "$GOOD" | head -n -1 > "$RUN/m/hung-head.txt"
python3 "$EVAL" "$RUN/m/hung-pre.txt" > "$RUN/m/hung-pre-said.txt" 2>&1
python3 "$EVAL" "$RUN/m/hung-head.txt" > "$RUN/m/hung-head-said.txt" 2>&1
check "m: a protocol that ends in the preflight's look or after the head: said so, no stage blamed" \
	'grep -q "^NO END LINE: .* in the look alive after vorher " "$RUN/m/hung-pre-said.txt" && grep -q "^NO END LINE: .* in the preflight, before any stage started" "$RUN/m/hung-head-said.txt"'
# A stage deeper in the stack than the budget, a wait that used its whole bound, a check that breaks.
sed 's/^STAPEL gmc bytes=0$/STAPEL gmc bytes=30000/' "$GOOD" > "$RUN/m/deep.txt"
python3 "$EVAL" "$RUN/m/deep.txt" > "$RUN/m/deep-said.txt" 2>&1
check "m: a stage over the stack budget: named, exit 1" \
	'[ $? = 1 ] && grep -q "stack 30000 of 24576 bytes: OVER THE STAGE BUDGET" "$RUN/m/deep-said.txt"'
sed '0,/^\(SPUR sos WAIT MPASP_SMN_C2PMSG_35 .* max=1000000 last=0x80000000\) reads=15 /s//\1 reads=1000000 /' "$GOOD" > "$RUN/m/slow.txt"
python3 "$EVAL" "$RUN/m/slow.txt" > "$RUN/m/slow-said.txt" 2>&1
check "m: a wait that ran to Linux's bound refutes wait-time; a wait bound in time is read too" \
	'[ $? = 1 ] && ! cmp -s "$GOOD" "$RUN/m/slow.txt" && grep -q "^wait-time *refuted *1 waits ran to Linux.s bound" "$RUN/m/slow-said.txt" && python3 -I -c "import sys; sys.dont_write_bytecode = True; sys.path.insert(0, sys.argv[1]); from gx_messung_rows import ran_out; assert ran_out({\"max\": \"time:2000us/10us\", \"us\": 2000, \"reads\": 3}) and not ran_out({\"max\": \"time:2000us/10us\", \"us\": 1999, \"reads\": 3})" "$ROOT/host/gaming-pc"'
sed 's/^\(SPUR sos WAIT MPASP_SMN_C2PMSG_35 .*\) max=1000000 /\1 max=soon /' "$GOOD" > "$RUN/m/broken.txt"
python3 "$EVAL" "$RUN/m/broken.txt" > "$RUN/m/broken-said.txt" 2>&1
check "m: a check that breaks on what it reads is the script's error, never a verdict" \
	'[ $? = 1 ] && grep -q "^wait-time *EVALUATOR ERROR ValueError" "$RUN/m/broken-said.txt"'
# A CP_STAT that never settled after the unhalt: gccp stops after Linux's accesses, and its WAIT line refutes
# cp-stat-wait although the stage did not pass.
sed 's/^ANTWORT gccp gx_bringup card0 gccp: PASS$/ANTWORT gccp gx_bringup card0 gccp: FAIL/; s/^\(ANTWORT gccp WAIT .* # CP_STAT until .*\): 21 reads in 20 us, last 0x00000000$/\1: 100000 reads in 100000 us, last 0x88028000/; s/^WEITER gccp gleich wie Linux$/STOPP gccp die Stufe hat angehalten | stopped: CP_STAT still 0x88028000/' "$GOOD" > "$RUN/m/unsettled.txt"
python3 "$EVAL" "$RUN/m/unsettled.txt" > "$RUN/m/unsettled-said.txt" 2>&1
check "m: a CP_STAT that never settled stops the run at gccp and refutes cp-stat-wait" \
	'[ $? = 1 ] && grep -q "^stopped at gccp " "$RUN/m/unsettled-said.txt" && grep -q "^cp-stat-wait *refuted *Linux.s loop ran out" "$RUN/m/unsettled-said.txt" && grep -q "^cp-unhalt *not measurable" "$RUN/m/unsettled-said.txt"'
printf 'KOPF format=0\n' > "$RUN/m/other.txt"
python3 "$EVAL" "$RUN/m/other.txt" > "$RUN/m/other-said.txt" 2>&1
check "m: a file of another format is refused with one line" '[ $? = 2 ] && [ "$(wc -l < "$RUN/m/other-said.txt")" = 1 ]'

echo "== syntax"
for f in "$CAP" "$ROOT/host/gaming-pc/gx-trace-check.sh"; do check "bash -n ${f#"$ROOT"/}" 'bash -n "$f"'; done

[ $fail = 0 ] && echo "PASS: trace kit checks" || echo "FAIL: trace kit checks"
exit $fail
