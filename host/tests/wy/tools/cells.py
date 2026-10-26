#!/usr/bin/env python3
"""WY-0c/WY-5: compares the glyphs of two tabs in a bitscomp screenshot (P6 PPM), so a test can tell that 'ü' is
drawn as its own glyph and not as 'u' or a fallback. Exo 2 is proportional: `ctl hot` gives each tab's text origin
and every glyph's box (x:width, from the origin); glyph I of tab 1 is compared with glyph I of tab 2.

usage: cells.py SHOT X1,Y1 GLYPHS1 X2,Y2 GLYPHS2 0xH SPEC...   SPEC = I=same | I=differ | I=ink
Prints one line per SPEC; exits 1 if any does not hold.
"""
import sys


def read_ppm(path):
    with open(path, "rb") as f:
        data = f.read()
    parts = data.split(maxsplit=4)
    assert parts[0] == b"P6"
    w, h = int(parts[1]), int(parts[2])
    return w, h, parts[4]


def glyph(img, x0, y0, box, ch, inner=False):
    w, _, px = img
    gx, gw = box
    if inner:
        # Neighbours may reach into a glyph's advance box (kerning, overhang): compare its middle only.
        gx, gw = gx + 2, max(1, gw - 4)
    return [px[(y * w + x0 + gx) * 3:(y * w + x0 + gx + gw) * 3] for y in range(y0, y0 + ch)]


def main():
    shot, a, ga, b, gb, size, *specs = sys.argv[1:]
    img = read_ppm(shot)
    (ax, ay), (bx, by) = (map(int, a.split(",")), map(int, b.split(",")))
    boxes = lambda g: [tuple(map(int, p.split(":"))) for p in g.split(",")]
    ba, bb = boxes(ga), boxes(gb)
    ch = int(size.split("x")[1])
    ok = True
    for spec in specs:
        i, want = spec.split("=")
        i = int(i)
        ca = glyph(img, ax, ay, ba[i], ch)
        if want == "ink":
            bg = ca[0][0:3]
            holds = any(r[k:k + 3] != bg for r in ca for k in range(0, len(r), 3))
        else:
            # Same width and pixels, compared at each tab's own glyph position.
            ca, cb = glyph(img, ax, ay, ba[i], ch, True), glyph(img, bx, by, bb[i], ch, True)
            holds = (ba[i][1] == bb[i][1] and ca == cb) == (want == "same")
        print(f"glyph {i}: {want} {'yes' if holds else 'NO'}")
        ok &= holds
    sys.exit(0 if ok else 1)


main()
