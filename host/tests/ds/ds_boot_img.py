#!/usr/bin/env python3
"""DS-5: the boot scene's pictures compared (the kernel's boot screen and bitscomp's continuation, both 2560x1440).

Regions in screen pixels (crates/console/src/bootdraw.rs, user/services/bitscomp/src/chrome_render/boot_draw.rs):
the ring's sprite, the title, the step rows, the status line, the progress count, the header label, the hint."""
import os
import sys

sys.dont_write_bytecode = True
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from ds_img import read_ppm  # noqa: E402,F401


def px(v):
    return (v * 4 + 1) // 3


# (x0, y0, x1, y1), end exclusive.
RING = (1280 - 347, px(380) - 347, 1280 + 347, px(380) + 347)
TITLE = (900, px(664), 1660, px(664) + px(84))
ROWS = (px(96), px(820), px(96) + px(520), px(820 + 24 * 10))
DESKTOP_ROW = (px(96), px(820 + 24 * 9), px(96) + px(520), px(820 + 24 * 10))
STATUS = (px(96), px(758), 2560 - px(96), px(758) + px(28))
PROGRESS = (px(1364) - 8, px(836), px(1824) + 8, px(860) + px(6) + 8)
HEADER = (px(1200), px(36), px(1872), px(36) + px(22))
HINT = (1800, px(1020) - 4, px(1824) + 4, px(1020) + px(14) + 4)
# bitscomp's pointer at its start position (0, 0), drawn over the scene on fbdev (CR DS-5-2 hides it).
POINTER = (0, 0, 64, 64)
CORE = (1280, px(380))


def inside(r, x, y):
    return r[0] <= x < r[2] and r[1] <= y < r[3]


def diff(a, b, allowed=(), tol=0):
    """(pixels differing outside `allowed`, inside the ring, inside the title, all differing pixels, first outside)."""
    (w, h, pa), (w2, h2, pb) = a, b
    if (w, h) != (w2, h2):
        raise SystemExit(f"RED: sizes differ: {w}x{h} and {w2}x{h2}")
    out = ring = title = total = 0
    first = None
    for y in range(h):
        o = y * w * 3
        ra, rb = pa[o:o + w * 3], pb[o:o + w * 3]
        if ra == rb:
            continue
        for x in range(w):
            i = x * 3
            if ra[i:i + 3] == rb[i:i + 3]:
                continue
            if tol and max(abs(ra[i + k] - rb[i + k]) for k in range(3)) <= tol:
                continue
            total += 1
            ring += inside(RING, x, y)
            title += inside(TITLE, x, y)
            if not any(inside(r, x, y) for r in allowed):
                out += 1
                if first is None:
                    first = (x, y, ra[i:i + 3].hex(), rb[i:i + 3].hex())
    return out, ring, title, total, first


def pixel(img, x, y):
    w, _, p = img
    o = (y * w + x) * 3
    return p[o] << 16 | p[o + 1] << 8 | p[o + 2]


def is_core(p):
    """The ring's accent core dot (green, bright)."""
    return p is not None and (p >> 8) & 0xFF > 0xB0 and (p >> 16) & 0xFF < 0xA0


def region(img, r):
    w, _, p = img
    return b"".join(p[(y * w + r[0]) * 3:(y * w + r[2]) * 3] for y in range(r[1], r[3]))
