#!/usr/bin/env python3
"""DS-2 on the host: the HUD kit at the cheap effect level, drawn by headless bitscomp (built-in defaults, fixed
status values, two wyclient windows).

Usage: ds_hud.py <bitscomp> <wyclient> <dir> <WxH>
Writes <dir>/{dark,gpu,light,ring-a,ring-b}.ppm and prints `ok:` lines and `cost:` lines; exit 0 = green.
Checks: theme.effects=gpu draws exactly the cheap picture; the wallpaper (dots) lies under the tiles; a busy ring
moves (only the ring's pixels change), at most 10 steps a second; waiting and idle rings and a still desktop draw
no frame (idle checked over 10 s)."""
import os
import re
import subprocess
import sys
import time

sys.dont_write_bytecode = True
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from ds_img import differing, read_ppm  # noqa: E402

BIN, CLIENT, DIR, SIZE = sys.argv[1:5]
SOCK = "wayland-dshud"
env = dict(os.environ, XDG_RUNTIME_DIR=DIR, WAYLAND_DISPLAY=SOCK)
env.pop("DISPLAY", None)


def need(cond, what, detail=""):
    if not cond:
        raise SystemExit(f"RED: {what}\n{detail}")
    print(f"ok: {what}", flush=True)


def ctl(*words):
    return subprocess.run([BIN, "ctl", "--socket", SOCK, *words], env=env, capture_output=True, text=True, timeout=20).stdout


def wait_for(pred, what):
    for _ in range(100):
        if pred():
            return
        time.sleep(0.1)
    raise SystemExit(f"RED: {what}\n{ctl('list')}")


def shot(name):
    path = os.path.join(DIR, f"{name}.ppm")
    ctl("shot", path)
    return read_ppm(path)


def stats():
    s = ctl("stats")
    m = re.search(r"frames=(\d+)", s)
    need(m is not None, "ctl stats answers", s)
    return int(m.group(1)), s


def field(s, k):
    m = re.search(rf"{k}=([0-9.]+)", s)
    return m.group(1) if m else "?"


def still(secs, what):
    time.sleep(0.5)
    f0, _ = stats()
    time.sleep(secs)
    f1, _ = stats()
    need(f1 == f0, f"{what}: 0 frames in {secs} s (frames={f0}, then {f1})")


procs = []
comp = subprocess.Popen([BIN, "--backend", "headless", "--size", SIZE, "--socket", SOCK, "--fixed-status",
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
    ctl("pointer", "4", "4")
    w, h, dark = shot("dark")

    # The wallpaper under the tiles: in the gap below the tiles' bottom the dots of the grid show (lighter pixels
    # every 32 px), which a flat ground never has.
    m = re.search(r"work=(-?\d+),(-?\d+),(\d+),(\d+)", ctl("hot"))
    need(m is not None, "the work area is listed", ctl("hot"))
    wx, wy, ww, wh = (int(v) for v in m.groups())
    lum = lambda x, y: sum(dark[(y * w + x) * 3:(y * w + x) * 3 + 3])
    row = round((wy + wh) * 1.25) - 4
    vals = [lum(x, row) for x in range(round(wx * 1.25), round((wx + ww) * 1.25))]
    if os.environ.get("DS_HUD_WALLPAPER", "1") == "1":
        need(len(set(vals)) > 3, f"the ground under the tiles is the wallpaper, not one flat colour ({len(set(vals))} values)")

    ctl("set", "theme.effects", "gpu")
    time.sleep(0.4)
    _, _, gpu = shot("gpu")
    need(differing(dark, gpu, w, None, tol=0) == 0, "theme.effects=gpu draws exactly the cheap picture")
    ctl("set", "theme.effects", "cheap")

    # Cost of a full redraw on the host (theme change: every cached image and the wallpaper drawn again).
    ctl("stats", "reset")
    ctl("set", "theme.mode", "light")
    time.sleep(1.0)
    _, s = stats()
    print(f"cost: host full redraw (theme change) {SIZE}: frames={field(s, 'frames')} render_avg_ms={field(s, 'render_avg_ms')} "
          f"present_avg_ms={field(s, 'present_avg_ms')} damaged_mpx_avg={field(s, 'damaged_mpx_avg')}", flush=True)
    shot("light")
    ctl("set", "theme.mode", "dark")
    time.sleep(0.6)

    # Cost of the workspace switch to the start page (its glass cards, shadows and brackets drawn) and back.
    hot = [l for l in ctl("hot").splitlines() if "wsnew" in l and " at=" in l]
    need(bool(hot), "the new-workspace button is listed")
    ctl("stats", "reset")
    ctl("click", *hot[0].split(" at=")[1].split()[0].split(","))
    wait_for(lambda: "\nstart new at=" in ctl("hot"), "no start page on the empty workspace")
    time.sleep(0.8)
    _, s = stats()
    print(f"cost: host switch to the start page: frames={field(s, 'frames')} render_avg_ms={field(s, 'render_avg_ms')} "
          f"damaged_mpx_avg={field(s, 'damaged_mpx_avg')}", flush=True)
    ctl("stats", "reset")
    ctl("ws", "remove")
    time.sleep(0.8)
    _, s = stats()
    print(f"cost: host back to the tiles (cached): frames={field(s, 'frames')} render_avg_ms={field(s, 'render_avg_ms')} "
          f"damaged_mpx_avg={field(s, 'damaged_mpx_avg')}", flush=True)

    # The busy ring: it moves, only its own pixels change, at most 10 steps a second.
    print(ctl("ring", "working").strip(), flush=True)
    time.sleep(0.5)
    _, _, a = shot("ring-a")
    time.sleep(0.35)
    _, _, b = shot("ring-b")
    d = differing(a, b, w, None, tol=0)
    need(0 < d < w * h // 500, f"the working ring moves and nothing else changes ({d} pixels)")
    ctl("stats", "reset")
    f0, _ = stats()
    time.sleep(3.0)
    f1, s = stats()
    need(12 <= f1 - f0 <= 33, f"working ring: {f1 - f0} frames in 3 s (10 steps a second at most)")
    print(f"cost: host ring step: render_avg_ms={field(s, 'render_avg_ms')} damaged_mpx_avg={field(s, 'damaged_mpx_avg')}", flush=True)
    ctl("ring", "waiting")
    still(2.0, "waiting ring (warm, still)")
    ctl("ring", "idle")
    still(10.0, "idle ring, still desktop")
    ctl("quit")
finally:
    for p in reversed(procs):
        p.kill()
        p.wait()
