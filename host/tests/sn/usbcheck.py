#!/usr/bin/env python3
"""SN-8b: checks the WAVs of host/tests/sn/50-usbaudio.sh. The ktests play crates/snd/src/testsig.rs `c`
(left the 12000 sine of the frame number, right frame number + 1, mod 2^16) through the kernel mixer.
QEMU's usb-audio plays at its default volume (240 of 255 in its mixer, which is what the driver's 0 dB maps
to), so every USB sample arrives scaled by 240/255 (+-2 for rounding, as host/tests/us/isowav.py); the
intel-hda WAV (mixer=off) is exact. A data size of 0 in the header (QEMU killed early) means: all after it.
  tone U N        sn.usb.tone: the first sound in U is frames 0..N-1 of `c`, every one once, in order, none bad
  unplug U1 H U2 sn.usb.unplug: every non-silent frame of U1 (headset), H (jack), U2 (the headset plugged in
                  again, its own audiodev) is `c`; each file in order; one file goes on where the one before
                  stopped (within 1 s: what was queued at the pull is lost), so no frame plays twice
"""
import math
import struct
import sys

SINE48 = [round(12000 * math.sin(2 * math.pi * i / 48)) for i in range(48)]
TOL = 2


def read(path):
    d = open(path, "rb").read()
    if d[0:4] != b"RIFF" or d[8:12] != b"WAVE":
        sys.exit(f"RED: {path}: not a RIFF/WAVE file")
    pos, fmt, data = 12, None, None
    while pos + 8 <= len(d):
        cid, size = d[pos:pos + 4], struct.unpack("<I", d[pos + 4:pos + 8])[0]
        if cid == b"fmt ":
            fmt = struct.unpack("<HHIIHH", d[pos + 8:pos + 24])
        if cid == b"data":
            data = d[pos + 8:] if size == 0 else d[pos + 8:pos + 8 + size]
            break
        pos += 8 + size
    if fmt is None or data is None:
        sys.exit(f"RED: {path}: no fmt or data chunk")
    if fmt[1] != 2 or fmt[2] != 48000 or fmt[5] != 16:
        sys.exit(f"RED: {path}: format {fmt}, want 2 channels 48000 Hz 16 bit")
    n = len(data) // 4
    s = struct.unpack("<%dh" % (2 * n), data[:4 * n])
    return list(zip(s[0::2], s[1::2]))


def s16(v):
    v &= 0xFFFF
    return v - 0x10000 if v >= 0x8000 else v


def matches(x, u, scale):
    """Frame x is `c` of absolute frame number u (left the sine of u, right u + 1 as 16 bit)."""
    want = (SINE48[u % 48] * scale, s16(u + 1) * scale)
    return abs(x[0] - want[0]) <= TOL and abs(x[1] - want[1]) <= TOL + abs(want[1]) * 0.002


def candidates(x, scale):
    """Counter values (frame number mod 2^16) the right channel allows."""
    n = (round(x[1] / scale) - 1) & 0xFFFF
    return [(n + d) & 0xFFFF for d in (0, -1, 1, -2, 2)]


def frames_of(path, scale, last=-1, first=None):
    """Absolute frame numbers of every non-silent frame, following on from `last` (a gap of up to three
    counter wraps, 4 s, is resolved by the sine, whose period 48 does not divide 2^16); `first` picks the first
    frame's number instead. Returns (numbers, frames that are not `c`)."""
    out, bad = [], 0
    for x in read(path):
        if x == (0, 0):
            continue
        u = None
        if first is not None and not out:
            u = first(x)
        else:
            # The nearest frame after `last` that fits (a repeat of `last` itself counts, to be caught as one).
            vs = sorted(b + k for c in candidates(x, scale) for b in [last + ((c - last) & 0xFFFF)] for k in (0, 0x10000, 0x20000))
            u = next((v for v in vs if matches(x, v, scale)), None)
        if u is None:
            bad += 1
            if bad <= 3:
                print(f"     {path}: frame {x} is not the signal after frame {last}")
            continue
        out.append(u)
        last = u
    return out, bad


def check_tone(u, n):
    f, bad = frames_of(u, 240 / 255)
    if bad:
        return f"tone: {bad} frames are not the signal"
    # Frame 0 is (0, 1); scaled, its right sample rounds to 0 in QEMU and the frame reads as silence.
    if f[:1] == [1]:
        f.insert(0, 0)
    if f[:n] != list(range(n)):
        first = next((i for i, (a, b) in enumerate(zip(f, range(n))) if a != b), len(f))
        return f"tone: {len(f)} signal frames, want 0..{n - 1} in order; first difference at {first}"
    print(f"ok   tone: frames 0..{n - 1} each once, in order, on the headset (scale 240/255)")
    return None


def check_unplug(u1, h, u2):
    S = 240 / 255
    last, parts = -1, []
    for path, scale, least in ((u1, S, 9600), (h, 1.0, 4800), (u2, S, 9600)):
        f, bad = frames_of(path, scale, last)
        if bad:
            return f"unplug: {path}: {bad} frames are not the signal"
        if any(b <= a for a, b in zip(f, f[1:])):
            return f"unplug: {path}: frames out of order or repeated"
        if len(f) < least:
            return f"unplug: {path}: only {len(f)} signal frames, want {least}"
        if parts and not 0 < f[0] - last <= 48000:
            return f"unplug: {path} starts at {f[0]} after {last}: played twice or a second lost"
        gaps = sum(1 for a, b in zip(f, f[1:]) if b != a + 1)
        print(f"     unplug: {path}: {len(f)} frames {f[0]}..{f[-1]}, {gaps} gaps")
        parts.append(f)
        last = f[-1]
    lost = parts[1][0] - parts[0][-1] - 1
    back = parts[2][0] - parts[1][-1] - 1
    print(f"ok   unplug: headset, jack, headset again in order, no frame twice, no garbage; lost at the pull {lost}, at the replug {back}")
    return None


def main():
    a = sys.argv[1:]
    if len(a) == 3 and a[0] == "tone":
        e = check_tone(a[1], int(a[2]))
    elif len(a) == 4 and a[0] == "unplug":
        e = check_unplug(a[1], a[2], a[3])
    else:
        sys.exit(__doc__)
    if e:
        sys.exit("RED: " + e)


main()
