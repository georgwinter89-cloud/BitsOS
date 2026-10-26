# usage: fbshot.py <fb-file> <WxH> <out.png> [--half] [--pitch BYTES]
# Turns the fake framebuffer (32-bit BGRX rows, pitch = W*4 unless given)
# into a PNG.
import struct, sys, zlib
args = sys.argv[1:]
half = "--half" in args
pitch = None
if "--pitch" in args:
    pitch = int(args[args.index("--pitch") + 1])
path, size, out = args[:3]
w, h = (int(v) for v in size.lower().split("x"))
pitch = pitch or w * 4
if w < 1 or h < 1 or pitch < w * 4:
    sys.exit(f"fbshot: odd size {w}x{h} pitch {pitch}")
data = open(path, "rb").read()
if len(data) < (h - 1) * pitch + w * 4:
    sys.exit(f"fbshot: {path} has {len(data)} bytes, {w}x{h} pitch {pitch} needs more")
step = 2 if half else 1
rows = []
for y in range(0, h, step):
    line = data[y * pitch:y * pitch + w * 4]
    b, g, r = line[0::4 * step], line[1::4 * step], line[2::4 * step]
    px = bytearray(len(b) * 3)
    px[0::3], px[1::3], px[2::3] = r, g, b
    rows.append(b"\x00" + bytes(px))
ow, oh = len(rows[0]) // 3, len(rows)
def chunk(t, d):
    return struct.pack(">I", len(d)) + t + d + struct.pack(">I", zlib.crc32(t + d) & 0xFFFFFFFF)
png = b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", ow, oh, 8, 2, 0, 0, 0))
png += chunk(b"IDAT", zlib.compress(b"".join(rows), 6)) + chunk(b"IEND", b"")
open(out, "wb").write(png)
print(f"fbshot: {out} {ow}x{oh}")
