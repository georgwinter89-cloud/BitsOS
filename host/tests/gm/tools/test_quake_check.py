#!/usr/bin/env python3
"""GM-8: host/tests/minipc/quake_check.py on made-up answers, no machine: a good run passes every step; each way the real run can
go wrong fails the step that should see it, and only with that does the run exit 1. Run by host/tests/gm/50-vkquake.sh."""
import argparse
import os
import re
import sys
import tempfile
import time
import zlib

sys.dont_write_bytecode = True
HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.join(HERE, "..", "..", "minipc"))
import quake_check  # noqa: E402
from vk_check import ROOT, Unreachable  # noqa: E402

quake_check.WINDOW_SECS = 0.05
quake_check.CLOSE_SECS = 0.05
quake_check.PICTURE_SECS = 0.05
quake_check.WATCH_SECS = 0
time.sleep = lambda _s: None
W, H = 64, 40


def png(path, seed):
    rows = b"".join(b"\0" + bytes((x * 7 + y * 3 + seed * k) & 255 for x in range(W) for k in (1, 2, 3)) for y in range(H))
    def chunk(k, b):
        return len(b).to_bytes(4, "big") + k + b + zlib.crc32(k + b).to_bytes(4, "big")
    ihdr = W.to_bytes(4, "big") + H.to_bytes(4, "big") + bytes([8, 2, 0, 0, 0])
    with open(path, "wb") as f:
        f.write(b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", ihdr) + chunk(b"IDAT", zlib.compress(rows)) + chunk(b"IEND", b""))


class Fake(quake_check.Quake):
    """The machine as the run sees it, with `bad` naming what is wrong this time."""

    def __init__(self, a, bad=""):
        super().__init__(a)
        self.bad = bad
        self.playing, self.closed, self.commits, self.signaled, self.reads, self.shots = False, False, 0, 100, 0, 0
        self.calls, self.notes = [], []

    def note(self, text):
        self.notes.append(text)
        super().note(text)

    def mcp(self, tool, args=None, png=None, save=None, wait=None):
        args = args or {}
        self.calls.append((tool, args))
        if self.bad == "link-drops" and self.playing and tool == "screen":
            self.link_gone = True
        if getattr(self, "link_gone", False):
            raise Unreachable("no answer")
        if tool == "ktest":
            return False, "passed=2 failed=0 skipped=0 refused=0" if self.bad != "ktest" else "passed=1 failed=1 skipped=0 refused=0"
        if tool == "snd":
            if args.get("output"):
                return (True, "refused: no display side") if self.bad == "hdmi-refused" else (False, "target=auto\nmixer=idle playing=none")
            return False, "target=auto\nmixer=running playing=hda streams=1 clients=1 played=9" if self.bad == "loud" else "target=auto\nmixer=idle playing=none"
        if tool == "gx_bringup":
            return False, self.jobs()
        if tool == "screen":
            self.shots += 1
            globals()["png"](png, 1 if self.bad == "static" else self.shots)
            return False, "ok"
        if tool == "output":
            return False, self.game_says()
        if tool == "stop":
            return False, "stopped"
        if tool == "run":
            return False, self.program(args)
        raise AssertionError(tool)

    def jobs(self):
        self.reads += 1
        if self.bad == "no-counters":
            return "gx_bringup: REFUSED\nno backend on the render node"
        if self.playing:
            self.signaled += 500
        faults = 1 if self.bad == "fault" and self.reads > 1 else 0
        head = "FAIL" if self.bad == "picture-moved" and self.reads > 2 else "PASS"
        device = "LOST" if self.bad == "lost" and self.reads > 1 else "up"
        return (f"gx_bringup: {head}\nbackend jobs: gfx_0.0.0: emitted {self.signaled}, signaled {self.signaled}, 0 on the ring, rptr 0x0, wptr 0x0\n"
                f"backend jobs: {self.signaled} ended, 0 queued, {faults} VM faults, 0 queues rebuilt after a fault, 0 hangs, reset counter 0, device {device}\n")

    def game_says(self):
        device = "llvmpipe (LLVM 23, 256 bits)" if self.bad == "cpu" else quake_check.RADV_NAME
        # Buffered stdout: the game's lines come when it ends.
        if not self.closed:
            return ""
        text = f"Device: {device}\n{W}x{H if self.bad != 'small' else H - 1}x24 60Hz fullscreen\n"
        return text + ("ended: signal 11\n" if self.bad == "crash" else "ended: exit 0\n")

    def program(self, args):
        path, a = args["path"], args["args"]
        if path == "/bin/bitscomp":
            words = a[3:]
            if words == ["stats"]:
                self.commits += 300 if self.playing and not self.closed and self.bad != "still" else 3
                failed = 4 if self.bad == "refused" and self.playing else 0
                return f"frames={self.commits} commits={self.commits} dmabuf_imported={2 if self.playing else 0} dmabuf_failed={failed} output={W}x{H} build=gpu renderer=gles\nended: exit 0"
            if words == ["list"]:
                if self.playing and not self.closed and self.bad != "no-window":
                    fs = "" if self.bad == "windowed" else " fullscreen"
                    return f'tile=0 page=0/1{fs} visible=true focused=true frame=0,0 {W}x{H} id=7 app_id=vkquake title="vkQuake 1.37.0" pid=77 hung=false ws=1\nend\nended: exit 0'
                # A window of someone else, before and after: never this game's.
                return 'tile=1 page=0/1 visible=true focused=false frame=0,0 9x9 id=3 app_id=foot title="x" pid=5 hung=false ws=1\nend\nended: exit 0'
            if words[0].startswith("close"):
                assert words == ["close 7"], words
                if self.bad != "no-close":
                    self.closed = True
                return "ok\nended: exit 0"
            raise AssertionError(words)
        if a[1:2] == ["--check"]:
            node = "" if self.bad == "no-node" else "renderD128"
            return f"vkquake: libraries: 8 programs and libraries resolve inside x\nvkquake: render nodes: {node}\nvkquake: desktop: /run/user/1000/wayland-bits-0\nended: exit 0"
        if a[:1] == [quake_check.LAUNCHER]:
            assert "-nosound" in a, "the game must start with -nosound"
            self.playing = True
            return "pid 77 /bin/sh is running (call 5)"
        return "ended: exit 0"


def run(bad, out):
    a = argparse.Namespace(target="x", tls=None, token_file="/nonexistent", out=out, socket="s", xdg="/x")
    f = Fake(a, bad)
    fails = []
    real_step = f.step

    def step(name, ok, detail=""):
        if not ok:
            fails.append(name)
        return real_step(name, ok, detail)

    f.step = step
    return f, f.main(), fails


def real_formats():
    """The kernel's own format strings, read from the source, through the run's parser: a change there turns this gate red
    instead of the first step on the Mini-PC."""
    with open(os.path.join(ROOT, "crates/drivers/amdgpu/src/hw/lost.rs")) as f:
        lines = re.findall(r'"(backend jobs: [^"]*\{\}[^"]*)"', f.read())
    with open(os.path.join(ROOT, "crates/drivers/amdgpu/src/psplive.rs")) as f:
        heads = re.findall(r'"(gx_bringup: PASS)"', f.read())
    text = "\n".join(heads + [re.sub(r"\{[^{}]*\}", "7", x) for x in lines])
    j = quake_check.jobs(text)
    want = {"head": "PASS", "signaled": {"7": 7}, "ended": 7, "faults": 7, "rebuilt": 7, "hangs": 7, "resets": 7, "device": "7"}
    if j != want:
        print(f"FAIL hw/lost.rs and quake_check.jobs() no longer agree: {lines} gave {j}")
        return 1
    return 0


def main():
    bad = real_formats()
    with tempfile.TemporaryDirectory() as out:
        f, code, fails = run("", out)
        if code or fails:
            print(f"FAIL the good run: exit {code}, {fails}")
            bad += 1
        # A refused hdmi-off changes nothing: the game is silent by -nosound.
        f, code, fails = run("hdmi-refused", out)
        if code or fails:
            print(f"FAIL hdmi-off refused must not fail the run: {fails}")
            bad += 1
        first = [c[0] for c in f.calls if c[0] in ("snd", "gx_bringup")][:1]
        if first != ["snd"]:
            print(f"FAIL the sound must be off before the game: {first}")
            bad += 1
        for what, step in (("ktest", "ktest"), ("no-node", "tree"), ("no-window", "window"), ("windowed", "window"), ("cpu", "device"),
                           ("loud", "silent"), ("still", "presenting"), ("refused", "presenting"), ("static", "picture"), ("small", "mode"),
                           ("fault", "chip-jobs"), ("lost", "chip-jobs"), ("no-counters", "chip-before"), ("no-close", "close"), ("crash", "close"),
                           ("picture-moved", "gx-d9")):
            f, code, fails = run(what, out)
            if code != 1 or step not in fails:
                print(f"FAIL {what}: exit {code}, failed steps {fails}, wanted {step} among them")
                bad += 1
        # The game must not stay behind, and when the link is gone the run says so, keeps the output and fails on the link.
        f, code, fails = run("no-close", out)
        if not any(c[0] == "stop" for c in f.calls):
            print("FAIL a game that did not close was not stopped")
            bad += 1
        os.remove(os.path.join(out, "game.txt"))
        f, code, fails = run("link-drops", out)
        with open(os.path.join(out, "game.txt")) as g:
            kept = "pid 77" in g.read()
        if code != 1 or "mcp" not in fails or not kept or not any("may still run" in n for n in f.notes):
            print(f"FAIL the link dropped during the game: exit {code}, failed {fails}, output kept {kept}, notes {f.notes}")
            bad += 1
    a = quake_check.jobs("gx_bringup: PASS\nnothing else")
    good, _ = quake_check.jobs_verdict(a, a)
    if good:
        print("FAIL an answer without counters passed")
        bad += 1
    print(f"test_quake_check: {'green' if not bad else f'{bad} wrong'}")
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main())
