#!/bin/bash
# SN-3: the mixer (crates/snd) on two QEMU intel-hda controllers (hda1 stands in for HDMI), sound only into
# WAV files (every hda-duplex has a `wav` audiodev; QEMU never picks a default backend for it).
#  A: 4 CPUs, MSI, default latency (40 ms): sn.mix null output, state, two clients, switch, resample;
#     mixcheck.py: the two-client mix sample-exact with saturation, every frame exactly once across
#     hda0 -> hda1 -> hda0, the 44.1 kHz client as a clean 1 kHz sine at 48 kHz.
#  B: 16 CPUs, polled (hda_irq=off), 20 ms latency: switch and two again.
set -eu
cd "$BITSOS_ROOT"
RUN="$BITSOS_RUN_DIR"
HERE="$BITSOS_ROOT/host/tests/sn"
fail=0
check() { if grep -aq -- "$2" "$1"; then echo "ok   $3"; else echo "FAIL $3 (no '$2')"; fail=1; fi; }
nocheck() { if grep -aq -- "$2" "$1"; then echo "FAIL $3:"; grep -a -- "$2" "$1" | head -5; fail=1; else echo "ok   $3"; fi; }

boot() { # name smp config-extra log w0 w1
    local name="$1" smp="$2" cfg="$3" log="$4" w0="$5" w1="$6"
    rm -f "$log" "$w0" "$w1"
    cargo xtask image --name "$BITSOS_LANE-$name" --ktest --config-extra "$cfg;loglevel=info"
    cargo xtask qemu --name "$BITSOS_LANE-$name" --timeout 180 --smp "$smp" --net virtio --serial-log "$log" -- \
        -audiodev "wav,id=a0,path=$w0,out.frequency=48000,out.channels=2,out.format=s16" \
        -audiodev "wav,id=a1,path=$w1,out.frequency=48000,out.channels=2,out.format=s16" \
        -device intel-hda,id=h0 -device hda-duplex,bus=h0.0,audiodev=a0,mixer=off \
        -device intel-hda,id=h1 -device hda-duplex,bus=h1.0,audiodev=a1,mixer=off || true
    touch "$log"
    grep -a "KTEST" "$log" || true
}

wavs() { # tag w0 w1 modes...
    local tag="$1" w0="$2" w1="$3"
    shift 3
    for m in "$@"; do
        case "$m" in
            switch) python3 -I "$HERE/mixcheck.py" switch "$w0" "$w1" || { echo "FAIL $tag: switch WAVs"; fail=1; } ;;
            *) python3 -I "$HERE/mixcheck.py" "$m" "$w0" || { echo "FAIL $tag: $m WAV"; fail=1; } ;;
        esac
    done
}

# --- A ---
LOG="$RUN/mix-a.log"; W0="$RUN/mix-a0.wav"; W1="$RUN/mix-a1.wav"
# Not sn.mix.leave_running: its 10 s of the switch signal would mix into the switch check (Reload only).
boot mix-a 4 "ktest=sn.mix.null,sn.mix.state,sn.mix.two,sn.mix.switch,sn.mix.resample" "$LOG" "$W0" "$W1"
check "$LOG" 'KTEST done passed=5 failed=0 skipped=0' "A: the five sn.mix tests pass"
check "$LOG" 'snd mix.thread name=snd/mix class=kernel_rt prio=45 lead=1920 floor=960 period=480' "A: mixer thread KernelRt, 40 ms lead (20 ms floor) in 10 ms periods"
check "$LOG" 'snd mix.table rate=44100 phases=160 step=147 taps=64' "A: 160-phase table for 44.1 kHz"
check "$LOG" 'snd mix.null_output' "A: null output used without an endpoint (sn.mix.null)"
check "$LOG" 'snd mix.switch from=hda0/jack/0:0x3 to=hda1/jack/0:0x3 mode=make_before_break' "A: switch hda0 -> hda1 without a break"
check "$LOG" 'snd mix.switch from=hda1/jack/0:0x3 to=hda0/jack/0:0x3 mode=make_before_break' "A: switch back"
check "$LOG" 'snd mix.retired endpoint=hda1' "A: the old output retires after its margin"
check "$LOG" 'snd client.underrun id=.* name=ktest.starved count=1' "A: a starved client counts its underrun"
nocheck "$LOG" ' E snd \| E ksync' "A: no error events"
wavs A "$W0" "$W1" two switch resample

# --- B ---
LOG="$RUN/mix-b.log"; W0="$RUN/mix-b0.wav"; W1="$RUN/mix-b1.wav"
boot mix-b 16 "ktest=sn.mix.switch,sn.mix.two;hda_irq=off;snd_latency_ms=20" "$LOG" "$W0" "$W1"
check "$LOG" 'KTEST done passed=2 failed=0 skipped=0' "B: switch and two pass polled with 20 ms"
check "$LOG" 'snd hda.irq_mode ctrl=hda0 mode=polled' "B: polled"
check "$LOG" 'snd mix.thread name=snd/mix class=kernel_rt prio=45 lead=1024 floor=512 period=256' "B: 20 ms lead"
nocheck "$LOG" ' E snd \| E ksync' "B: no error events"
wavs B "$W0" "$W1" two switch
exit $fail
