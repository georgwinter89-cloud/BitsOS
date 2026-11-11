#!/usr/bin/env python3
"""WY-17 scenario client (45-v2-scanout.sh): the direct path on V2, over MCP.

Usage: v2_scanout.py <mcp host port> <token> <qmp socket> <run dir> fake|off
Exit 0 = green. Timing is never judged (QEMU): only what shows and what was chosen."""
import os
import re
import sys
import time

sys.dont_write_bytecode = True  # no __pycache__ in the tree
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
MODE = sys.argv[5]
sys.argv = sys.argv[:5] + ["-"]
import v2_check as v  # noqa: E402
from v2_check import Job, Qmp, call, connect, console_back, differing, need, screen  # noqa: E402

ENV = dict(v.ENV)
SOCK_N = 0


def ctl(c, sock, *words):
    _, text, _ = call(c, "run", {"path": "/bin/bitscomp", "args": ["ctl", "--socket", sock, *words], "env": ENV, "timeout_ms": 30000})
    return text


class Round:
    """One compositor with one window, started with `args`."""

    def __init__(self, c, q, args):
        global SOCK_N
        SOCK_N += 1
        self.c, self.q, self.sock = c, q, f"wayland-{SOCK_N}"
        self.comp = Job(c, "/bin/bitscomp", ["--socket", self.sock, "--fixed-status", *args], sock=self.sock)
        self.comp.wait_for(rf"WAYLAND_DISPLAY={self.sock} .*backend=fbdev size=1280x800", f"bitscomp {args} started on fbdev", 90)
        Job(c, "/bin/wyclient", ["--title", "one", "--pattern", "0"], sock=self.sock)
        end = time.time() + 60
        while 'title="one"' not in ctl(c, self.sock, "list"):
            if time.time() > end:
                raise SystemExit(f"RED: the window did not map\n{ctl(c, self.sock, 'list')}\n{self.comp.poll()[-1500:]}")
            time.sleep(0.5)
        time.sleep(6)

    def said(self, pattern, what):
        need(re.search(pattern, self.comp.poll()) is not None, what, self.comp.text[-2500:])

    def stat(self, key):
        m = re.search(rf"\b{key}=(\w+)", ctl(self.c, self.sock, "stats"))
        return m.group(1) if m else None

    def look(self, what):
        """MCP screen, and QEMU's screendump shows the same."""
        w, h, px = screen(self.c, what)
        dw, dh, dpx = self.q.screendump(what.replace(" ", "-"))
        need((dw, dh) == (w, h) and differing(px, dpx, w, tol=0) == 0, f"{what}: MCP screen == QEMU screendump", f"{differing(px, dpx, w, tol=0)} pixels")
        return w, h, px

    def churn(self):
        """The same changes in every round: partial damage, each frame into the other window."""
        out = {"first": self.look("first picture")}
        for _ in range(4):
            ctl(self.c, self.sock, "max")
            time.sleep(1.5)
        out["toggled"] = self.look("after maximise toggles")
        for i in range(1, 9):
            ctl(self.c, self.sock, "pointer", str(i * 130), str(i * 80))
            time.sleep(0.4)
        time.sleep(2)
        out["pointer"] = self.look("after the pointer crossed the screen")
        ctl(self.c, self.sock, "float")
        time.sleep(3)
        ctl(self.c, self.sock, "pointer", "640", "400")
        time.sleep(3)
        out["floated"] = self.look("with a floated window")
        return out

    def quit(self):
        ctl(self.c, self.sock, "quit")
        time.sleep(2)


def same(a, b, what, tol=0, share=0):
    (w, h, pa), (_, _, pb) = a, b
    n = differing(pa, pb, w, tol=tol)
    need(n <= share * w * h, f"{what}: {n} pixels differ", f"tolerance {tol}, at most {share * 100} %")


def counters(c):
    """The sampler takes a round every 2 s."""
    time.sleep(5)
    _, t, _ = call(c, "metrics", {"name": "fbdev.", "last": 30, "max": 1, "agg": "last"})
    out = {}
    for line in t.splitlines():
        f = line.split()
        if len(f) >= 3 and f[0].startswith("fbdev.") and f[-1] != "-":
            out[f[0]] = int(float(f[-1]))
    return out


def fake(c, q):
    pixman = Round(c, q, ["--renderer", "pixman"])
    pixman.said(r"scanout: readback: renderer pixman draws by CPU", "pixman: the choice is logged")
    need(pixman.stat("scanout") == "readback", "pixman: ctl stats says readback")
    ref = pixman.churn()
    pixman.quit()

    back = Round(c, q, ["--scanout", "readback"])
    back.said(r"build gpu, renderer gles \(CONFIG.TXT bitscomp=auto\): handing over", "bitscomp=auto with a node hands over")
    back.said(r"renderer gles: the software rasteriser \(under a hypervisor\)", "under a hypervisor GLES is llvmpipe")
    back.said(r"scanout: readback: --scanout readback", "the choice is logged")
    need(back.stat("scanout") == "readback", "readback: ctl stats says readback")
    read = back.churn()
    back.quit()

    direct = Round(c, q, [])
    direct.said(r"renderer=gles", "bitscomp=auto draws with GLES")
    direct.said(r"renderer gles: GL_RENDERER=llvmpipe", "llvmpipe draws")
    direct.said(r"scanout: direct: gles draws into two dma-buf windows of fb0's memory", "the direct path is chosen and logged")
    need(direct.stat("scanout") == "direct", "direct: ctl stats says direct")
    before = counters(c)
    pics = direct.churn()
    after = counters(c)
    for k in pics:
        same(read[k], pics[k], f"{k}: the direct picture against the readback's")
        same(ref[k], pics[k], f"{k}: the direct picture against Pixman's", tol=24, share=0.003)
    need(direct.stat("scanout") == "direct", "still direct after all of it", direct.comp.poll()[-1500:])
    need("back to readback" not in direct.comp.text and "render:" not in direct.comp.text, "no way out and no skipped frame", direct.comp.text[-1500:])
    need(after.get("fbdev.scanout.calls", 0) > before.get("fbdev.scanout.calls", 0), "the kernel counts the frames' BEGIN", str(after))
    need(after.get("fbdev.damage.pans", 0) > before.get("fbdev.damage.pans", 0), "the frames came through pans with reported damage", str(after))

    # A compositor killed in the middle: the console comes back, and the next one finds its windows.
    pid = re.search(r"pid=(\d+)", direct.comp.text).group(1)
    _, t, _ = call(c, "run", {"path": "/bin/sh", "args": ["-c", f"kill -9 {pid}"], "env": ENV, "timeout_ms": 10000})
    time.sleep(2)
    console_back(c, q, "killed", "after a kill -9 of the direct compositor")
    again = Round(c, q, [])
    again.said(r"scanout: direct", "the next compositor draws direct again")
    same(read["first"], again.look("first picture again"), "the next compositor's picture")
    again.quit()


def off(c, q):
    rescue = Round(c, q, [])
    rescue.said(r"build rescue \(CONFIG.TXT bitscomp=auto\): no render node", "bitscomp=auto without a node stays with the rescue build")
    rescue.said(r"build=rescue renderer=pixman", "the rescue build draws with Pixman")
    rescue.said(r"scanout: readback: renderer pixman draws by CPU", "the choice is logged")
    a = rescue.look("Pixman's picture")
    rescue.quit()

    gles = Round(c, q, ["--renderer", "gles"])
    gles.said(r"renderer=gles", "--renderer gles draws with GLES")
    gles.said(r"scanout: readback: no render node", "no node: the readback path, logged")
    need(gles.stat("scanout") == "readback", "ctl stats says readback")
    same(a, gles.look("GLES's picture"), "against Pixman's", tol=24, share=0.003)
    gles.quit()


def main():
    c = connect(300)
    q = Qmp(sys.argv[3])
    {"fake": fake, "off": off}[MODE](c, q)
    print(f"GREEN: {MODE}")


if __name__ == "__main__":
    main()
