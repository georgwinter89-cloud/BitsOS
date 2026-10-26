#!/usr/bin/env python3
"""WY-7 (72-latency.sh): input-to-screen latency while many terminals start, as Georg did on 21.10.

Usage: v2_latency.py <mcp host port> <token> <qmp socket> <run dir>
bitscomp through MCP `run`, one foot; a second MCP connection moves the pointer every 30 ms (bitscomp reads the
mouse from /dev/input and draws its own pointer: each move is one frame on the kernel's `program` latency path).
First a quiet stretch, then LAT_FOOTS (default 10) Strg+Alt+Enter back to back while the pointer keeps moving,
then LAT_KEYS (default 5) more, one every 0.3 s, with the pointer still (the launch press alone must be answered).
The kernel's `latency` tool gives the figures; they are printed, judged only when LAT_BUDGET_MS is set (QEMU timing
is indicative, the Mini-PC decides). Always checked, timing-free: programs start through the trampoline and a
windowless start carries the "starting" mark until it ends. Exit 0 = green (all windows came, no evdev drop)."""
import os
import re
import sys
import threading
import time

sys.dont_write_bytecode = True
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import v2_foot as f  # noqa: E402  (sets argv for v2_check)
from v2_check import Job, call, ctl, need  # noqa: E402
import v2_check as v  # noqa: E402

FOOTS = int(os.environ.get("LAT_FOOTS", "10"))
KEYS = int(os.environ.get("LAT_KEYS", "5"))
BUDGET = os.environ.get("LAT_BUDGET_MS")


def program_line(c, reset=False):
    _, text, _ = call(c, "latency", {"last": 64, "reset": reset})
    line = next((l for l in text.splitlines() if l.startswith("program:")), "program: (none)")
    return line, text


def max_ms(line):
    m = re.search(r"max_us=([\d.]+)", line)
    return float(m.group(1)) / 1e3 if m else 0.0


class Mover(threading.Thread):
    """Moves the pointer by +-24 px every 30 ms on its own MCP connection until stopped."""

    def __init__(self):
        super().__init__(daemon=True)
        self.stop = threading.Event()
        self.moves = 0
        self.errors = 0

    def run(self):
        c = v.connect(30)
        d = 24
        while not self.stop.is_set():
            err, _, _ = call(c, "send_mouse", {"dx": d, "dy": 0})
            self.errors += bool(err)
            self.moves += 1
            d = -d
            time.sleep(0.03)


def phase(c, mover, secs):
    program_line(c, reset=True)
    m0 = mover.moves
    time.sleep(secs)
    line, text = program_line(c)
    return line, text, mover.moves - m0


def slowest(text, n=5):
    rows = [l for l in text.splitlines() if l.startswith("program arrival_ns")]
    return sorted(rows, key=lambda l: -float(re.search(r"total_us=([\d.]+)", l).group(1)))[:n]


def main():
    c = v.connect(300)
    # WY-8: LAT_FB=--fb-pan (GX-3 CR 2's two windows) or --fb-copy (the default) for comparing both.
    comp = f.COMP = Job(c, "/bin/bitscomp", ["--socket", v.SOCK, "--stats", "5", *os.environ.get("LAT_FB", "").split()])
    comp.wait_for(r"backend=fbdev size=\d+x\d+", "bitscomp started on fbdev")
    f.keys(c, "ctrl+alt+enter")
    f.wait_windows(c, 1, "Strg+Alt+Enter opened a foot window")
    f.wait_shells(c, 1, "the first foot's shell is ready")
    f.mouse(c, x=640, y=400)
    mover = Mover()
    mover.start()
    quiet, _, n = phase(c, mover, 5)
    print(f"quiet ({n} pointer moves): {quiet}", flush=True)

    program_line(c, reset=True)
    t0, m0 = time.time(), mover.moves
    for _ in range(FOOTS):
        f.keys(c, "ctrl+alt+enter")
    f.wait_windows(c, FOOTS + 1, f"{FOOTS} more foot windows opened", secs=120)
    f.wait_shells(c, FOOTS + 1, "all foot shells are ready", secs=120)
    time.sleep(1)
    burst, text = program_line(c)
    print(f"burst ({FOOTS} foot in {time.time() - t0:.1f} s, {mover.moves - m0} pointer moves): {burst}", flush=True)
    for l in slowest(text):
        print("  slowest: " + l, flush=True)
    mover.stop.set()
    mover.join(5)

    # Keys alone, a start every 0.3 s as by hand: each launch press must be answered by the frame after it
    # (the "starting" mark), not by the program's first window.
    program_line(c, reset=True)
    for _ in range(KEYS):
        f.keys(c, "ctrl+alt+enter")
        time.sleep(0.3)
    f.wait_windows(c, FOOTS + KEYS + 1, f"{KEYS} more foot windows opened by keys alone", secs=120)
    time.sleep(1)
    keys, text = program_line(c)
    print(f"keys alone ({KEYS} starts): {keys}", flush=True)
    for l in slowest(text):
        print("  slowest: " + l, flush=True)
    stats = next((l for l in ctl(c, "stats").splitlines() if "frames=" in l), "")
    print(f"bitscomp: {stats.strip()}", flush=True)
    # The two fixes as mechanisms (timing-free, so a regression turns this red although QEMU timing is not judged):
    # programs start through the posix_spawn trampoline, and a start without a window yet carries the mark until
    # the program ends (a windowless `sleep`: the mark goes with it, long before its 5 s limit).
    need(" spawn=trampoline" in stats, "programs start through the trampoline (no fork of the compositor)", stats)
    need(" launching=0 " in stats + " ", "no start waits for a window once all foot windows are up", stats)
    need(ctl(c, "spawn", "sleep", "1").strip().startswith("pid "), "ctl spawn sleep 1")
    marked = next((l for l in ctl(c, "stats").splitlines() if "frames=" in l), "")
    need(" launching=1 " in marked + " ", "a windowless start carries the starting mark", marked)
    end = time.time() + 3.5
    while " launching=0 " not in next((l for l in ctl(c, "stats").splitlines() if "frames=" in l), "") + " ":
        if time.time() > end:
            raise SystemExit("RED: the mark stayed after the windowless program ended (should go before its 5 s limit)")
        time.sleep(0.3)
    print("ok: the starting mark went with the program's end", flush=True)
    need(mover.errors == 0, "every pointer move was taken by MCP", f"{mover.errors} errors")
    need("frames=" in burst and not burst.endswith("frames=0"), "frames measured on the program path during the burst", burst)
    worst = max(max_ms(burst), max_ms(keys))
    if BUDGET:
        need(worst <= float(BUDGET), f"worst input-to-screen latency while terminals start <= {BUDGET} ms (was {worst:.1f} ms)",
             burst + "\n" + keys)
    else:
        print(f"note: worst {max_ms(burst):.1f} ms in the burst, {max_ms(keys):.1f} ms by keys alone, {max_ms(quiet):.1f} ms "
              "quiet (not judged: QEMU timing is indicative; LAT_BUDGET_MS=50 judges it)", flush=True)
    with open(os.path.join(v.RUN, "bitscomp.txt"), "w") as out:
        out.write(comp.poll())
    ctl(c, "quit")


if __name__ == "__main__":
    try:
        main()
    except SystemExit as e:
        if e.code not in (0, None) and f.COMP is not None:
            print(str(e.code))
            try:
                f.COMP.poll()
            except Exception as x:
                print(f"(no more output: {x})")
            print(f.COMP.text[-3000:])
            raise SystemExit(1)
        raise
