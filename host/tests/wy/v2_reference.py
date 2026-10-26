#!/usr/bin/env python3
"""WY-1a: the reference picture for 40-v2.sh, drawn on the host by headless bitscomp (1280x800, scale 1.25)
with the same two wyclient windows mapped in the same order and the pointer moved to the same spot.

Usage: v2_reference.py <bitscomp> <wyclient> <dir> [themes]
Writes <dir>/ref.ppm and <dir>/pointer.txt ("X Y" physical pixels, the centre of the first window's body),
prints the ctl list. WY-5: the status bar shows fixed values (--fixed-status, as on V2 in the scenarios), the theme
is the built-in default (= settingsd's defaults); with `themes` also <dir>/light.ppm (theme.mode light) and
<dir>/accent.ppm (light with accent #ffb020) for 80-theme.sh. Exit 0 = done."""
import os
import subprocess
import sys
import time

BIN, CLIENT, DIR = sys.argv[1], sys.argv[2], sys.argv[3]
SOCK = "wayland-wyref"
env = dict(os.environ, XDG_RUNTIME_DIR=DIR, WAYLAND_DISPLAY=SOCK)
env.pop("DISPLAY", None)


def ctl(*words):
    return subprocess.run([BIN, "ctl", "--socket", SOCK, *words], env=env, capture_output=True, text=True, timeout=10).stdout


def wait_for(pred, what):
    for _ in range(100):
        if pred():
            return
        time.sleep(0.1)
    raise SystemExit(f"RED: reference: {what}\n{ctl('list')}")


def pointer_spot(listing):
    """The middle of the first tile's frame, 10 points below centre (inside the client), in physical pixels."""
    for line in listing.splitlines():
        if line.startswith("tile=0 "):
            xy, wh = line.split("frame=")[1].split()[:2]
            x, y = map(int, xy.split(","))
            w, h = map(int, wh.split("x"))
            return round((x + w / 2) * 1.25), round((y + h / 2 + 10) * 1.25)
    raise SystemExit(f"RED: no tile 0 in\n{listing}")


procs = []
comp = subprocess.Popen([BIN, "--backend", "headless", "--size", "1280x800", "--socket", SOCK, "--fixed-status",
                         "--settings", "none", "--mcp", "none"], env=env,
                        stdout=subprocess.DEVNULL, stderr=open(os.path.join(DIR, "bitscomp.err"), "w"))
procs.append(comp)
try:
    wait_for(lambda: os.path.exists(os.path.join(DIR, SOCK + ".ctl")), "no control socket")
    for n, title in enumerate(["one", "two"]):
        procs.append(subprocess.Popen([CLIENT, "--title", title, "--pattern", str(n)], env=env,
                                      stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL))
        wait_for(lambda: f'title="{title}"' in ctl("list"), f"wyclient {title} did not map")
    time.sleep(0.5)
    listing = ctl("list")
    print(listing.strip())
    x, y = pointer_spot(listing)
    ctl("pointer", str(x), str(y))
    time.sleep(0.3)
    ctl("shot", os.path.join(DIR, "ref.ppm"))
    with open(os.path.join(DIR, "pointer.txt"), "w") as f:
        f.write(f"{x} {y}\n")
    print(f"reference: {DIR}/ref.ppm, pointer {x} {y}")
    if sys.argv[4:] == ["themes"]:
        ctl("set", "theme.mode", "light")
        time.sleep(0.3)
        ctl("shot", os.path.join(DIR, "light.ppm"))
        ctl("set", "theme.accent", "#ffb020")
        time.sleep(0.3)
        ctl("shot", os.path.join(DIR, "accent.ppm"))
        print(f"reference: {DIR}/light.ppm, {DIR}/accent.ppm")
    ctl("quit")
finally:
    for p in reversed(procs):
        p.kill()
        p.wait()
