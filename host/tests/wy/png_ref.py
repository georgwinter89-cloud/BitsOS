#!/usr/bin/env python3
# PK-L1a: the reference for bitscomp's PNG decoder (icon_png.rs), made with Python's zlib only: for every *.png in the
# folder given, one line "name<TAB>width<TAB>height<TAB>fnv1a64-of-RGBA8" or "name<TAB>-<TAB>reason" for a file it
# does not read (Adam7, broken). RGBA8 as icon_png.rs defines it: 16-bit samples keep their high byte, grey below
# 8 bits scaled to 0-255 (v*255//max), tRNS as a palette alpha table or the one transparent grey/RGB colour.
import os, struct, sys, zlib


def fnv(data):
    h = 0xcbf29ce484222325
    for b in data:
        h = ((h ^ b) * 0x100000001b3) & 0xffffffffffffffff
    return h


def decode(buf):
    if buf[:8] != b'\x89PNG\r\n\x1a\n':
        raise ValueError('signature')
    pos, ihdr, plte, trns, idat = 8, None, b'', b'', b''
    while True:
        n, = struct.unpack('>I', buf[pos:pos + 4])
        kind, data = buf[pos + 4:pos + 8], buf[pos + 8:pos + 8 + n]
        crc, = struct.unpack('>I', buf[pos + 8 + n:pos + 12 + n])
        if zlib.crc32(kind + data) != crc:
            raise ValueError('crc')
        pos += 12 + n
        if kind == b'IHDR':
            ihdr = struct.unpack('>IIBBBBB', data)
        elif kind == b'PLTE':
            plte = data
        elif kind == b'tRNS':
            trns = data
        elif kind == b'IDAT':
            idat += data
        elif kind == b'IEND':
            break
    w, h, depth, colour, _, _, interlace = ihdr
    if interlace:
        raise ValueError('interlaced')
    chans = {0: 1, 2: 3, 3: 1, 4: 2, 6: 4}[colour]
    bits = chans * depth
    stride = (w * bits + 7) // 8
    step = max(1, bits // 8)
    raw = zlib.decompress(idat)
    rows, prev = [], bytearray(stride)
    for y in range(h):
        f = raw[y * (stride + 1)]
        row = bytearray(raw[y * (stride + 1) + 1:(y + 1) * (stride + 1)])
        for i in range(stride):
            a = row[i - step] if i >= step else 0
            b = prev[i]
            c = prev[i - step] if i >= step else 0
            if f == 1:
                row[i] = (row[i] + a) & 255
            elif f == 2:
                row[i] = (row[i] + b) & 255
            elif f == 3:
                row[i] = (row[i] + (a + b) // 2) & 255
            elif f == 4:
                p = a + b - c
                pa, pb, pc = abs(p - a), abs(p - b), abs(p - c)
                row[i] = (row[i] + (a if pa <= pb and pa <= pc else b if pb <= pc else c)) & 255
        rows.append(row)
        prev = row
    out = bytearray()
    mx = (1 << depth) - 1
    key = [struct.unpack('>H', trns[i:i + 2])[0] for i in range(0, len(trns) - 1, 2)] if colour in (0, 2) else []
    for row in rows:
        if depth == 16:
            s = [struct.unpack('>H', row[i:i + 2])[0] for i in range(0, len(row), 2)]
        elif depth == 8:
            s = list(row)
        else:
            s = []
            for byte in row:
                for k in range(8 // depth):
                    s.append((byte >> (8 - depth * (k + 1))) & mx)
        e8 = (lambda v: v >> 8) if depth == 16 else (lambda v: v) if depth == 8 else (lambda v: v * 255 // mx)
        for x in range(w):
            if colour == 0:
                g = s[x]
                out += bytes([e8(g)] * 3 + [0 if key[:1] == [g] else 255])
            elif colour == 2:
                r, g, b = s[3 * x:3 * x + 3]
                out += bytes([e8(r), e8(g), e8(b), 0 if key[:3] == [r, g, b] else 255])
            elif colour == 3:
                i = s[x]
                c = plte[3 * i:3 * i + 3] if 3 * i + 3 <= len(plte) else b'\0\0\0'
                out += bytes(c) + bytes([trns[i] if i < len(trns) else 255])
            elif colour == 4:
                g = e8(s[2 * x])
                out += bytes([g, g, g, e8(s[2 * x + 1])])
            else:
                out += bytes(e8(v) for v in s[4 * x:4 * x + 4])
    return w, h, out


for name in sorted(os.listdir(sys.argv[1])):
    if not name.endswith('.png'):
        continue
    try:
        with open(os.path.join(sys.argv[1], name), 'rb') as f:
            w, h, px = decode(f.read())
        print(f'{name}\t{w}\t{h}\t{fnv(px)}')
    except Exception as e:  # noqa: BLE001 - every unreadable file is just not counted
        print(f'{name}\t-\t{type(e).__name__}: {e}')
