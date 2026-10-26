#!/usr/bin/env python3
"""WY-5 scenario client (80-theme.sh): the theme from V2's settingsd, applied live by bitscomp, over MCP.

Usage: v2_theme.py <mcp host port> <token> <qmp socket> <run dir> <reference dir>
Exit 0 = green."""
import os
import re
import sys
import time

sys.dont_write_bytecode = True  # no __pycache__ in the tree
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
REF = sys.argv[5]
sys.argv = sys.argv[:5] + ["-"]
import v2_check as v  # noqa: E402
from v2_check import Job, Qmp, call, connect, ctl, differing, need, read_ppm, screen  # noqa: E402


def setting(c, tool, args):
    err, text, _ = call(c, tool, args)
    need(not err, f"{tool} {args}", text)


def wait_theme(c, pattern, what, secs):
    t0 = time.time()
    while not re.search(pattern, ctl(c, "theme")):
        if time.time() - t0 > secs:
            raise SystemExit(f"RED: {what}\n{ctl(c, 'theme')}")
        time.sleep(0.05)
    ms = int((time.time() - t0) * 1000)
    print(f"ok: {what} after {ms} ms", flush=True)
    return ms


def compare(c, q, n, ref, box, what):
    rw, rh, rpx = read_ppm(os.path.join(REF, ref))
    time.sleep(0.4)
    w, h, px = screen(c, what)
    need((w, h) == (rw, rh), f"{what}: size {w}x{h} == reference")
    d = differing(px, rpx, w, box)
    need(d < w * h // 200, f"{what}: MCP screen == host reference {ref} ({d} pixels differ)")
    dw, dh, dpx = q.screendump(n)
    need((dw, dh) == (w, h) and differing(px, dpx, w) < w * h // 200, f"{what}: MCP screen == QEMU screendump")
    return px


def main():
    c = connect(300)
    q = Qmp(v.QMP)
    px_x, px_y = map(int, open(os.path.join(REF, "pointer.txt")).read().split())
    # WY-11: Adwaita's default pointer at 48 px (hotspot 6,2) drawn in software.
    box = (px_x - 8, px_y - 4, px_x + 44, px_y + 48)
    end = time.time() + 60
    while "settings_set" not in str(v.rpc(c, "tools/list")):
        if time.time() > end:
            raise SystemExit("RED: no settings_set tool (settingsd not running on V2?)")
        time.sleep(1)
    print("ok: settingsd's MCP tools are there", flush=True)
    comp = Job(c, "/bin/bitscomp", ["--socket", v.SOCK, "--fixed-status"])
    comp.wait_for(r"backend=fbdev size=1280x800", "bitscomp started on fbdev at 1280x800")
    wait_theme(c, r"settingsd=true", "bitscomp reads V2's settingsd", 20)
    need("mode=dark accent=#00e5ff" in ctl(c, "theme"), "the default theme is the mock-up's", ctl(c, "theme"))
    for n, title in enumerate(["one", "two"]):
        Job(c, "/bin/wyclient", ["--title", title, "--pattern", str(n)])
        end = time.time() + 20
        while f'title="{title}"' not in ctl(c, "list"):
            if time.time() > end:
                raise SystemExit(f"RED: wyclient {title} did not map\n{ctl(c, 'list')}")
            time.sleep(0.3)
    err, text, _ = call(c, "send_mouse", {"x": px_x, "y": px_y})
    need(not err, f"send_mouse to {px_x},{px_y}", text)
    dark = compare(c, q, 1, "ref.ppm", box, "dark")

    setting(c, "settings_set", {"key": "theme.mode", "value": "light"})
    ms = wait_theme(c, r"mode=light", "theme.mode light reached bitscomp", 5)
    need(ms <= 1000, f"light within 1 s ({ms} ms)")
    compare(c, q, 2, "light.ppm", box, "light")

    setting(c, "settings_set", {"key": "theme.accent", "value": "#ffb020"})
    ms = wait_theme(c, r"accent=#ffb020", "theme.accent #ffb020 reached bitscomp", 5)
    need(ms <= 1000, f"accent within 1 s ({ms} ms)")
    compare(c, q, 3, "accent.ppm", box, "accent")

    setting(c, "settings_undo", {"n": 2})
    wait_theme(c, r"mode=dark accent=#00e5ff", "settings_undo restored dark and cyan", 5)
    back = compare(c, q, 4, "ref.ppm", box, "undone")
    need(differing(back, dark, 1280, box, tol=0) == 0, "undo gives the first picture back exactly")
    out = ctl(c, "quit")
    print(f"ok: bitscomp ctl quit ({out.strip()[:40]})", flush=True)


main()
