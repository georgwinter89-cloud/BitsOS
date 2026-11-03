#!/bin/bash
# SN-2: the HD Audio driver on QEMU's intel-hda + hda-duplex, sound only into WAV files (never the host's sound
# devices: every hda-duplex gets a `wav` audiodev; QEMU never picks a default backend for it).
#  A: two controllers at once (the SN-3 switching setup), MSI, 4 CPUs: hda0 plays 2 s at 48 kHz (mixer off),
#     hda1 2 s at 44.1 kHz (mixer on: the DAC amp at 0 dB); each WAV is checked sample-exactly (wavcheck.py);
#     probe/dump/jack ktests too.
#  B: one controller, 16 CPUs, CONFIG hda_irq=off (polled) and hda_snoop=off (UC/WC DMA, or the logged fallback).
#  C: ich9-intel-hda (8086:293E): Quiesce with a running stream and the restart, a bring-up over a running
#     stream (QemuOnly tests).
#  D: SN-7, the reboot's Quiesce order with the mixer streaming silence (QemuOnly).
set -eu
cd "$BITSOS_ROOT"
RUN="$BITSOS_RUN_DIR"
HERE="$BITSOS_ROOT/host/tests/sn"
fail=0
check() { if grep -aq -- "$2" "$1"; then echo "ok   $3"; else echo "FAIL $3 (no '$2')"; fail=1; fi; }
# A log line the ktest also asserts in the kernel (KTEST ok): its absence is reported, not failed. SN-12 fixer: the
# serial log can lose its last lines when QEMU exits right after `KTEST done` (seen once in section C).
soft() { if grep -aq -- "$2" "$1"; then echo "ok   $3"; else echo "note $3: no '$2' (log tail lost? the ktest asserts it)"; fi; }
nocheck() { if grep -aq -- "$2" "$1"; then echo "FAIL $3:"; grep -a -- "$2" "$1" | head -5; fail=1; else echo "ok   $3"; fi; }

boot() { # name smp config-extra log -- qemu args
    local name="$1" smp="$2" cfg="$3" log="$4"
    shift 5
    cargo xtask image --name "$BITSOS_LANE-$name" --ktest --config-extra "$cfg;loglevel=info"
    rm -f "$log"
    cargo xtask qemu --name "$BITSOS_LANE-$name" --timeout 180 --smp "$smp" --net virtio --serial-log "$log" -- "$@" || true
    touch "$log"
    grep -a "KTEST" "$log" || true
}

# --- A ---
LOG="$RUN/a.log"
W0="$RUN/hda0.wav"; W1="$RUN/hda1.wav"
rm -f "$W0" "$W1"
boot hda-a 4 "ktest=sn.hda.play,sn.hda.probe,sn.hda.dump,sn.hda.jack" "$LOG" -- \
    -audiodev "wav,id=a0,path=$W0,out.frequency=48000,out.channels=2,out.format=s16" \
    -audiodev "wav,id=a1,path=$W1,out.frequency=44100,out.channels=2,out.format=s16" \
    -device intel-hda,id=h0 -device hda-duplex,bus=h0.0,audiodev=a0,mixer=off \
    -device intel-hda,id=h1 -device hda-duplex,bus=h1.0,audiodev=a1,mixer=on
check "$LOG" 'KTEST done passed=3 failed=0 skipped=1' "A: play, probe, dump pass; jack skips (no presence detect)"
for t in play probe dump; do check "$LOG" "KTEST ok sn.hda.$t " "A: sn.hda.$t"; done
check "$LOG" 'snd hda.codec ctrl=hda0 cad=0 vendor=0x1af40021 .*outputs=1 .*missing=0 timeouts=0' "A: hda0 codec read over the CORB"
check "$LOG" 'snd hda.codec ctrl=hda1 cad=0 vendor=0x1af40022 .*outputs=1 .*missing=0 timeouts=0' "A: hda1 codec read over the CORB (mixer on: 1AF40022)"
check "$LOG" 'snd hda.output ctrl=hda0 cad=0 pin=0x3 kind=line_out .*path=03-02' "A: line-out pin 3 -> DAC 2"
check "$LOG" 'pci msi.alloc .*driver=hda mode=msi vectors=1 ' "A: MSI for hda"
check "$LOG" 'snd hda.irq_mode ctrl=hda1 mode=msi' "A: hda1 interrupt-driven"
check "$LOG" 'snd hda.thread ctrl=hda1 thread=hda/1 class=kernel_rt' "A: service thread per controller"
check "$LOG" 'ktest hda.played ctrl=hda0 rate=48000 frames=96000 .*irq_driven=true' "A: hda0 played 96000 frames at 48 kHz, MSI"
check "$LOG" 'ktest hda.played ctrl=hda1 rate=44100 frames=88200 .*irq_driven=true' "A: hda1 played 88200 frames at 44.1 kHz, MSI"
check "$LOG" 'snd hda.close ctrl=hda0 .*underruns=0 lpib_bogus=0' "A: no underrun, no bogus LPIB"
check "$LOG" 'ktest hda.dump ctrl=hda0 cad=0 line="0 00 F0000 1AF40021"' "A: dump lines in the log"
python3 -I "$HERE/wavcheck.py" "$W0" --rate 48000 --frames 96000 || fail=1
python3 -I "$HERE/wavcheck.py" "$W1" --rate 44100 --frames 88200 || fail=1
nocheck "$LOG" ' E snd ' "A: no error events"

# --- B ---
LOG="$RUN/b.log"
W2="$RUN/hda2.wav"
rm -f "$W2"
boot hda-b 16 "ktest=sn.hda.play;hda_irq=off;hda_snoop=off" "$LOG" -- \
    -audiodev "wav,id=a0,path=$W2,out.frequency=48000,out.channels=2,out.format=s16" \
    -device intel-hda,id=h0 -device hda-duplex,bus=h0.0,audiodev=a0,mixer=off
check "$LOG" 'KTEST done passed=1 failed=0' "B: play passes polled and without snooping"
check "$LOG" 'snd hda.irq_mode ctrl=hda0 mode=polled' "B: polled (hda_irq=off)"
if grep -aq 'W snd hda.snoop_fallback' "$LOG"; then
    echo "ok   B: UC/WC memory refused (MM-4 before MM-6), logged fallback to snooping"
else
    check "$LOG" 'snd hda.open ctrl=hda0 .*snoop=false' "B: stream on uncached/write-combining memory"
fi
python3 -I "$HERE/wavcheck.py" "$W2" --rate 48000 --frames 96000 || fail=1
nocheck "$LOG" ' E snd ' "B: no error events"

# --- C ---
LOG="$RUN/c.log"
boot hda-c 4 "ktest=sn.hda.quiesce,sn.hda.stale,sn.hda.panic_stop" "$LOG" -- \
    -audiodev "wav,id=a0,path=$RUN/hda3.wav,out.frequency=48000" \
    -device ich9-intel-hda,id=h0 -device hda-duplex,bus=h0.0,audiodev=a0
check "$LOG" 'snd hda.probe .*device=0x293e' "C: ich9-intel-hda bound"
check "$LOG" 'KTEST done passed=3 failed=0' "C: quiesce, stale and panic_stop pass"
# The ktests assert these themselves (last_quiesce, last_stale, the panic report); the log lines are the record.
for t in quiesce stale panic_stop; do check "$LOG" "KTEST ok sn.hda.$t " "C: sn.hda.$t"; done
soft "$LOG" 'snd hda.quiesced ctrl=hda0 was=up' "C: quiesce note logged after the fact"
# SN-12: the codec's shutdown verbs went out and were answered (QEMU's codec keeps no power state).
soft "$LOG" 'snd hda.quiesced ctrl=hda0 was=up stuck=false codec=no_power_states' "C: codec shutdown answered"
soft "$LOG" 'ktest hda.panic_stop ctrl=hda0 was=up stuck=false codec=no_power_states' "C: the panic path's stop"
soft "$LOG" 'I snd hda.stale_dma ctrl=hda0 streams=1' "C: running stream found and stopped at bring-up"
nocheck "$LOG" ' E snd ' "C: no error events"

# --- D ---
# SN-7: the reboot's Quiesce order with the mixer streaming silence (sn.mix.reboot_order, QemuOnly): the stream
# DMA stays stopped, the mixer closes its output without reaching the controller and opens nothing again.
LOG="$RUN/d.log"
boot hda-d 4 "ktest=sn.mix.reboot_order" "$LOG" -- \
    -audiodev "wav,id=a0,path=$RUN/hda4.wav,out.frequency=48000" \
    -device intel-hda,id=h0 -device hda-duplex,bus=h0.0,audiodev=a0
check "$LOG" 'KTEST ok sn.mix.reboot_order ' "D: sn.mix.reboot_order"
check "$LOG" 'KTEST done passed=1 failed=0 skipped=0' "D: the reboot order passes, not skipped"
soft "$LOG" 'ktest mix.reboot_order ctrl=hda0 streams_before=1' "D: one stream ran before the Quiesce"
soft "$LOG" 'snd hda.quiesced ctrl=hda0 was=up stuck=false codec=no_power_states' "D: the engines were seen stopped, the codec shut down"
nocheck "$LOG" ' E snd ' "D: no error events"
exit $fail
