#!/usr/bin/env python3
"""DS-1: reference pictures drawn on the host by headless bitscomp (no settingsd: its built-in defaults, the design
v2 tokens), two wyclient windows mapped in order and the pointer in the first one's middle.

Usage: ds_reference.py <bitscomp> <wyclient> <dir> <WxH> <shot>...
A shot is `name` or `name:step;step`: a step `key=value` is set (through `ctl set`, cumulative), a step `!start`
opens a new workspace (the start page), `!home` removes it again, `!menu` opens the circle menu on window "one"
with its "Breiter" item under the pointer; then <dir>/<name>.ppm is taken. Writes <dir>/pointer.txt ("X Y",
physical px, before any step moves the pointer). Exit 0 = done."""
import os
import subprocess
import sys
import time

BIN, CLIENT, DIR, SIZE, SHOTS = sys.argv[1], sys.argv[2], sys.argv[3], sys.argv[4], sys.argv[5:]
SCALE = 1.25  # theme.scale's default
SOCK = "wayland-dsref"
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


def hot_at(pattern, field):
    for line in ctl("hot").splitlines():
        if pattern in line and f" {field}=" in line:
            return line.split(f" {field}=")[1].split()[0].split(",")
    raise SystemExit(f"RED: reference: no {field} on a hot line with {pattern}\n{ctl('hot')}")


def action(a):
    if a == "!start":
        ctl("click", *hot_at("wsnew", "at"))
        wait_for(lambda: "ws 2 active=true tiles=0" in ctl("ws"), "the new workspace is not shown")
        wait_for(lambda: "\nstart new at=" in ctl("hot"), "no start page on the empty workspace")
    elif a == "!home":
        ctl("ws", "remove")
        wait_for(lambda: "ws 1 active=true tiles=2" in ctl("ws"), "not back on workspace 1")
    elif a == "!menu":
        ctl("click", *hot_at('title="one"', "header"), "right")
        wait_for(lambda: "\nmenu centre=" in ctl("hot"), "no circle menu on window one")
        ctl("pointer", *hot_at('label="Breiter"', "at"))
    else:
        raise SystemExit(f"RED: reference: unknown step {a}")


def pointer_spot(listing):
    for line in listing.splitlines():
        if line.startswith("tile=0 "):
            xy, wh = line.split("frame=")[1].split()[:2]
            x, y = map(int, xy.split(","))
            w, h = map(int, wh.split("x"))
            return round((x + w / 2) * SCALE), round((y + h / 2 + 10) * SCALE)
    raise SystemExit(f"RED: no tile 0 in\n{listing}")


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
    x, y = pointer_spot(ctl("list"))
    ctl("pointer", str(x), str(y))
    with open(os.path.join(DIR, "pointer.txt"), "w") as f:
        f.write(f"{x} {y}\n")
    for shot in SHOTS:
        name, _, sets = shot.partition(":")
        for kv in filter(None, sets.split(";")):
            if kv.startswith("!"):
                action(kv)
                continue
            k, _, v = kv.partition("=")
            ctl("set", k, v)
        time.sleep(0.4)
        ctl("shot", os.path.join(DIR, f"{name}.ppm"))
        hot = ctl("hot")
        shown = [w for w, m in [("menu", "\nmenu centre="), ("start", "\nstart new at=")] if m in hot]
        print(f"reference {name}: {ctl('theme').strip()} shows=[{','.join(shown)}]", flush=True)
    # WY-13: at the cheap effect level a still desktop draws nothing (the last shot's settle time has passed).
    ctl("key", "1")
    time.sleep(1.0)
    frames = lambda: int(ctl("stats").split("frames=")[1].split()[0])
    f0 = frames()
    time.sleep(2.0)
    print(f"idle frames: {f0} -> {frames()}", flush=True)
    ctl("quit")
finally:
    for p in reversed(procs):
        p.kill()
        p.wait()
