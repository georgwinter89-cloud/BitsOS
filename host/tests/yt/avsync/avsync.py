#!/usr/bin/env python3
"""YT-5: A/V offset of the click/flash clip (make-clip.sh) from Cobalt's frame trace and QEMU's WAV.

  avsync.py TRACE WAV SERIAL_LOG SECONDS

TRACE      av-trace.txt from V2 (BITSOS_AV_TRACE): "shown <monotonic us> <pts us> <offset us|->" per frame
WAV        QEMU's wav audiodev output (s16, 48 kHz, 2 channels): the HDA codec's DAC stream
SERIAL_LOG the guest's serial log: `snd mix.open` gives the guest time at which the output (and so the WAV) began
SECONDS    the clip's length (flashes and clicks at 1 .. SECONDS-1 s)

Prints per flash/click pair: when the white frame reached the screen, when its click is in the WAV (both guest
monotonic time), and sound minus picture (> 0: sound late). The WAV's time base assumes one output session (one
mix.open, nothing played before it) and QEMU writing the WAV at the guest's pace; QEMU timing is not judged
(CONTRACTS ktest rules), so this only reports. Exit 1 only when the data is incomplete (no trace, no clicks)."""
import array
import re
import struct
import sys

RATE = 48000
LOUD = 3000          # the burst is 0.5 full scale; Opus' ringing before it stays far below
QUIET_GAP = RATE // 2


def flashes(trace):
    shown = {}
    for line in open(trace):
        p = line.split()
        if len(p) == 4 and p[0] == "shown":
            t, pts = int(p[1]), int(p[2])
            if pts % 1_000_000 == 0 and pts > 0 and pts // 1_000_000 not in shown:
                shown[pts // 1_000_000] = t
    return shown


def offsets(trace):
    v = [int(p[3]) for p in (l.split() for l in open(trace)) if len(p) == 4 and p[0] == "shown" and p[3] != "-"]
    return v


def clicks(wav):
    d = open(wav, "rb").read()
    if d[:4] != b"RIFF" or d[8:12] != b"WAVE":
        raise SystemExit(f"RED: {wav} is no WAV file")
    i = 12
    fmt = data = None
    while i + 8 <= len(d):
        cid, n = d[i:i + 4], struct.unpack_from("<I", d, i + 4)[0]
        if cid == b"fmt ":
            fmt = struct.unpack_from("<HHIIHH", d, i + 8)
        elif cid == b"data":
            data = d[i + 8:i + 8 + n] if n and i + 8 + n <= len(d) else d[i + 8:]
            break
        i += 8 + n + (n & 1)
    if not fmt or fmt[1] != 2 or fmt[2] != RATE or fmt[5] != 16 or data is None:
        raise SystemExit(f"RED: {wav}: want s16 stereo {RATE} Hz, got {fmt}")
    frames = len(data) // 4
    a = array.array("h")
    a.frombytes(data[:frames * 4])
    if sys.byteorder != "little":
        a.byteswap()
    on, last_loud = [], -QUIET_GAP
    for f in range(frames):
        if abs(a[2 * f]) > LOUD or abs(a[2 * f + 1]) > LOUD:
            if f - last_loud >= QUIET_GAP:
                on.append(f)
            last_loud = f
    return on, frames


def opens(serial):
    t = []
    for line in open(serial, errors="replace"):
        if " snd mix.open " in line:
            m = re.match(r"\[\s*([0-9.]+)\]", line)
            if m:
                t.append(float(m.group(1)))
    return t


def main():
    trace, wav, serial, secs = sys.argv[1], sys.argv[2], sys.argv[3], int(sys.argv[4])
    fl = flashes(trace)
    off = offsets(trace)
    on, frames = clicks(wav)
    op = opens(serial)
    print(f"trace: {sum(1 for _ in open(trace))} frames shown, flashes seen {len(fl)}/{secs - 1}")
    if off:
        off.sort()
        print(f"picture vs Cobalt's audio clock (pts - clock at show, > 0 early): n={len(off)} "
              f"median {off[len(off) // 2] / 1000:.1f} ms, p5 {off[len(off) // 20] / 1000:.1f}, "
              f"p95 {off[len(off) * 19 // 20] / 1000:.1f}, min {off[0] / 1000:.1f}, max {off[-1] / 1000:.1f}")
    print(f"wav: {frames / RATE:.2f} s, clicks found {len(on)}; mix.open at {op} s")
    if not fl or not on:
        print("RED: no flashes in the trace or no clicks in the WAV")
        return 1
    # The clicks are k = 1 .. secs-1 in order; the WAV may start late (first clicks before the output opened are
    # not lost: Cobalt starts its sink before the first frame) or hold earlier sound (then the count is off).
    if len(on) != secs - 1:
        print(f"note: {len(on)} clicks for {secs - 1} flashes: the pairs below assume the last click is k={secs - 1}")
    first_k = secs - len(on)
    if len(op) != 1:
        print(f"unanchored: {len(op)} output sessions (need exactly 1 to place the WAV in guest time); "
              "only the click spacing is reported")
        gaps = [(b - a) / RATE * 1000 for a, b in zip(on, on[1:])]
        print(f"click spacing: min {min(gaps):.1f} ms max {max(gaps):.1f} ms (1000 = no gap/stretch)" if gaps else "")
        return 0
    t0 = op[0]
    d = []
    for i, n in enumerate(on):
        k = first_k + i
        if k in fl:
            snd = t0 + n / RATE
            pic = fl[k] / 1e6
            d.append((k, pic, snd, (snd - pic) * 1000))
    for k, pic, snd, ms in d:
        print(f"  k={k:2d} picture {pic:10.4f} s  sound {snd:10.4f} s  sound-picture {ms:+7.1f} ms")
    if d:
        v = sorted(x[3] for x in d)
        print(f"sound minus picture over {len(v)} pairs: median {v[len(v) // 2]:+.1f} ms, min {v[0]:+.1f}, "
              f"max {v[-1]:+.1f}, spread {v[-1] - v[0]:.1f} ms (QEMU: reported, not judged)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
