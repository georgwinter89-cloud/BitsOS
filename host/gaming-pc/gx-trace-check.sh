# GT: the register trace's checks, shared by the Gaming-PC capture (host/gaming-pc/gpu-messung.sh) and the Mini-PC
# kit (gxtrace/trace.sh sources a copy of this file; plan/hw/gxtrace-kit.md). A trace must never come back empty
# without a word: the events are checked to be on before the card starts, the dump checks every pipe stage, the packed
# file and that the trace has register lines, and one plain German line says the result.
# Sourced, never run. Defines only gx_* functions; GX_STATUS holds the last one-line result.
# The dump reads tracefs through a pipe on purpose: tracefs (like procfs and sysfs) says size 0 to stat(), and
# `zstd -o out <file>` then stops with "error 11 : Src size is incorrect" and leaves an empty out file.

GX_STATUS=""
GX_LOST="" # the last dump's lost events (gx_lost)

# gx_num <file>: the first decimal number in a tracefs value ("7 (expanded: 1408)" -> 7), empty when none.
gx_num() {
	grep -oE '[0-9]+' "$1" 2>/dev/null | head -1
}

# gx_buf_kb: per-CPU ring buffer, a quarter of free memory spread over the CPUs, 16 to 128 MiB each.
gx_buf_kb() {
	local avail cpus kb
	avail=$(awk '/^MemAvailable:/ {print $2}' /proc/meminfo)
	cpus=$(nproc)
	kb=$((avail / 4 / cpus))
	[ "$kb" -gt 131072 ] && kb=131072
	[ "$kb" -lt 16384 ] && kb=16384
	echo "$kb"
}

# gx_write <file> <value>: one checked tracefs write; prints what failed.
gx_write() {
	if ! echo "$2" > "$1" 2>/dev/null; then
		echo "write refused: $2 > $1"
		return 1
	fi
}

# gx_reset <tracefs> <buffer kb>: tracing off, the old trace cleared, a full buffer drops new events (the start of the
# init matters most), the buffer sized. Every write is checked.
gx_reset() {
	local t=$1 kb=$2
	gx_write "$t/tracing_on" 0 && gx_write "$t/trace" "" && gx_write "$t/options/overwrite" 0 &&
		gx_write "$t/buffer_size_kb" "$kb" || {
		GX_STATUS="Mitschrift nicht scharf, es fehlt: die Einstellungen ließen sich nicht schreiben"
		return 1
	}
	echo "buffer $(cat "$t/buffer_size_kb") KiB per CPU, $(nproc) CPUs"
}

# gx_enable <tracefs> <system>...: switches each system's events on that exists. Which events are needed is checked
# by gx_events_on (the kit may use kprobes instead of amdgpu's tracepoints).
gx_enable() {
	local t=$1 sys
	shift
	for sys in "$@"; do
		if [ ! -d "$t/events/$sys" ]; then
			echo "events $sys: not there"
		elif gx_write "$t/events/$sys/enable" 1; then
			echo "events $sys: on"
		fi
	done
}

# gx_events_on <tracefs>: right before the card starts, after `tracing_on` was set: the register reads and writes are
# in set_event (amdgpu's tracepoints, or the kit's kprobes gx:gx_rreg/gx_wreg), the buffer has a size, tracing is on.
# On failure GX_STATUS says what is missing and the caller must not start the card.
gx_events_on() {
	local t=$1 miss=() kb
	if [ ! -r "$t/set_event" ]; then
		miss+=("die Liste der Ereignisse ($t/set_event)")
	else
		grep -qxE 'amdgpu:amdgpu_device_rreg|amdgpu:\*|gx:gx_rreg' "$t/set_event" || miss+=("Lesezugriffe (amdgpu:amdgpu_device_rreg)")
		grep -qxE 'amdgpu:amdgpu_device_wreg|amdgpu:\*|gx:gx_wreg' "$t/set_event" || miss+=("Schreibzugriffe (amdgpu:amdgpu_device_wreg)")
	fi
	kb=$(gx_num "$t/buffer_size_kb")
	[ "${kb:-0}" -gt 0 ] || miss+=("der Puffer (buffer_size_kb)")
	[ "$(cat "$t/tracing_on" 2>/dev/null)" = 1 ] || miss+=("die Aufnahme (tracing_on)")
	if [ ${#miss[@]} -gt 0 ]; then
		GX_STATUS="Mitschrift nicht scharf, es fehlt: $(printf '%s, ' "${miss[@]}" | sed 's/, $//')"
		echo "$GX_STATUS"
		return 1
	fi
	echo "armed: $(grep -cE '^(amdgpu|gx):' "$t/set_event") register events on, buffer $kb KiB per CPU"
}

# gx_lost <tracefs>: events the ring buffer lost (overrun + dropped, all CPUs), "?" when unreadable.
gx_lost() {
	local s
	s=$(cat "$1"/per_cpu/cpu*/stats 2>/dev/null) || { echo "?"; return; }
	awk '/^(overrun|dropped events):/ {s += $NF} END {print s + 0}' <<< "$s"
}

gx_empty() {
	GX_STATUS="Mitschrift LEER: $1"
	echo "$GX_STATUS"
}

# gx_trace_dump <tracefs> <out.zst>: packs the trace into <out.zst> and checks it. First output line is the one plain
# line (also in GX_STATUS), the second the counts. Returns 0 only for a trace with register reads and writes.
# A failed stage leaves no file behind; a file that packed but holds no register lines stays (tiny) beside the LEER line.
gx_trace_dump() {
	local t=$1 f=$2 part="$2.part" st counts n w r i lost
	rm -f "$part"
	cat "$t/trace" | zstd -q -T0 -9 -f > "$part"
	st=("${PIPESTATUS[@]}")
	if [ "${st[0]}" != 0 ]; then
		rm -f "$part"
		gx_empty "Lesen der Mitschrift fehlgeschlagen (cat $t/trace rc=${st[0]})"
		return 1
	fi
	if [ "${st[1]}" != 0 ]; then
		rm -f "$part"
		gx_empty "Packen fehlgeschlagen (zstd rc=${st[1]})"
		return 1
	fi
	if ! zstd -q -t "$part" 2>/dev/null; then
		rm -f "$part"
		gx_empty "die gepackte Datei ist kaputt (zstd -t)"
		return 1
	fi
	# The pipe's exit codes travel out of the command substitution on the same line as the counts.
	counts=$(zstd -qdc "$part" | awk '/ amdgpu_device_wreg: | gx_wreg: / {w++} / amdgpu_device_rreg: | gx_rreg: / {r++}
		/ gx_ind_/ {i++} !/^#/ {a++} END {printf "%d %d %d %d", a, w, r, i}'
		echo " ${PIPESTATUS[0]} ${PIPESTATUS[1]}")
	read -r n w r i st0 st1 <<< "$counts"
	if [ "${st0:-x}" != 0 ] || [ "${st1:-x}" != 0 ]; then
		rm -f "$part"
		gx_empty "Auszählen fehlgeschlagen (zstd -d rc=${st0:-?}, awk rc=${st1:-?})"
		return 1
	fi
	mv -f "$part" "$f"
	lost=$(gx_lost "$t")
	GX_LOST=$lost
	if [ "$n" = 0 ]; then
		gx_empty "keine einzige Zeile, nur der Kopf (die Ereignisse waren aus oder der Puffer wurde vorher geleert)"
	elif [ "$w" = 0 ] || [ "$r" = 0 ]; then
		gx_empty "keine Registerzugriffe ($w Schreib-, $r Lesezugriffe in $n Zeilen)"
	else
		GX_STATUS="Mitschrift: OK, $n Zeilen ($w Schreib-, $r Lesezugriffe), verloren $lost"
		echo "$GX_STATUS"
	fi
	echo "counts: wreg $w rreg $r indirect $i all $n lost $lost"
	[ "${GX_STATUS#Mitschrift: OK}" != "$GX_STATUS" ]
}

# gx_incomplete <status file> <problem>...: after an OK dump, a trace that is not whole must not read as OK. Problems
# are the caller's (a bind that ran over its time or failed) and the dump's lost events: with overwrite 0 a full
# buffer drops the newest events, so the end of the init is missing. Any of them rewrites the plain line (the status
# file's first line, GX_STATUS) to "Mitschrift UNVOLLSTÄNDIG: ..." and returns 1; 0 when the trace is whole.
gx_incomplete() {
	local f=$1 p rest
	shift
	p=("$@")
	case "$GX_LOST" in
	0) ;;
	'' | '?') p+=("Verluste nicht lesbar (per_cpu/*/stats)") ;;
	*) p+=("$GX_LOST Ereignisse verloren (Puffer voll, das Ende fehlt)") ;;
	esac
	[ ${#p[@]} = 0 ] && return 0
	rest=$(head -1 "$f")
	GX_STATUS="Mitschrift UNVOLLSTÄNDIG: $(printf '%s, ' "${p[@]}" | sed 's/, $//'); aufgenommen ${rest#Mitschrift: OK, }"
	{ echo "$GX_STATUS"; tail -n +2 "$f"; } > "$f.new" && mv -f "$f.new" "$f"
	return 1
}
