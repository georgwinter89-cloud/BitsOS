#!/usr/bin/env python3
"""WY-17b: vk_check.py's `window` and `presenting` steps (host/tests/minipc, profile real) against a headless bitscomp,
without a Mini-PC: the real `ctl stats` and `ctl list` answers go through the real code, wyclient --dmabuf plays vkcube.
Proves that the `presenting` line still has its form and that a refused buffer still makes it FAIL.

Usage: vk_presenting.py <bitscomp> <wyclient> <socket> [--refuse]
Exit 0 = the line is as expected. Needs WAYLAND's XDG_RUNTIME_DIR and a running bitscomp on <socket>."""
import argparse
import contextlib
import io
import os
import re
import subprocess
import sys

sys.dont_write_bytecode = True  # no __pycache__ in the tree
HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.join(HERE, "..", "..", "minipc"))
sys.path.insert(0, os.path.join(HERE, "..", "..", "gm", "tools"))
ARGV, sys.argv = sys.argv, sys.argv[:1]
import vk_check  # noqa: E402

# What host/tests/minipc/README.md and the desk read off the line: one form, kept by this check.
LINE = re.compile(r"^(PASS|FAIL) presenting: (\d+) commits in (\d+) s \(desktop alone (\d+), needed (\d+)\), frames (\d+), dma-bufs taken (\d+), refused (\d+)(.*)$", re.M)


class Local(vk_check.Check):
    """`Check` with its MCP calls answered on this machine."""

    def __init__(self, a, bitscomp, wyclient, refuse):
        super().__init__(a)
        self.bitscomp, self.wyclient, self.refuse = bitscomp, wyclient, refuse
        self.procs = []

    def ctl(self, *words):
        return subprocess.run([self.bitscomp, "ctl", "--socket", self.a.socket, *words], capture_output=True, text=True, timeout=20).stdout

    def client(self, *args):
        p = subprocess.Popen([self.wyclient, *args], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, env=dict(os.environ, WAYLAND_DISPLAY=self.a.socket))
        self.procs.append(p)
        return p

    def mcp(self, tool, args=None, png=None, save=None, wait=None):
        if tool == "run":
            p = self.client("--title", "vkcube", "--dmabuf", "--animate")
            if self.refuse:
                self.client("--title", "bad", "--dmabuf", "--modifier", "0x0200000000000001")
            return False, f"(call 1) pid {p.pid} vkcube is running\nSelected GPU 0: {vk_check.RADV_NAME}, type: IntegratedGpu\n"
        return False, ""

    def pixels(self, line):
        pass

    def close(self, line):
        pass

    def save(self, name, text):
        pass


def main():
    p = argparse.ArgumentParser()
    p.add_argument("bitscomp")
    p.add_argument("wyclient")
    p.add_argument("socket")
    p.add_argument("--refuse", action="store_true")
    o = p.parse_args(ARGV[1:])
    out_dir = os.environ.get("BITSOS_RUN_DIR", ".")
    a = argparse.Namespace(profile="real", ref_dir=None, out=out_dir, xdg=os.environ["XDG_RUNTIME_DIR"], socket=o.socket)
    chk = Local(a, o.bitscomp, o.wyclient, o.refuse)
    buf = io.StringIO()
    try:
        with contextlib.redirect_stdout(buf):
            chk.window()
    finally:
        for q in chk.procs:
            q.kill()
            q.wait()
    text = buf.getvalue()
    m = LINE.search(text)
    if not m:
        sys.exit(f"RED: no `presenting` line of the known form in:\n{text}")
    word, taken, refused, rest = m.group(1), int(m.group(7)), int(m.group(8)), m.group(9)
    if o.refuse:
        if word != "FAIL" or refused < 1 or "CR-GM1c-2" not in rest or "renderer=pixman" not in rest:
            sys.exit(f"RED: a refused buffer must FAIL and name the renderer:\n{text}")
    elif word != "PASS" or taken < 1 or refused != 0 or "PASS window" not in text:
        sys.exit(f"RED: a window whose buffers were taken must PASS:\n{text}")
    print(m.group(0))


if __name__ == "__main__":
    main()
