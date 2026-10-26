#!/usr/bin/env python3
"""desk fix typeloss (70-typeloss.sh): typed input through MCP send_keys -> evdev -> bitscomp -> foot -> pty -> ash.

Usage: v2_typeloss.py <mcp host port> <token> <qmp socket> <run dir>
Every line `echo abcdefghijklmnopqrstuvwxyz0123456789 > /tmp/tl<n>` goes out in one send_keys call (as on the
Mini-PC) and must arrive whole. Exit 0 = green (no line lost, no evdev overflow, MCP reported no drop)."""
import os
import re
import sys
import time

sys.dont_write_bytecode = True
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import v2_foot as f  # noqa: E402  (sets argv for v2_check)
from v2_check import Job, call, ctl, need  # noqa: E402
import v2_check as v  # noqa: E402

WORD = "abcdefghijklmnopqrstuvwxyz0123456789"
LINES = int(os.environ.get("TL_LINES", "50"))
FRESH = int(os.environ.get("TL_FRESH", "10"))
BURST = int(os.environ.get("TL_BURST", "10"))
LOST = []
DROP_REPORTS = []
TYPED = [0]


def send_line(c, path):
    text = f"echo {WORD} > {path}\n"
    err, out, _ = call(c, "send_keys", {"text": text})
    need(not err, f"send_keys for {path}", out)
    TYPED[0] += len(text)
    if "dropped" in out:
        DROP_REPORTS.append(out)


def check_line(c, path, secs=10):
    end = time.time() + secs
    while True:
        t = f.read_file(c, path)
        if t == WORD + "\n":
            return True
        if time.time() > end:
            LOST.append((path, t))
            print(f"LOST: {path} holds {t!r}", flush=True)
            f.clean_line(c)
            return False
        time.sleep(0.2)


def counters(c):
    time.sleep(2.5)  # the sampler takes the counters every 2 s
    _, text, _ = call(c, "metrics", {"name": "input.evdev_overflows,input.evdev_lost", "last": 5, "max": 1})
    got = {}
    for l in text.splitlines():
        m = re.match(r"(input\.evdev_\w+) \S+ (\S+)", l)
        if m:
            got[m.group(1)] = 0 if m.group(2) == "-" else int(float(m.group(2)))
    return got, text


def main():
    c = v.connect(300)
    comp = f.COMP = Job(c, "/bin/bitscomp", ["--socket", v.SOCK])
    comp.wait_for(r"backend=fbdev size=1280x800", "bitscomp started on fbdev at 1280x800")
    f.keys(c, "ctrl+alt+enter")
    (a,) = f.wait_windows(c, 1, "Strg+Alt+Enter opened a foot window")
    f.wait_shells(c, 1, "the first foot's shell is ready")
    w, _ = f.hot(c)
    f.focus_in(c, a["id"], w[a["id"]]["body"])

    t0 = time.time()
    for i in range(LINES):
        send_line(c, f"/tmp/tl{i}")
        check_line(c, f"/tmp/tl{i}")
    print(f"phase lines: {LINES} lines, {len(LOST)} lost, {time.time() - t0:.1f} s", flush=True)

    # Right after a foot opened: a second foot, typed into as soon as its shell waits.
    lost0 = len(LOST)
    f.keys(c, "ctrl+alt+enter")
    f.wait_windows(c, 2, "a second foot window opened")
    f.wait_shells(c, 2, "both foot shells are ready")
    for i in range(FRESH):
        send_line(c, f"/tmp/tf{i}")
        check_line(c, f"/tmp/tf{i}")
    print(f"phase fresh foot: {FRESH} lines, {len(LOST) - lost0} lost", flush=True)

    # Back to back: the next call goes out before the shell ran the line before.
    lost0 = len(LOST)
    for i in range(BURST):
        send_line(c, f"/tmp/tb{i}")
    for i in range(BURST):
        check_line(c, f"/tmp/tb{i}", secs=20)
    print(f"phase burst: {BURST} lines, {len(LOST) - lost0} lost", flush=True)

    got, text = counters(c)
    stats = ctl(c, "stats")
    print(f"typed {TYPED[0]} characters in {LINES + FRESH + BURST} send_keys calls", flush=True)
    print(f"kernel: {got}", flush=True)
    stats = next((l for l in stats.splitlines() if "input_dropped=" in l), stats)
    print(f"bitscomp: {stats.strip()}", flush=True)
    print(f"MCP send_keys answers that reported dropped events: {len(DROP_REPORTS)}", flush=True)
    for r in DROP_REPORTS[:3]:
        print("  " + r, flush=True)
    m = re.search(r"input_dropped=(\d+)", stats)
    need(not LOST, f"no typed line lost ({len(LOST)} of {LINES + FRESH + BURST} lost)", "\n".join(map(str, LOST)))
    need(got.get("input.evdev_overflows", 0) == 0, "no evdev reader ring overflowed", text)
    need(m is not None and m.group(1) == "0", "bitscomp read no SYN_DROPPED", stats)
    need(not DROP_REPORTS, "MCP send_keys reported no dropped events", "\n".join(DROP_REPORTS[:3]))
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
