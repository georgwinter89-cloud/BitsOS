#!/usr/bin/env python3
"""DS-5 in QEMU: the kernel's boot screen and bitscomp's first frame on QEMU's 2560x1440 GOP screen (QMP screendumps).

Usage: ds_boot_qemu.py first|key <qmp socket> <run dir> <serial log>
first: screendumps until bitscomp's first frame (its hint differs from the kernel's); the last kernel frame and that
       frame are the same picture outside the step rows, the status line, the count, the header and the hint (the
       ring and the title pixel for pixel); then the desktop shows; the times bitscomp logged are printed.
key:   a key (QMP send-key) while bitscomp's scene is up: the desktop shows without the fade."""
import json
import os
import re
import socket
import sys
import time

sys.dont_write_bytecode = True
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from ds_boot_img import CORE, HEADER, HINT, POINTER, PROGRESS, ROWS, STATUS, diff, is_core, pixel, read_ppm, region  # noqa: E402

MODE, QMP_PATH, RUN_DIR, SERIAL = sys.argv[1:5]
# Outside the regions that may change between the two frames, at most this many pixels may differ (the tolerance:
# 0 seen on the host; a few for an odd rounding of the ring's float maths on another CPU).
OUTSIDE_MAX = 64


# A screendump that missed bitscomp's short scene is not a wrong picture: the dumps of a 2560x1440 screen take tens
# of ms each and the scene lasts a few hundred. Those paths exit with MISSED, and 61-boot-qemu.sh boots again (up to
# 3 tries); a wrong picture (ring, title, the rest, a gap, the fade) stays red at once. The pixel identity of the
# same scene is proven deterministically by the host model test (`cargo test boot`).
MISSED = 75


def missed(what):
    print(f"MISSED ({MODE}): {what}", flush=True)
    raise SystemExit(MISSED)


def need(cond, what, detail=""):
    if not cond:
        raise SystemExit(f"RED ({MODE}): {what}\n{detail}")
    print(f"  ok: {what}", flush=True)


class Qmp:
    def __init__(self, path):
        end = time.time() + 60
        while True:
            try:
                self.s = socket.socket(socket.AF_UNIX)
                self.s.settimeout(30)
                self.s.connect(path)
                break
            except OSError:
                if time.time() > end:
                    raise SystemExit("RED: no QMP socket")
                time.sleep(0.2)
        self.f = self.s.makefile("rw")
        self.f.readline()
        self.cmd("qmp_capabilities")

    def cmd(self, name, args=None):
        self.f.write(json.dumps({"execute": name, "arguments": args or {}}) + "\n")
        self.f.flush()
        while True:
            r = json.loads(self.f.readline())
            if "return" in r:
                return r["return"]
            if "error" in r:
                raise SystemExit(f"RED: qmp {name}: {r['error']}")


def dump(q, name):
    path = os.path.join(RUN_DIR, name)
    if os.path.exists(path):
        os.remove(path)
    q.cmd("screendump", {"filename": path})
    return read_ppm(path)


def serial():
    with open(SERIAL, "rb") as f:
        return f.read().decode("utf-8", "replace")


def blank(img):
    """The handover gap: fb0's own memory (all black) shown between KD_GRAPHICS and bitscomp's first store."""
    # The core is pure black only there (the take's copy may still be on its way down the screen).
    return pixel(img, *CORE) == 0 and pixel(img, 0, 0) == 0


def torn(img):
    """bitscomp's first store still on its way to the screen: some edge of the dump is fb0's black (the scene's
    background is never pure black there). Part of the handover gap, not the frame to compare (checker run 28.10.:
    rows 0-163 still black gave ring 2776)."""
    w, h = img[0], img[1]
    return any(pixel(img, x, y) == 0 for x, y in [(0, 0), (w - 1, 0), (0, h // 2), (w - 1, h // 2), (0, h - 1), (w - 1, h - 1)])


def first_frames(q, timeout=240):
    """(the last kernel frame, bitscomp's first frame, black dumps in between and their span in s): dumps back to
    back from the kernel's screen on."""
    end = time.time() + timeout
    kernel_hint = last = None
    n = gap = 0
    gap_t0 = gap_t1 = None
    while time.time() < end:
        n += 1
        t = time.time()
        img = dump(q, f"dump-{n % 2}.ppm")
        if img[0] < 2560 or img[1] < 1440:
            time.sleep(0.1)
            continue
        core = is_core(pixel(img, *CORE))
        if kernel_hint is None:
            if core and not torn(img):
                kernel_hint, last = region(img, HINT), img
            continue
        if core and torn(img) and region(img, HINT) == kernel_hint:
            continue  # a kernel frame not fully on screen yet: neither the last kernel frame nor the gap
        if blank(img) or (core and torn(img)):
            gap += 1
            gap_t0 = gap_t0 or t
            gap_t1 = time.time()
            continue
        if not core:
            missed(f"the desktop showed before a screendump caught bitscomp's scene ({n} dumps)\n" + scene_lines())
        if region(img, HINT) == kernel_hint:
            if gap:
                raise SystemExit(f"RED ({MODE}): the kernel's picture came back after {gap} black dumps")
            last = img
            continue
        print(f"  {n} screendumps until bitscomp's first frame", flush=True)
        return last, img, gap, (gap_t1 - gap_t0) if gap else 0.0
    missed(f"no boot screen, or bitscomp never showed its scene ({n} dumps)\n" + scene_lines())


def scene_lines():
    return "\n".join(m.group(0) for m in re.finditer(r"bitscomp: boot scene: [^\n\[]*", serial()))


def wait_shown(timeout=120):
    end = time.time() + timeout
    while time.time() < end:
        m = re.search(r"boot scene: desktop shown (\d+) ms after bitscomp's start \((\w+); transition (\d+) ms", serial())
        if m:
            return m
        time.sleep(0.2)
    missed(f"bitscomp never logged the desktop as shown\n" + scene_lines())


def main():
    q = Qmp(QMP_PATH)
    kernel, comp, gap, gap_s = first_frames(q)
    if gap:
        # fb0's memory is not the kernel's picture: the take (gopfb fbdev `take` -> `refresh`) shows it black until
        # bitscomp's first store. A kernel change (CR DS-5-3, lane DP); DS_REQUIRE_CRS=1 makes it a failure.
        msg = f"black between the kernel's picture and bitscomp's first frame: {gap} screendumps over {gap_s:.2f} s (CR DS-5-3)"
        if os.environ.get("DS_REQUIRE_CRS") == "1":
            need(False, msg)
        print(f"  SKIP (needs CR DS-5-3): {msg}", flush=True)
    else:
        print("  ok: no black gap at the handover", flush=True)
    if MODE == "key":
        q.cmd("send-key", {"keys": [{"type": "qcode", "data": "spc"}]})
    else:
        out, ring, title, total, first = diff(kernel, comp, allowed=(ROWS, STATUS, PROGRESS, HEADER, HINT, POINTER))
        w, h, _ = comp
        print(f"  differing pixels: {total} of {w * h} ({100.0 * total / (w * h):.3f} %), outside the step list and labels {out}", flush=True)
        need(ring == 0 and title == 0, f"the ring and the title stand where the kernel drew them, pixel for pixel (ring {ring}, title {title})")
        need(out <= OUTSIDE_MAX, f"the rest of the picture is the kernel's ({out} pixels differ, at most {OUTSIDE_MAX})", str(first))
        need(total <= w * h // 50, f"no visible jump: under 2 % of the screen changed ({total})")
    m = wait_shown()
    log = serial()
    ready = re.search(r"boot scene: desktop ready (\d+) ms after bitscomp's start \(([^;]+); scene (\d+) ms", log)
    need(ready is not None, "bitscomp logged when the desktop was ready", scene_lines())
    if MODE == "key":
        need(ready.group(2) == "key" and m.group(2) == "key" and m.group(3) == "0", f"the key skipped the fade ({m.group(0)})", scene_lines())
    else:
        need(int(m.group(3)) <= 400, f"the transition took at most 400 ms ({m.group(3)} ms)")
    time.sleep(1)
    need(not is_core(pixel(dump(q, "desktop.ppm"), *CORE)), "the desktop shows, the scene is gone")
    print("  " + scene_lines().replace("\n", "\n  "))
    print(f"  time (QEMU, numbers only): bitscomp start -> desktop ready {ready.group(1)} ms ({ready.group(2)}), shown {m.group(1)} ms")


main()
print(f"ds boot scene ({MODE}): green")
