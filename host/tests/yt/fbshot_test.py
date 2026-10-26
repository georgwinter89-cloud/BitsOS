# Host test of scripts/fbshot.py: a known fake framebuffer -> PNG with the
# right size and pixels (full, --half, and a padded --pitch).
# usage: fbshot_test.py <scripts-dir> <temp-dir>
import os, struct, subprocess, sys, zlib

scripts, tmp = sys.argv[1:3]
fbshot = os.path.join(scripts, "fbshot.py")


def read_png(path):
    data = open(path, "rb").read()
    assert data[:8] == b"\x89PNG\r\n\x1a\n", "PNG signature"
    pos, idat = 8, b""
    w = h = None
    while pos < len(data):
        n, t = struct.unpack(">I4s", data[pos:pos + 8])
        body = data[pos + 8:pos + 8 + n]
        crc = struct.unpack(">I", data[pos + 8 + n:pos + 12 + n])[0]
        assert crc == zlib.crc32(t + body) & 0xFFFFFFFF, "chunk CRC"
        if t == b"IHDR":
            w, h, depth, ctype = struct.unpack(">IIBB", body[:10])
            assert (depth, ctype) == (8, 2), "8-bit RGB"
        elif t == b"IDAT":
            idat += body
        pos += 12 + n
    raw = zlib.decompress(idat)
    rows = []
    for y in range(h):
        line = raw[y * (1 + w * 3):(y + 1) * (1 + w * 3)]
        assert line[0] == 0, "filter type 0"
        rows.append(line[1:])
    return w, h, lambda x, y: tuple(rows[y][x * 3:x * 3 + 3])


def make_fb(path, w, h, pitch):
    buf = bytearray(b"\xEE" * (pitch * h))  # padding stays 0xEE
    for y in range(h):
        for x in range(w):
            # BGRX: red = x*10, green = y*20, blue = 7
            buf[y * pitch + x * 4:y * pitch + x * 4 + 4] = bytes([7, y * 20, x * 10, 0])
    open(path, "wb").write(buf)


def shot(*args):
    out = os.path.join(tmp, "shot.png")
    subprocess.run([sys.executable, "-I", fbshot, *args[:2], out, *args[2:]],
                   check=True, stdout=subprocess.DEVNULL)
    return read_png(out)


fb = os.path.join(tmp, "fb-shot")
make_fb(fb, 7, 5, 28)
w, h, px = shot(fb, "7x5")
assert (w, h) == (7, 5), (w, h)
assert px(0, 0) == (0, 0, 7) and px(6, 4) == (60, 80, 7), (px(0, 0), px(6, 4))
w, h, px = shot(fb, "7x5", "--half")
assert (w, h) == (4, 3) and px(3, 2) == (60, 80, 7), (w, h, px(3, 2))
make_fb(fb, 7, 5, 40)
w, h, px = shot(fb, "7x5", "--pitch", "40")
assert (w, h) == (7, 5) and px(6, 4) == (60, 80, 7) and px(3, 1) == (30, 20, 7)
# Too small a file is refused, not read past.
open(fb, "wb").write(b"\0" * 10)
r = subprocess.run([sys.executable, "-I", fbshot, fb, "7x5", os.path.join(tmp, "x.png")],
                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
assert r.returncode != 0
print("ok fbshot")
