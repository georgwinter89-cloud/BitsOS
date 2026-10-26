#!/usr/bin/env python3
"""SN-3: checks of the WAVs QEMU's wav audiodevs wrote while the sn.mix ktests played through the mixer
(crates/snd/src/testsig.rs computes the same signals). A WAV holds the tests one after the other, separated by
silence; each check finds its own segment by content.
  two W0          the two-client mix sample-exactly: left sat(a + ((b*24576 + 0x4000) >> 15)), right a's counter;
                  saturation must occur (a: 24000 sine + counter, b: hash noise -16000..16000)
  switch W0 W1    one client switched hda0 -> hda1 -> hda0: every non-silent frame is the test signal
                  (left the 12000 sine of the frame number, right frame number + 1), inside each file in
                  order, and over both files every frame 1..57600 exactly once (no loss, no repeat)
  resample W0     a 44.1 kHz 1 kHz sine of amplitude 16000 resampled to 48 kHz: one segment with L == R of
                  48000 +- 100 frames, a least-squares fit at 1 kHz: amplitude within 0.5 %, THD+N < -80 dB
  alsa-gen DIR    SN-4: writes the WAVs aplay plays on V2 (s48.wav S16 48 kHz, f48.wav FLOAT_LE 48 kHz,
                  s441.wav S16 44.1 kHz)
  cobalt W0 S     SN-7: alsa-hw cobalt's S seconds of sawtooth unbroken (sample-exact) in W0
  alsa W0         SN-4: alsa-hw selftest's 1 s (raw ioctls), s48 and f48 (aplay -D hw:0,0) each sample-exactly
                  somewhere in W0, s441 (aplay -D default: plug -> hw, the mixer resamples) as a clean 1 kHz sine
"""
import array
import math
import struct
import sys

SINE48 = [round(12000 * math.sin(2 * math.pi * i / 48)) for i in range(48)]


def read_wav(path):
    data = open(path, "rb").read()
    if data[0:4] != b"RIFF" or data[8:12] != b"WAVE":
        raise ValueError(f"{path}: not a RIFF/WAVE file")
    pos, fmt, pcm = 12, None, None
    while pos + 8 <= len(data):
        cid, size = data[pos:pos + 4], struct.unpack("<I", data[pos + 4:pos + 8])[0]
        body = data[pos + 8:]
        if cid == b"fmt ":
            fmt = struct.unpack("<HHIIHH", body[:16])
        elif cid == b"data":
            pcm = body if size == 0 or size > len(body) else body[:size]
            break
        pos += 8 + size + (size & 1)
    if fmt is None or pcm is None:
        raise ValueError(f"{path}: no fmt or data chunk")
    tag, channels, rate, _, _, bits = fmt
    if tag != 1 or channels != 2 or bits != 16 or rate != 48000:
        raise ValueError(f"{path}: want PCM 48 kHz 2x16 bit, got tag={tag} rate={rate} channels={channels} bits={bits}")
    n = len(pcm) // 4
    s = struct.unpack(f"<{2 * n}h", pcm[:4 * n])
    return [(s[2 * i], s[2 * i + 1]) for i in range(n)]


def segments(f, gap=64):
    """Runs of sound separated by at least `gap` silent frames: [(start, end)]."""
    out, start, quiet = [], None, 0
    for i, x in enumerate(f):
        if x != (0, 0):
            if start is None:
                start = i
            quiet = 0
        elif start is not None:
            quiet += 1
            if quiet >= gap:
                out.append((start, i - quiet + 1))
                start, quiet = None, 0
    if start is not None:
        out.append((start, len(f) - quiet))
    return out


def sig_a(i):
    return 2 * SINE48[i % 48], i % 32767 + 1


def sig_b(i):
    h = ((i * 2654435761) & 0xFFFFFFFF) >> 16
    return h % 32001 - 16000, 0


def mixed(i, ga=32768, gb=24576):
    a, b = sig_a(i), sig_b(i)
    return tuple(max(-32768, min(32767, ((a[c] * ga + 0x4000) >> 15) + ((b[c] * gb + 0x4000) >> 15))) for c in range(2))


def is_c(x):
    n = (x[1] & 0xFFFF) - 1
    return n >= 0 and x[0] == SINE48[n % 48]


def check_two(path, n=48000):
    f = read_wav(path)
    for s, e in segments(f):
        if f[s] != mixed(0):
            continue
        if e - s < n:
            return f"two: segment at {s} has {e - s} frames, want {n}"
        for i in range(n):
            if f[s + i] != mixed(i):
                return f"two: frame {i} is {f[s + i]}, want {mixed(i)}"
        sat = sum(1 for i in range(n) if f[s + i][0] in (32767, -32768))
        if sat == 0:
            return "two: no saturated sample"
        if any(x != (0, 0) for x in f[s + n:e]):
            return f"two: sound after the {n} frames"
        print(f"ok   two: {n} frames sample-exact at {s} ({sat} saturated)")
        return None
    return "two: no segment starts with the mix's first frame"


def check_switch(p0, p1, n=57600):
    seen = []
    for path in (p0, p1):
        f = read_wav(path)
        counts = []
        for s, e in segments(f):
            seg = f[s:e]
            loud = [x for x in seg if x != (0, 0)]
            if sum(1 for x in loud if is_c(x)) < 0.99 * len(loud):
                continue
            bad = [x for x in loud if not is_c(x)]
            if bad:
                return f"switch: {path}: {len(bad)} frames are not the signal, first {bad[0]}"
            counts += [x[1] & 0xFFFF for x in loud]
        if any(b <= a for a, b in zip(counts, counts[1:])):
            return f"switch: {path}: frames out of order"
        print(f"     switch: {path}: {len(counts)} frames, runs {runs(counts)}")
        seen += counts
    seen.sort()
    if seen != list(range(1, n + 1)):
        lost = sorted(set(range(1, n + 1)) - set(seen))
        dup = len(seen) - len(set(seen))
        return f"switch: {len(lost)} frames lost (first {lost[:5]}), {dup} doubled"
    print(f"ok   switch: all {n} frames exactly once across both outputs")
    return None


def runs(c):
    out, start = [], None
    for i, v in enumerate(c):
        if start is None:
            start = v
        if i + 1 == len(c) or c[i + 1] != v + 1:
            out.append(f"{start}-{v}")
            start = None
    return ",".join(out)


def fit(x, f):
    w = 2 * math.pi * f / 48000
    ss = cc = sc = xs = xc = 0.0
    for i, v in enumerate(x):
        s, c = math.sin(w * i), math.cos(w * i)
        ss += s * s
        cc += c * c
        sc += s * c
        xs += v * s
        xc += v * c
    det = ss * cc - sc * sc
    a, b = (xs * cc - xc * sc) / det, (xc * ss - xs * sc) / det
    res = sig = 0.0
    for i, v in enumerate(x):
        m = a * math.sin(w * i) + b * math.cos(w * i)
        res += (v - m) ** 2
        sig += m * m
    return math.hypot(a, b), 10 * math.log10(res / sig)


def check_resample(path, n=48000):
    f = read_wav(path)
    for s, e in segments(f):
        seg = f[s:e]
        if abs(len(seg) - n) > 100 or any(x[0] != x[1] for x in seg):
            continue
        amp, thd = fit([x[0] for x in seg[1000:1000 + 38400]], 1000.0)
        print(f"     resample: segment at {s}, {len(seg)} frames, amplitude {amp:.1f}, THD+N {thd:.1f} dB")
        if abs(amp / 16000 - 1) > 0.005:
            return f"resample: amplitude {amp:.1f}, want 16000"
        if thd > -80:
            return f"resample: THD+N {thd:.1f} dB"
        print("ok   resample: 1 kHz at 48 kHz, amplitude and distortion within limits")
        return None
    return "resample: no segment of about 48000 frames with L == R"


# --- SN-4 ---
def alsa_hw(n):
    l, r = ((n * 37) & 0x3fff) - 8192, 8191 - ((n * 19) & 0x3fff)
    return (l, r)


def alsa_s48(n):
    return (round(10000 * math.sin(2 * math.pi * 997 * n / 48000 + 0.3)), round(7000 * math.sin(2 * math.pi * 1499 * n / 48000)))


def alsa_f48(n):
    k = round(9000 * math.sin(2 * math.pi * 661 * n / 48000))
    return (k, -k)


def alsa_s441(n):
    v = round(16000 * math.sin(2 * math.pi * 1000 * n / 44100))
    return (v, v)


def wav_bytes(frames, rate, flt=False):
    if flt:
        pcm = b"".join(struct.pack("<ff", l / 32768, r / 32768) for l, r in frames)
        fmt = struct.pack("<HHIIHH", 3, 2, rate, rate * 8, 8, 32)
    else:
        pcm = b"".join(struct.pack("<hh", l, r) for l, r in frames)
        fmt = struct.pack("<HHIIHH", 1, 2, rate, rate * 4, 4, 16)
    body = b"WAVE" + b"fmt " + struct.pack("<I", len(fmt)) + fmt + b"data" + struct.pack("<I", len(pcm)) + pcm
    return b"RIFF" + struct.pack("<I", len(body)) + body


def alsa_gen(d):
    for name, sig, rate, flt in [("s48", alsa_s48, 48000, False), ("f48", alsa_f48, 48000, True), ("s441", alsa_s441, 44100, False)]:
        open(f"{d}/{name}.wav", "wb").write(wav_bytes([sig(n) for n in range(rate)], rate, flt))
    print(f"ok   alsa-gen: s48.wav f48.wav s441.wav in {d}")


def find_exact(f, want):
    first = want[0]
    for i in range(len(f) - len(want) + 1):
        if f[i] == first and f[i:i + len(want)] == want:
            return i
    return None


def check_alsa(path):
    f = read_wav(path)
    for name, sig in [("alsa-hw selftest (raw ioctls, S16)", alsa_hw), ("aplay hw:0,0 S16 48 kHz", alsa_s48), ("aplay hw:0,0 FLOAT_LE 48 kHz", alsa_f48)]:
        at = find_exact(f, [sig(n) for n in range(48000)])
        if at is None:
            return f"{name}: its 48000 frames are not in the WAV sample-exactly"
        print(f"ok   {name}: all 48000 frames sample-exact at frame {at}")
    for s, e in segments(f):
        seg = f[s:e]
        if abs(len(seg) - 48000) > 200 or any(x[0] != x[1] for x in seg):
            continue
        amp, thd = fit([x[0] for x in seg[1000:1000 + 38400]], 1000.0)
        print(f"     aplay default 44.1 kHz: segment at {s}, {len(seg)} frames, amplitude {amp:.1f}, THD+N {thd:.1f} dB")
        if abs(amp / 16000 - 1) > 0.005 or thd > -80:
            return f"aplay default 44.1 kHz: amplitude {amp:.1f} / THD+N {thd:.1f} dB"
        print("ok   aplay default (plug -> hw) 44.1 kHz: resampled to 48 kHz, clean")
        return None
    return "aplay default 44.1 kHz: no segment of about 48000 frames with L == R"


def check_cobalt(path, seconds):
    """SN-7: alsa-hw cobalt's sawtooth (alsa_hw) runs through the WAV without a break: no XRUN gap, no
    underrun, no frame lost or repeated. Bytes are compared (the WAV holds millions of frames)."""
    data = open(path, "rb").read()
    at = data.find(b"data")
    if data[0:4] != b"RIFF" or at < 0:
        return f"{path}: not a RIFF/WAVE file"
    pcm = data[at + 8:]
    n = int(seconds) * 48000
    want = array.array("h", [v for i in range(n) for v in alsa_hw(i)]).tobytes()
    start = pcm.find(want[:256])
    while start >= 0 and start % 4:
        start = pcm.find(want[:256], start + 1)
    if start < 0:
        return "cobalt: the sawtooth's start is not in the WAV"
    got = pcm[start:start + len(want)]
    if got == want:
        print(f"ok   cobalt: all {n} frames ({seconds} s) sample-exact without a break at frame {start // 4}")
        return None
    lo, hi = 0, min(len(got), len(want)) // 4
    while lo < hi:
        mid = (lo + hi) // 2
        if got[:4 * (mid + 1)] == want[:4 * (mid + 1)]:
            lo = mid + 1
        else:
            hi = mid
    return f"cobalt: the sawtooth breaks at frame {lo} of {n} ({lo / 48000:.3f} s; WAV has {len(got) // 4} frames from its start)"


def main():
    mode, files = sys.argv[1], sys.argv[2:]
    err = {"two": check_two, "switch": check_switch, "resample": check_resample, "alsa": check_alsa, "alsa-gen": alsa_gen,
           "cobalt": check_cobalt}[mode](*files)
    if err:
        print(f"FAIL {err}")
        sys.exit(1)


if __name__ == "__main__":
    main()
