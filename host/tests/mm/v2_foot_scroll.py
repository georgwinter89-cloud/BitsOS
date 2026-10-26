#!/usr/bin/env python3
"""MM-rmap scenario client (50-foot-scroll.sh): one foot in bitscomp prints 200000 lines, then 4000 lines one per frame; /proc/meminfo Shmem
must stay bounded (foot punches the scrolled-off part of its pool while bitscomp maps it).

Usage: v2_foot_scroll.py <mcp host port> <token> <qmp socket> <run dir>. Exit 0 = green."""
import os
import re
import sys
import time

sys.dont_write_bytecode = True  # no __pycache__ in the tree
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "wy"))
import v2_foot as f  # noqa: E402
from v2_foot import v, Job, call, need  # noqa: E402

LINES = 200000
BOUND_KB = 64 * 1024


def shmem_kb(c):
    t = f.read_file(c, "/proc/meminfo") or ""
    m = re.search(r"^Shmem:\s+(\d+) kB", t, re.M)
    need(m is not None, "/proc/meminfo has Shmem", t[:400])
    return int(m.group(1))


def phase(c, line, done_file, base):
    cmd = f"{line}; echo $? > {done_file}\n"
    f.type_text(c, cmd)
    peak, samples, start, retyped = base, [], time.time(), False
    while True:
        done = f.read_file(c, done_file)
        s = shmem_kb(c)
        peak = max(peak, s)
        samples.append(s)
        if done is not None and done.endswith("\n"):
            break
        if not retyped and time.time() - start > 15 and done is None and len(set(samples)) == 1:
            print("WARN: nothing ran after 15 s (lost typed input?): typing the line again", flush=True)
            f.clean_line(c)
            f.type_text(c, cmd)
            retyped = True
        if time.time() - start > 600:
            raise SystemExit(f"RED: {line!r} did not finish within 600 s; Shmem samples {samples}")
        time.sleep(2)
    need(done == "0\n", f"{line!r} ended with 0 in foot", repr(done))
    print(f"Shmem while {line!r} ran (kB, every 2 s): {samples}; took {time.time() - start:.0f} s (QEMU, not judged)", flush=True)
    return peak


def main():
    c = v.connect(300)
    comp = Job(c, "/bin/bitscomp", ["--socket", v.SOCK])
    comp.wait_for(r"backend=fbdev size=1280x800", "bitscomp started on fbdev at 1280x800")
    f.keys(c, "ctrl+alt+enter")
    f.wait_windows(c, 1, "Strg+Alt+Enter opened a foot window")
    f.wait_shells(c, 1, "foot's shell is ready")
    f.typed_into_shell(c, lambda: f.type_text(c, "echo ready > /tmp/mm-ready\n"), "/tmp/mm-ready", r"ready\n", "the shell takes input")
    base = shmem_kb(c)
    print(f"Shmem before the scroll: {base} kB", flush=True)
    # Fast: 200000 lines (foot draws whole screens, many lines per frame). Slow: one line per frame, so every
    # frame scrolls by one row and foot moves its buffer forward in the pool (its shm scrolling), punching behind.
    peak = phase(c, f"seq {LINES}", "/tmp/mm-scrolled1", base)
    peak = max(peak, phase(c, "i=0; while [ $i -lt 4000 ]; do echo line $i; sleep 0.01; i=$((i+1)); done", "/tmp/mm-scrolled2", base))
    after = shmem_kb(c)
    print(f"Shmem before {base} kB, peak {peak} kB, after {after} kB", flush=True)
    need(peak - base <= BOUND_KB, f"Shmem grew by {peak - base} kB while foot scrolled (bound {BOUND_KB} kB)")
    f.keys(c, "ctrl+d")
    f.wait_windows(c, 0, "the shell's exit closed foot")
    print("ok: done", flush=True)


if __name__ == "__main__":
    main()
