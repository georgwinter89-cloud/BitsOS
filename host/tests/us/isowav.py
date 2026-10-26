#!/usr/bin/env python3
"""SN-8a: checks the WAV that QEMU's usb-audio (wav audiodev, 48 kHz s16 stereo) wrote while `us.isoch.stream`
streamed its pattern over isochronous OUT: audio frame i = (left (i mod 48) * 512 - 12288, right i mod 2^16)
(`crates/drivers/xhci/src/ktests_isoch.rs`). QEMU's usb-audio plays at its default volume (240 of 255 in its
mixer), so every sample arrives scaled by 240/255 (+-2 for rounding). Walks the file frame by frame against the
pattern; where frames are missing (QEMU's stream buffer full or empty: timing in QEMU is not judged) it finds the
pattern again within the next 4800 frames and counts a gap. A data size of 0 in the header (QEMU killed before
it finished the file) means: everything after the header.
Checks: 48000 Hz, 2 channels, 16 bit; at least --min-frames pattern frames, in order; no frame that is neither
pattern nor silence (no garbage, no swapped channels, no repeated data).
Usage: isowav.py FILE --min-frames N
"""
import argparse
import struct
import sys

SCALE = 240 / 255
TOL = 2


def pattern(i):
    return ((i % 48) * 512 - 12288) * SCALE, ((i & 0xFFFF) - 0x10000 if (i & 0xFFFF) >= 0x8000 else (i & 0xFFFF)) * SCALE


def near(frame, i):
    l, r = pattern(i)
    return abs(frame[0] - l) <= TOL and abs(frame[1] - r) <= TOL + abs(r) * 0.002


def read(path):
    d = open(path, "rb").read()
    if d[0:4] != b"RIFF" or d[8:12] != b"WAVE":
        sys.exit("RED: not a RIFF/WAVE file")
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
        sys.exit("RED: no fmt or data chunk")
    n = len(data) // 4
    s = struct.unpack("<%dh" % (2 * n), data[:4 * n])
    return fmt, list(zip(s[0::2], s[1::2]))


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("file")
    ap.add_argument("--min-frames", type=int, required=True)
    a = ap.parse_args()
    fmt, frames = read(a.file)
    if fmt[1] != 2 or fmt[2] != 48000 or fmt[5] != 16:
        sys.exit("RED: format %s, want 2 channels 48000 Hz 16 bit" % (fmt,))
    k, j, ok, gaps, bad, silence = 0, 0, 0, 0, 0, 0
    while k < len(frames):
        f = frames[k]
        if near(f, j):
            ok, j, k = ok + 1, j + 1, k + 1
            continue
        if f == (0, 0):
            silence, k = silence + 1, k + 1
            continue
        # Frames lost before this one: the pattern goes on later (within 100 ms), checked over 8 frames.
        found = next((x for x in range(j + 1, j + 4800) if all(k + t < len(frames) and near(frames[k + t], x + t) for t in range(8))), None)
        if found is not None:
            gaps, j = gaps + 1, found
            continue
        bad += 1
        if bad <= 5:
            print("frame %d: %s, pattern frame %d would be %s" % (k, f, j, tuple(round(x) for x in pattern(j))))
        k += 1
    print("isowav: %d frames, %d pattern frames, %d gaps, %d silent, %d bad, last pattern frame %d" % (len(frames), ok, gaps, silence, bad, j))
    if bad:
        sys.exit("RED: %d frames are neither the pattern nor silence" % bad)
    if ok < a.min_frames:
        sys.exit("RED: %d pattern frames, want at least %d" % (ok, a.min_frames))


if __name__ == "__main__":
    main()
