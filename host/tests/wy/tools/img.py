#!/usr/bin/env python3
"""WY-5: screenshots for the look tests, without libraries. P6 PPM (what `bitscomp ctl shot` writes) and RGB PNG
(the references under host/tests/wy/ref/, filter 0 written, filters 0-4 read).

  img.py png SHOT.ppm OUT.png               convert
  img.py diff A B [TOL] [MAXSHARE]          A, B: .ppm or .png; a pixel differs when a channel differs by more
                                            than TOL (default 24); exit 1 when more than MAXSHARE (default 0.003)
                                            of the pixels differ; prints the share and the box of the differences
  img.py pixel FILE X Y                     prints "R G B"
"""
import struct
import sys
import zlib


def read_ppm(path):
    with open(path, "rb") as f:
        data = f.read()
    parts = data.split(maxsplit=4)
    if parts[0] != b"P6":
        raise SystemExit(f"{path}: not a P6 PPM")
    w, h = int(parts[1]), int(parts[2])
    return w, h, parts[4][: w * h * 3]


def write_png(path, w, h, rgb):
    raw = b"".join(b"\x00" + rgb[y * w * 3:(y + 1) * w * 3] for y in range(h))

    def chunk(tag, body):
        c = struct.pack(">I", len(body)) + tag + body
        return c + struct.pack(">I", zlib.crc32(tag + body) & 0xFFFFFFFF)

    png = b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 2, 0, 0, 0))
    png += chunk(b"IDAT", zlib.compress(raw, 9)) + chunk(b"IEND", b"")
    with open(path, "wb") as f:
        f.write(png)


def read_png(path):
    with open(path, "rb") as f:
        data = f.read()
    if data[:8] != b"\x89PNG\r\n\x1a\n":
        raise SystemExit(f"{path}: not a PNG")
    i, idat, w, h = 8, b"", 0, 0
    while i < len(data):
        n = struct.unpack(">I", data[i:i + 4])[0]
        tag, body = data[i + 4:i + 8], data[i + 8:i + 8 + n]
        if tag == b"IHDR":
            w, h, depth, ctype = struct.unpack(">IIBB", body[:10])
            if (depth, ctype) != (8, 2):
                raise SystemExit(f"{path}: only 8-bit RGB PNGs")
        elif tag == b"IDAT":
            idat += body
        i += 12 + n
    raw, out, stride, prev = zlib.decompress(idat), bytearray(), w * 3, bytearray(w * 3)
    for y in range(h):
        f, line = raw[y * (stride + 1)], bytearray(raw[y * (stride + 1) + 1:(y + 1) * (stride + 1)])
        for x in range(stride):
            a = line[x - 3] if x >= 3 else 0
            b, c = prev[x], prev[x - 3] if x >= 3 else 0
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
        out += line
        prev = line
    return w, h, bytes(out)


def load(path):
    return read_png(path) if path.endswith(".png") else read_ppm(path)


def main():
    cmd = sys.argv[1]
    if cmd == "png":
        w, h, rgb = read_ppm(sys.argv[2])
        write_png(sys.argv[3], w, h, rgb)
    elif cmd == "pixel":
        w, h, rgb = load(sys.argv[2])
        x, y = int(sys.argv[3]), int(sys.argv[4])
        print(*rgb[(y * w + x) * 3:(y * w + x) * 3 + 3])
    elif cmd == "diff":
        (wa, ha, a), (wb, hb, b) = load(sys.argv[2]), load(sys.argv[3])
        tol = int(sys.argv[4]) if len(sys.argv) > 4 else 24
        share = float(sys.argv[5]) if len(sys.argv) > 5 else 0.003
        if (wa, ha) != (wb, hb):
            print(f"sizes differ: {wa}x{ha} vs {wb}x{hb}")
            sys.exit(1)
        n, box = 0, [wa, ha, -1, -1]
        for i in range(0, len(a), 3):
            if max(abs(a[i] - b[i]), abs(a[i + 1] - b[i + 1]), abs(a[i + 2] - b[i + 2])) > tol:
                n += 1
                p = i // 3
                x, y = p % wa, p // wa
                box = [min(box[0], x), min(box[1], y), max(box[2], x), max(box[3], y)]
        s = n / (wa * ha)
        print(f"differ {n} of {wa * ha} pixels ({s:.5f})" + (f" in {box[0]},{box[1]}..{box[2]},{box[3]}" if n else ""))
        sys.exit(0 if s <= share else 1)
    else:
        raise SystemExit(__doc__)


main()
