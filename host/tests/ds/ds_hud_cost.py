#!/usr/bin/env python3
"""DS-2 scenario client (40-hud-cost.sh): the HUD kit's cost on V2 in QEMU at 2560x1440, over MCP. Records numbers
only (timing is never judged in QEMU); checks that a still desktop draws no frame in 10 s and that a busy ring asks
for at most 10 frames a second.

Usage: ds_hud_cost.py <mcp host port> <token> <qmp socket> <run dir>"""
import os
import re
import sys
import time

sys.dont_write_bytecode = True
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
sys.argv = sys.argv[:5] + ["-"]  # ds_theme's argument shape (no reference dir needed here)
import ds_theme as d  # noqa: E402  (its main() runs only as a script)


def stats(c):
    s = d.ctl(c, "stats")
    m = re.search(r"frames=(\d+)", s)
    d.need(m is not None, "ctl stats answers", s)
    return int(m.group(1)), s


def cost(c, what):
    _, s = stats(c)
    f = {k: (re.search(rf"\b{k}=([0-9.]+)", s) or [None, "?"])[1] for k in ("frames", "render_avg_ms", "present_avg_ms", "damaged_mpx_avg")}
    print(f"cost: {what}: " + " ".join(f"{k}={v}" for k, v in f.items()), flush=True)


def hot_at(c, pattern, field):
    for line in d.ctl(c, "hot").splitlines():
        if pattern in line and f" {field}=" in line:
            return line.split(f" {field}=")[1].split()[0].split(",")
    raise SystemExit(f"RED: no {field} on a hot line with {pattern}")


def main():
    c = d.connect(300)
    q = d.Qmp(d.QMP)
    comp = d.start(c, "/bin/bitscomp", ["--socket", d.SOCK, "--fixed-status"])
    d.wait_output(c, comp, r"backend=fbdev size=2560x1440", "bitscomp started on fbdev at 2560x1440")
    for n, title in enumerate(["one", "two"]):
        d.start(c, "/bin/wyclient", ["--title", title, "--pattern", str(n)])
        end = time.time() + 20
        while f'title="{title}"' not in d.ctl(c, "list"):
            if time.time() > end:
                raise SystemExit(f"RED: wyclient {title} did not map")
            time.sleep(0.3)
    time.sleep(2.0)
    # bitscomp's own line per wallpaper drawn (B6), with its cost.
    for line in d.call(c, "output", {"call": comp[0]})[1].splitlines():
        if "wallpaper" in line:
            print(f"cost: {line.strip()}", flush=True)

    d.ctl(c, "stats", "reset")
    d.ctl(c, "set", "theme.mode", "light")
    time.sleep(3.0)
    cost(c, "theme change (wallpaper and every image drawn again, full output)")
    d.ctl(c, "set", "theme.mode", "dark")
    time.sleep(3.0)

    d.ctl(c, "stats", "reset")
    d.ctl(c, "click", *hot_at(c, "wsnew", "at"))
    time.sleep(2.0)
    cost(c, "switch to a new workspace (start page drawn, full output composed)")
    d.ctl(c, "stats", "reset")
    d.ctl(c, "ws", "remove")
    time.sleep(2.0)
    cost(c, "back to the tiles (cached images, full output composed)")
    q.screendump(1)
    print(f"ok: screendump {os.path.join(d.RUN, 'dump1.ppm')}", flush=True)

    d.ctl(c, "ring", "thinking")
    time.sleep(0.5)
    d.ctl(c, "stats", "reset")
    time.sleep(5.0)
    n, _ = stats(c)
    cost(c, "thinking ring, 5 s")
    d.need(n <= 5 * 10 + 4, f"busy ring: {n} frames in 5 s (at most 10 a second, plus the ctl calls)")
    d.ctl(c, "ring", "idle")
    time.sleep(1.0)
    f0, _ = stats(c)
    time.sleep(10.0)
    f1, _ = stats(c)
    d.need(f1 == f0, f"still desktop with an idle ring: 0 frames in 10 s (frames={f0}, then {f1})")
    d.ctl(c, "quit")


if __name__ == "__main__":
    main()
