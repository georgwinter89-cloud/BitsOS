#!/usr/bin/env python3
"""WY-15 (99-gles-cost.sh): bitscomp's CPU cost per frame with Pixman and with GLES on llvmpipe, in one V2 boot.

Usage: v2_cost.py <mcp host port> <token> <qmp socket> <run dir>
For each renderer: the gpu build (`/boot/BITSOS2/bin/bitscomp-gpu --renderer R`) through MCP `run`, one
`wyclient --animate` (a new picture on every frame callback), COST_SECS (default 15) seconds measured between two
snapshots of `ctl stats` and of bitscomp's threads' CPU time (/proc/PID/task/*/stat, utime+stime in USER_HZ 100).
Numbers only; checked: the start line names the renderer, frames were drawn, bitscomp ends on `ctl quit`."""
import os
import re
import sys
import time

sys.dont_write_bytecode = True
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
sys.argv = sys.argv[:5] + ["-"]
import v2_check as v  # noqa: E402
from v2_check import Job, ctl, need, shell  # noqa: E402

SECS = float(os.environ.get("COST_SECS", "15"))
GPU = "/boot/BITSOS2/bin/bitscomp-gpu"
HZ = 100


def cpu_ticks(c, pid):
    out = shell(c, f"cat /proc/{pid}/task/*/stat")
    total, n = 0, 0
    for line in out.splitlines():
        if ")" not in line:
            continue
        f = line.rsplit(")", 1)[1].split()
        total += int(f[11]) + int(f[12])
        n += 1
    return total, n


def stats(c):
    t = ctl(c, "stats")
    m = re.search(r"frames=(\d+) .*render_avg_ms=([\d.]+) present_avg_ms=([\d.]+) damaged_mpx_avg=([\d.]+)", t)
    need(m is not None, "ctl stats answers", t)
    return int(m.group(1)), float(m.group(2)), float(m.group(3)), float(m.group(4))


def measure(c, renderer):
    comp = Job(c, GPU, ["--socket", v.SOCK, "--renderer", renderer])
    comp.wait_for(r"backend=fbdev size=\d+x\d+ .*renderer=\w+", f"bitscomp ({renderer}) started on fbdev", 60)
    m = re.search(r"pid=(\d+) .*build=gpu renderer=(\w+) egl=(\w+)", comp.text)
    need(m is not None and m.group(2) == renderer, f"the start line says renderer={renderer}", comp.text[-1500:])
    pid = m.group(1)
    if renderer == "gles":
        g = re.search(r"renderer gles: (GL_RENDERER=[^;\n]*)", comp.text)
        need(g is not None and "llvmpipe" in g.group(1), "GLES draws with llvmpipe", comp.text[-1500:])
        print("gles:", g.group(1), flush=True)
    client = Job(c, "/bin/wyclient", ["--title", "anim", "--pattern", "0", "--animate"])
    client.wait_for(r"configure \d+ \d+", "the animated client is configured")
    time.sleep(3)
    f0, r0, p0, _ = stats(c)
    t0, _ = cpu_ticks(c, pid)
    w0 = time.time()
    time.sleep(SECS)
    f1, r1, p1, d1 = stats(c)
    t1, threads = cpu_ticks(c, pid)
    wall = time.time() - w0
    frames = f1 - f0
    need(frames > 0, f"{renderer}: frames were drawn ({frames})")
    # ctl stats gives running averages since the start: the window's own share from the totals.
    render_ms = (r1 * f1 - r0 * f0) / frames
    present_ms = (p1 * f1 - p0 * f0) / frames
    cpu_ms = (t1 - t0) * 1000.0 / HZ
    row = (f"{renderer}: {frames} frames in {wall:.1f} s ({frames / wall:.1f}/s), CPU {cpu_ms / frames:.2f} ms per frame "
           f"({cpu_ms / wall / 10:.0f} % of one CPU), render {render_ms:.2f} ms, present {present_ms:.2f} ms, "
           f"{d1:.3f} Mpx damaged per frame (average since start), {threads} threads")
    print("COST", row, flush=True)
    ctl(c, "quit")
    comp.wait_for(r"bitscomp: ended", f"bitscomp ({renderer}) ended after ctl quit", 30)
    time.sleep(2)
    return row


def main():
    c = v.connect(300)
    rows = [measure(c, "pixman"), measure(c, "gles")]
    with open(os.path.join(v.RUN, "cost.txt"), "w") as f:
        f.write("\n".join(rows) + "\n")
    print("ok: both renderers measured", flush=True)


if __name__ == "__main__":
    main()
