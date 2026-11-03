#!/usr/bin/env python3
"""DS-1: pictures for the look tests, without libraries. P6 PPM (what `bitscomp ctl shot` and QMP screendump write)
and PNG (the references under host/tests/ds/ref/ and MCP `screen`).

  ds_img.py png SHOT.ppm OUT.png            convert
  ds_img.py diff A B [TOL] [MAXSHARE]       a pixel differs when a channel differs by more than TOL (default 8);
                                            exit 1 when more than MAXSHARE (default 0.0002) of the pixels differ
"""
import struct
import sys
import zlib


def read_ppm(path):
    with open(path, "rb") as f:
        data = f.read()
    parts, at = [], 0
    while len(parts) < 4:
        while data[at:at + 1].isspace():
            at += 1
        start = at
        while not data[at:at + 1].isspace():
            at += 1
        parts.append(data[start:at])
    if parts[0] != b"P6":
        raise SystemExit(f"{path}: not a P6 PPM")
    w, h = int(parts[1]), int(parts[2])
    return w, h, data[at + 1:at + 1 + w * h * 3]


def write_png(path, w, h, rgb):
    raw = b"".join(b"\x00" + rgb[y * w * 3:(y + 1) * w * 3] for y in range(h))

    def chunk(tag, body):
        c = struct.pack(">I", len(body)) + tag + body
        return c + struct.pack(">I", zlib.crc32(tag + body) & 0xFFFFFFFF)

    png = b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 2, 0, 0, 0))
    png += chunk(b"IDAT", zlib.compress(raw, 9)) + chunk(b"IEND", b"")
    with open(path, "wb") as f:
        f.write(png)


def decode_png(data):
    """8-bit RGB or RGBA, any filter; returns (w, h, rgb)."""
    if data[:8] != b"\x89PNG\r\n\x1a\n":
        raise SystemExit("not a PNG")
    i, idat, w, h, bpp = 8, b"", 0, 0, 3
    while i < len(data):
        n = struct.unpack(">I", data[i:i + 4])[0]
        tag, body = data[i + 4:i + 8], data[i + 8:i + 8 + n]
        if tag == b"IHDR":
            w, h, depth, ctype = struct.unpack(">IIBB", body[:10])
            if depth != 8 or ctype not in (2, 6):
                raise SystemExit(f"PNG depth {depth} colour type {ctype}")
            bpp = 3 if ctype == 2 else 4
        elif tag == b"IDAT":
            idat += body
        i += 12 + n
    raw, stride = zlib.decompress(idat), w * bpp
    out, prev = bytearray(), bytearray(stride)
    for y in range(h):
        f, line = raw[y * (stride + 1)], bytearray(raw[y * (stride + 1) + 1:(y + 1) * (stride + 1)])
        for x in range(stride):
            a = line[x - bpp] if x >= bpp else 0
            b, c = prev[x], prev[x - bpp] if x >= bpp else 0
            if f == 1:
                line[x] = (line[x] + a) & 255
            elif f == 2:
                line[x] = (line[x] + b) & 255
            elif f == 3:
                line[x] = (line[x] + (a + b) // 2) & 255
            elif f == 4:
                p = a + b - c
                pa, pb, pc = abs(p - a), abs(p - b), abs(p - c)
                line[x] = (line[x] + (a if pa <= pb and pa <= pc else b if pb <= pc else c)) & 255
        out += line if bpp == 3 else bytes(v for k, v in enumerate(line) if k % 4 != 3)
        prev = line
    return w, h, bytes(out)


def load(path):
    if path.endswith(".png"):
        with open(path, "rb") as f:
            return decode_png(f.read())
    return read_ppm(path)


def differing(a, b, w, skip=None, tol=24):
    """Pixels whose channels differ by more than `tol`, outside the box `skip` (x0, y0, x1, y1)."""
    n = 0
    for i in range(0, min(len(a), len(b)), 3):
        if skip:
            x, y = (i // 3) % w, (i // 3) // w
            if skip[0] <= x < skip[2] and skip[1] <= y < skip[3]:
                continue
        if max(abs(a[i] - b[i]), abs(a[i + 1] - b[i + 1]), abs(a[i + 2] - b[i + 2])) > tol:
            n += 1
    return n


def main():
    cmd = sys.argv[1]
    if cmd == "png":
        write_png(sys.argv[3], *read_ppm(sys.argv[2]))
    elif cmd == "diff":
        (wa, ha, a), (wb, hb, b) = load(sys.argv[2]), load(sys.argv[3])
        tol = int(sys.argv[4]) if len(sys.argv) > 4 else 8
        share = float(sys.argv[5]) if len(sys.argv) > 5 else 0.0002
        if (wa, ha) != (wb, hb):
            print(f"sizes differ: {wa}x{ha} vs {wb}x{hb}")
            sys.exit(1)
        n = differing(a, b, wa, tol=tol)
        print(f"differ {n} of {wa * ha} pixels ({n / (wa * ha):.5f})")
        sys.exit(0 if n / (wa * ha) <= share else 1)
    else:
        raise SystemExit(__doc__)


if __name__ == "__main__":
    main()
