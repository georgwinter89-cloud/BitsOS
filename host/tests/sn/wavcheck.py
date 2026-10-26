#!/usr/bin/env python3
"""SN-2: sample-exact check of a WAV that QEMU's wav audiodev wrote while `sn.hda.play` ran (the idea of V1's
BitsOS-ASM/hda-test.mjs, in Python). The kernel plays `crates/drivers/hda/src/signal.rs`: 16-bit stereo, left
round(12000*sin(2*pi*i/48)), right the frame counter mod 2^16 (as int16). Checks:
  - the header rate is the one the audiodev was given (--rate), 2 channels, 16 bit;
  - before the signal only silence, then exactly --frames frames equal to the signal (no gap, no repeat, no
    swap, no resampling: a wrong stream rate makes QEMU resample, which no sample survives), then only silence.
Usage: wavcheck.py FILE --rate 48000 --frames 96000
"""
import argparse
import math
import struct
import sys


def signal(i):
    left = round(12000 * math.sin(2 * math.pi * (i % 48) / 48))
    right = (i & 0xFFFF) - 0x10000 if (i & 0xFFFF) >= 0x8000 else (i & 0xFFFF)
    return left, right


def read_wav(path):
    data = open(path, "rb").read()
    if data[0:4] != b"RIFF" or data[8:12] != b"WAVE":
        raise ValueError("not a RIFF/WAVE file")
    pos, fmt, pcm = 12, None, None
    while pos + 8 <= len(data):
        cid, size = data[pos:pos + 4], struct.unpack("<I", data[pos + 4:pos + 8])[0]
        body = data[pos + 8:]
        if cid == b"fmt ":
            fmt = struct.unpack("<HHIIHH", body[:16])
        elif cid == b"data":
            # QEMU fills the sizes in when it closes the file; an unfinished file has 0 there: take the rest.
            pcm = body if size == 0 or size > len(body) else body[:size]
            break
        pos += 8 + size + (size & 1)
    if fmt is None or pcm is None:
        raise ValueError("no fmt or data chunk")
    tag, channels, rate, _, _, bits = fmt
    if tag != 1 or channels != 2 or bits != 16:
        raise ValueError(f"want PCM 2x16 bit, got tag={tag} channels={channels} bits={bits}")
    n = len(pcm) // 4
    frames = struct.unpack(f"<{2 * n}h", pcm[:4 * n])
    return rate, [(frames[2 * i], frames[2 * i + 1]) for i in range(n)]


def check(path, want_rate, want_frames):
    rate, f = read_wav(path)
    print(f"{path}: rate={rate} frames={len(f)}")
    if rate != want_rate:
        return f"header rate {rate}, want {want_rate}"
    first = next((i for i, x in enumerate(f) if x != (0, 0)), None)
    if first is None:
        return "only silence"
    # The counter on the right says which signal frame this is (frame 0 is (0, 0) and looks like silence).
    j = f[first][1] & 0xFFFF
    start = first - j
    if j >= 48 or start < 0 or f[first] != signal(j):
        return f"first sound {f[first]} at frame {first} is no signal frame"
    for i in range(want_frames):
        k = start + i
        if k >= len(f):
            return f"file ends after {i} signal frames (want {want_frames})"
        if f[k] != signal(i):
            return f"frame {i} (file frame {k}) is {f[k]}, want {signal(i)}: gap, repeat or resampling"
    tail = f[start + want_frames:]
    bad = next((i for i, x in enumerate(tail) if x != (0, 0)), None)
    if bad is not None:
        return f"after the signal frame {start + want_frames + bad} is {tail[bad]}, want silence"
    print(f"ok: {want_frames} frames sample-exact from file frame {start}, {start} leading and {len(tail)} trailing silent frames")
    return None


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("wav")
    ap.add_argument("--rate", type=int, required=True)
    ap.add_argument("--frames", type=int, required=True)
    a = ap.parse_args()
    try:
        err = check(a.wav, a.rate, a.frames)
    except (OSError, ValueError, struct.error) as e:
        err = str(e)
    if err:
        print(f"FAIL {a.wav}: {err}")
        sys.exit(1)


if __name__ == "__main__":
    main()
