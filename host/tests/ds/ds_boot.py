#!/usr/bin/env python3
"""DS-5 on the host: headless bitscomp shows the boot scene from a fixture (BITSCOMP_BOOT_FILE, the format of
/proc/bitsos/boot) at 2560x1440.

Usage: ds_boot.py <bitscomp> <wyclient> <dir> <repo root>
Cases: hold (a started program without a window: the scene waits until the cap, its picture equals the kernel's
outside the desktop row and the hint, pixel for pixel; the fade at most 400 ms; then 0 frames when still), window
(the first client's window ends the wait before the cap), odd files and an earlier start's step (no scene, the
desktop at once). Prints the logged times (numbers only; timing is judged on the Mini-PC)."""
import os
import re
import subprocess
import sys
import time

sys.dont_write_bytecode = True
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from ds_boot_img import CORE, DESKTOP_ROW, HINT, diff, is_core, pixel, read_ppm  # noqa: E402

BIN, CLIENT, DIR, ROOT = sys.argv[1:5]
SOCK = "wayland-dsboot"
env = dict(os.environ, XDG_RUNTIME_DIR=DIR, WAYLAND_DISPLAY=SOCK)
env.pop("DISPLAY", None)
STEPS = [("kernel", "done", 120, "version 0.1.0 · generation 49"), ("memory", "done", 140, "31 gb frei"),
         ("cpus", "done", 280, "16 kerne"), ("storage", "done", 450, "stick sda"),
         ("network", "running", 900, "igc0 verbindet"), ("sound", "done", 330, "hda0"),
         ("gpu", "skipped", 500, "nur gop-bild"), ("services", "running", 700, "init startet"),
         ("mcp", "done", 520, "port 8765")]


def need(cond, what, detail=""):
    if not cond:
        raise SystemExit(f"RED: {what}\n{detail}")
    print(f"ok: {what}", flush=True)


def fixture(name, desktop_ms, screen="handed", body=None):
    path = os.path.join(DIR, name + ".boot")
    if body is None:
        rows = STEPS + [("desktop", "running", desktop_ms, "/bin/bitscomp startet (versuch 1)")]
        body = "bitsos-boot 1\nseq 14\ndrawn 14\nscreen %s\ngeneration 49\n" % screen
        body += "".join(f"step {a} {b} {c} {d}\n" for a, b, c, d in rows)
    with open(path, "w", encoding="utf-8") as f:
        f.write(body)
    return path


def now_ms():
    return int(time.monotonic() * 1000)


def ctl(*words):
    return subprocess.run([BIN, "ctl", "--socket", SOCK, *words], env=env, capture_output=True, text=True, timeout=20).stdout


class Comp:
    def __init__(self, name, boot, run=None):
        self.err = os.path.join(DIR, name + ".err")
        args = [BIN, "--backend", "headless", "--size", "2560x1440", "--socket", SOCK, "--fixed-status", "--settings", "none", "--mcp", "none"]
        if run:
            args += ["--run", run]
        e = dict(env)
        if boot:
            e["BITSCOMP_BOOT_FILE"] = boot
        self.p = subprocess.Popen(args, env=e, stdout=subprocess.DEVNULL, stderr=open(self.err, "w"))
        self.wait(lambda: os.path.exists(os.path.join(DIR, SOCK + ".ctl")), "no control socket")

    def log(self):
        with open(self.err, encoding="utf-8", errors="replace") as f:
            return f.read()

    def wait(self, pred, what, secs=15):
        end = time.time() + secs
        while time.time() < end:
            if pred():
                return
            time.sleep(0.02)
        raise SystemExit(f"RED: {what}\n{self.log()[-2000:]}")

    def shot(self, name):
        path = os.path.join(DIR, name + ".ppm")
        ctl("shot", path)
        return read_ppm(path)

    def frames(self):
        m = re.search(r"frames=(\d+)", ctl("stats"))
        return int(m.group(1)) if m else -1

    def stop(self):
        self.p.terminate()
        try:
            self.p.wait(10)
        except subprocess.TimeoutExpired:
            self.p.kill()
            self.p.wait()


def times(log):
    ready = re.search(r"boot scene: desktop ready (\d+) ms after bitscomp's start \(([^;]+); scene (\d+) ms", log)
    shown = re.search(r"boot scene: desktop shown (\d+) ms after bitscomp's start \(([^;]+); transition (\d+) ms\)", log)
    return ready, shown


def kernel_model(boot):
    """The kernel's own picture of `boot` (console's ignored test writes it; it fails on the bitscomp shot it is given,
    only its picture is used here)."""
    out = os.path.join(DIR, "kernel")
    os.makedirs(out, exist_ok=True)
    e = dict(os.environ, BITSOS_BOOT_PPM=out, BITSOS_SCREENDUMP=os.path.join(DIR, "hold.ppm"), BITSOS_BOOT_PROC=boot)
    subprocess.run(["cargo", "test", "-q", "-p", "console", "--lib", "bootscreen_matches_model", "--", "--ignored"],
                   cwd=ROOT, env=e, capture_output=True, text=True)
    path = os.path.join(out, "boot-model.ppm")
    need(os.path.exists(path), "the kernel's model picture was drawn (console tests_boot)")
    return read_ppm(path)


# hold: a program without a window; the scene waits for it until the cap.
boot = fixture("hold", now_ms())
c = Comp("hold", boot, run="sleep 30")
try:
    c.wait(lambda: "boot scene: shown" in c.log(), "the scene was not shown")
    scene = c.shot("hold")
    need(is_core(pixel(scene, *CORE)), "the first frames show the boot scene (the ring's core)")
    model = kernel_model(boot)
    out, ring, title, total, first = diff(model, scene, allowed=(DESKTOP_ROW, HINT))
    need(out == 0 and ring == 0 and title == 0,
         f"bitscomp's scene equals the kernel's picture pixel for pixel outside the desktop row and the hint ({total} pixels there differ)",
         f"outside {out}, ring {ring}, title {title}, first {first}")
    need(total > 0, "the desktop row shows bitscomp's own step (it differs from the kernel's)")
    c.wait(lambda: times(c.log())[1] is not None, "the desktop did not show", 10)
    ready, shown = times(c.log())
    need(ready.group(2).startswith("cap") and 1700 <= int(ready.group(3)) <= 3000, f"the wait for a window that does not come ends at the cap ({ready.group(0)})")
    need(int(shown.group(3)) <= 400, f"the transition takes at most 400 ms ({shown.group(3)} ms)")
    print(f"time: {ready.group(0)}\ntime: {shown.group(0)}")
    desk = c.shot("hold-desk")
    need(not is_core(pixel(desk, *CORE)), "the desktop shows after the fade")
finally:
    c.stop()

# window: the first client's window ends the wait before the cap.
boot = fixture("window", now_ms())
c = Comp("window", boot, run=CLIENT)
try:
    c.wait(lambda: times(c.log())[1] is not None, "the desktop did not show", 10)
    ready, shown = times(c.log())
    need(ready.group(2) == "ready" and "window shown" in c.log(), f"the first window ends the scene ({ready.group(0)})")
    need("bitscomp: erstes fenster" in c.log() or int(ready.group(3)) < 100, "the desktop row named the wait for the first window")
    print(f"time: {ready.group(0)}")
    # (Not in `hold`: there the launch feedback of the windowless program draws, WY's.)
    time.sleep(0.5)
    f0 = c.frames()
    time.sleep(3)
    need(c.frames() == f0 >= 0, f"after the scene the still desktop draws 0 frames in 3 s (frames={f0})")
finally:
    c.stop()

# no scene: odd files, another state, an earlier start's step; and no file at all (headless without the variable).
for name, path in [
    ("garbage", fixture("garbage", 0, body="\x00\x01 not the format\n")),
    ("text", fixture("text", now_ms(), screen="text")),
    ("earlier", fixture("earlier", now_ms() - 60_000)),
    ("missing", os.path.join(DIR, "nonexistent.boot")),
    ("none", None),
]:
    c = Comp(name, path)
    try:
        time.sleep(0.6)
        shot = c.shot(name)
        log = c.log()
        need(not is_core(pixel(shot, *CORE)) and "boot scene: shown" not in log, f"{name}: no scene, the desktop at once")
        if path:
            need("boot scene: none (" in log, f"{name}: the log says why", log[-1500:])
        else:
            need("boot scene" not in log, "none: a headless run without the variable says nothing about a scene")
    finally:
        c.stop()
print("GREEN: boot scene on the host")
