#!/usr/bin/env python3
"""WY-19 scenario client (46-v2-effects.sh): the effect level on V2 in QEMU, over MCP.

Usage: v2_effects.py <mcp host port> <token> <qmp socket> <run dir> -
Exit 0 = green. Timing is never judged (QEMU): only what shows and how many frames came."""
import os
import re
import sys
import time

sys.dont_write_bytecode = True  # no __pycache__ in the tree
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import v2_check as v  # noqa: E402
from v2_check import Job, Qmp, call, connect, differing, need, screen  # noqa: E402

ENV = dict(v.ENV)
SOCK_N = 0


def ctl(c, sock, *words):
    _, text, _ = call(c, "run", {"path": "/bin/bitscomp", "args": ["ctl", "--socket", sock, *words], "env": ENV, "timeout_ms": 30000})
    return text


class Round:
    """One compositor with one window."""

    def __init__(self, c, q, args):
        global SOCK_N
        SOCK_N += 1
        self.c, self.q, self.sock = c, q, f"wayland-{SOCK_N}"
        self.comp = Job(c, "/bin/bitscomp", ["--socket", self.sock, "--fixed-status", "--settings", "none", *args], sock=self.sock)
        self.comp.wait_for(rf"WAYLAND_DISPLAY={self.sock} .*backend=fbdev size=1280x800", f"bitscomp {args} started on fbdev", 90)
        Job(c, "/bin/wyclient", ["--title", "one", "--pattern", "0"], sock=self.sock)
        end = time.time() + 60
        while 'title="one"' not in ctl(c, self.sock, "list"):
            if time.time() > end:
                raise SystemExit(f"RED: the window did not map\n{self.comp.poll()[-1500:]}")
            time.sleep(0.5)
        time.sleep(6)

    def ctl(self, *words):
        return ctl(self.c, self.sock, *words)

    def field(self, text, key):
        m = re.search(rf"\b{key}=(\w+)", text)
        return m.group(1) if m else None

    def look(self, what, settle=4.0):
        """MCP screen, and QEMU's screendump shows the same."""
        time.sleep(settle)
        w, h, px = screen(self.c, what)
        dw, dh, dpx = self.q.screendump(what.replace(" ", "-") + f"-{self.sock}")
        n = differing(px, dpx, w, tol=0)
        need((dw, dh) == (w, h) and n == 0, f"{what}: MCP screen == QEMU screendump", f"{n} pixels")
        return w, h, px

    def level(self, want):
        end = time.time() + 60
        while self.field(self.ctl("fx"), "level") != want:
            if time.time() > end:
                raise SystemExit(f"RED: level {want} not reached: {self.ctl('fx')}\n{self.comp.poll()[-1500:]}")
            time.sleep(0.5)

    def frames_over(self, secs):
        self.ctl("stats", "reset")
        time.sleep(secs)
        return int(self.field(self.ctl("stats"), "frames"))

    def pictures(self):
        """The same changes in every round; clocks frozen, so the pictures are exact."""
        out = {"cheap": self.look("the cheap level")}
        need(self.frames_over(5) == 0, "cheap: a still desktop draws no frame")
        self.ctl("set", "theme.effects", "gpu")
        self.ctl("fx", "freeze", "4.5")
        self.level("gpu")
        out["gpu"] = self.look("the gpu level", settle=8)
        need(self.frames_over(4) == 0, "frozen clocks ask for no frame")
        self.ctl("ws", "new")
        self.ctl("fx", "stop")
        time.sleep(3)
        self.ctl("ws", "1")
        self.ctl("fx", "stop")
        time.sleep(3)
        self.ctl("ws", "2")
        self.ctl("fx", "hold", "0.35")
        out["wipe"] = self.look("a wipe standing still", settle=8)
        self.ctl("fx", "stop")
        self.ctl("ws", "1")
        self.ctl("fx", "stop")
        self.ctl("fx", "freeze", "off")
        time.sleep(2)
        n = self.frames_over(4)
        need(n > 0, f"ambient motion draws frames: {n}", self.ctl("fx") + self.ctl("stats") + self.comp.poll()[-2500:])
        self.ctl("set", "theme.motion", "off")
        time.sleep(4)
        need(self.frames_over(4) == 0, "motion off: no frame", self.ctl("fx"))
        self.ctl("unset", "theme.motion")
        self.ctl("set", "theme.effects", "cheap")
        self.level("cheap")
        return out

    def quit(self):
        self.ctl("quit")
        time.sleep(2)


def same(a, b, what, tol=0, share=0.0):
    (w, h, pa), (_, _, pb) = a, b
    n = differing(pa, pb, w, tol=tol)
    need(n <= share * w * h, f"{what}: {n} pixels differ", f"tolerance {tol}, at most {share * 100} %")


def main():
    c = connect(300)
    q = Qmp(sys.argv[3])
    pixman = Round(c, q, ["--renderer", "pixman"])
    ref = pixman.look("Pixman's picture")
    need(pixman.field(pixman.ctl("fx"), "renderer") == "pixman", "pixman: ctl fx names it", pixman.ctl("fx"))
    pixman.ctl("set", "theme.effects", "gpu")
    pixman.ctl("pointer", "9", "9")
    time.sleep(2)
    need(pixman.field(pixman.ctl("fx"), "level") == "cheap", "pixman: the gpu level chosen, the cheap level drawn", pixman.ctl("fx"))
    pixman.quit()

    back = Round(c, q, ["--scanout", "readback", "--fx-on-software"])
    need(back.field(back.ctl("stats"), "scanout") == "readback", "readback: ctl stats says readback")
    read = back.pictures()
    back.quit()

    direct = Round(c, q, ["--fx-on-software"])
    need(direct.field(direct.ctl("stats"), "scanout") == "direct", "direct: ctl stats says direct", direct.comp.poll()[-2500:])
    pics = direct.pictures()
    need(direct.field(direct.ctl("stats"), "scanout") == "direct", "still direct after all of it", direct.comp.poll()[-1500:])
    need("render:" not in direct.comp.text, "no skipped frame", direct.comp.text[-1500:])
    direct.quit()
    same(ref, read["cheap"], "cheap: GLES against Pixman", tol=24, share=0.003)
    for k in pics:
        same(read[k], pics[k], f"{k}: the direct path against the readback's", tol=2, share=0.0005)
    (w, h, pg), (_, _, pc) = read["gpu"], read["cheap"]
    need(differing(pg, pc, w, tol=2) > 1000, "the levels differ on V2 too")
    print("GREEN")


if __name__ == "__main__":
    main()
