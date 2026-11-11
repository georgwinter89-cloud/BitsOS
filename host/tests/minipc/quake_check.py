#!/usr/bin/env python3
"""GM-8: milestone 3 over MCP (quake.sh): vkQuake fullscreen in the running desktop on the real 780M, without a sound. One
PASS or FAIL per step, exit 0 only when all passed; a step that cannot do its work is a FAIL (Georg 30.10.).

Usage: quake_check.py <host[:port]> (--tls <key store> | --plain) --token-file <file> [--out <dir>] [--socket NAME] [--xdg DIR]

  mcp          MCP answers, the run's folder is made
  ktest        `ul.pkg.vkquake` with `ul.pkg.vkquake_sha`: the tree on the stick is the one the list names, its server starts
  tree         `vkquake --check`: libraries resolve in the trees, RADV's manifest, a render node, the desktop's socket
  sound-off    `snd` hdmi-off first; the answer is written down, refused or not (no display side, e.g.): the game itself runs
               with -nosound, so nothing can be audible either way
  chip-before  `gx_bringup until=jobs` (reads only) answers, device up
  window       the game's own new window (by its pid; app_id vkquake) is fullscreen, visible, focused, answering
  silent       with the game running the mixer has no client
  presenting   bitscomp's commits rise beyond the desktop's own, it took RADV's dma-bufs and refused none
  picture      two MCP `screen` pictures differ and show a scene (the last pair is kept in the output folder to look at)
  chip-jobs    between the two reads the rings signaled jobs, no fault, no hang, no reset, device up
  close        `ctl close ID`: the game ends with exit 0 and its window is gone
  device       the game's own line (its stdout comes at its end, buffered): it ran on RADV
  mode         the game's mode is the desktop's own output size, the same before and during the game, as is the screenshot's
  gx-d9        `gx_bringup until=jobs` after the game: PASS (the picture as at the node's start, no display write of its own),
               the output size unchanged, the desktop still commits"""
import argparse
import os
import re
import sys
import time

sys.dont_write_bytecode = True
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import vk_check  # noqa: E402
from vk_check import Check, Unreachable, crop_hash, ended, find_window, read_png, stats  # noqa: E402

LAUNCHER = "/boot/BITSOS2/bin/vkquake"
WORK = "/tmp/gm-quake"
RADV_NAME = vk_check.RADV_NAME
# Bounds of a stuck program, not of the hardware: the first start reads its libraries and 200 MB of data from the stick
# and compiles shaders.
WINDOW_SECS = 300
RUN_SECS = 300
CLOSE_SECS = 60
# The map may still load when the window is there: the picture moves within this bound.
PICTURE_SECS = 120
WATCH_SECS = 3
# Beyond the desktop's own in WATCH_SECS: a third of what 60 Hz would give, no timing is judged.
MIN_COMMITS = 60
# Whole-screen colours (of every eighth pixel) that say a scene, not a flat or black picture.
MIN_COLOURS = 16
MIN_JOBS = 10
GAME_ARGS = ["-nosound", "-noudp", "+map", "start", "+vid_describecurrentmode"]


def jobs(text):
    """The counters of a `gx_bringup until=jobs` answer: {"head", "signaled": {ring: n}, "ended", "faults", "rebuilt", "hangs", "resets", "device"}; a
    value missing from the answer is None, so a changed answer fails its step instead of passing unread."""
    head = re.search(r"gx_bringup: (\w+)", text)
    out = {"head": head.group(1) if head else None, "signaled": {}}
    for m in re.finditer(r"backend jobs: (\S+): emitted \d+, signaled (\d+)", text):
        out["signaled"][m.group(1)] = int(m.group(2))
    m = re.search(r"backend jobs: (\d+) ended, \d+ queued, (\d+) VM faults, (\d+) queues rebuilt after a fault, (\d+) hangs, reset counter (\d+), device (\w+)", text)
    keys = ("ended", "faults", "rebuilt", "hangs", "resets", "device")
    for i, k in enumerate(keys):
        out[k] = None if not m else (m.group(i + 1) if k == "device" else int(m.group(i + 1)))
    return out


def jobs_verdict(a, b):
    """(good, detail) of two reads around the game."""
    if a["ended"] is None or b["ended"] is None or not b["signaled"]:
        return False, f"the answer has no job counters: {a}, {b}"
    ran = {r: b["signaled"][r] - a["signaled"].get(r, 0) for r in b["signaled"]}
    done = sum(ran.values())
    bad = [f"{k} {a[k]} -> {b[k]}" for k in ("faults", "rebuilt", "hangs", "resets") if a[k] != b[k]]
    if b["device"] != "up":
        bad.append(f"device {b['device']}")
    good = done >= MIN_JOBS and not bad
    return good, f"jobs ended {a['ended']} -> {b['ended']}, signaled per ring {ran}" + ("; " + ", ".join(bad) if bad else "") + ("" if done >= MIN_JOBS else f"; fewer than {MIN_JOBS}")


def line_state(line):
    """The words of a `ctl list` line this run judges."""
    return {"fullscreen": " fullscreen " in line, "visible": "visible=true" in line, "focused": "focused=true" in line,
            "hung": "hung=true" in line, "app": re.search(r"app_id=(\S*)", line).group(1) if "app_id=" in line else None}


def game_mode(text):
    """(w, h) of the game's answer to vid_describecurrentmode ("1280x800x24 60Hz fullscreen"), else None."""
    m = re.search(r"\b(\d+)x(\d+)x\d+ [\d.]+Hz fullscreen", text)
    return (int(m.group(1)), int(m.group(2))) if m else None


def size_of(s):
    m = re.fullmatch(r"(\d+)x(\d+)", s or "")
    return (int(m.group(1)), int(m.group(2))) if m else None


class Quake(Check):
    def __init__(self, a):
        a.profile, a.ref_dir = "real", None
        super().__init__(a)
        self.env = dict(self.env, HOME=f"{WORK}/home", VKQ_HOME=f"{WORK}/home")
        self.game, self.game_text, self.wid, self.pid, self.modes = None, "", None, None, None

    def run_ktests(self):
        _, text = self.mcp("ktest", {"filter": "ul.pkg.vkquake", "timeout_ms": 900000}, wait=1000)
        self.save("ktest-ul.pkg.vkquake.txt", text)
        m = re.search(r"passed=(\d+) failed=(\d+) skipped=(\d+) refused=(\d+)", text)
        good = m and int(m.group(1)) >= 2 and m.group(2) == m.group(3) == m.group(4) == "0"
        self.step("ktest", good, m.group(0) if m else text[-300:])

    def tree_check(self):
        kind, code, text = self.run("/bin/sh", [LAUNCHER, "--check"], self.env, RUN_SECS)
        self.save("check.txt", text)
        good = (kind, code) == ("exit", 0) and "resolve inside" in text and re.search(r"render nodes:.*\brenderD128\b", text) is not None and "desktop: /" in text
        self.step("tree", good, "libraries resolve, renderD128 and the desktop are there" if good else text[-600:])

    def sound_off(self):
        err, text = self.mcp("snd", {"output": "hdmi-off"})
        self.save("snd-off.txt", text)
        self.step("sound-off", True, ("refused, " if err else "") + (text.strip().splitlines() or ["no answer"])[0] + "; the game runs with -nosound either way")

    def game_output(self):
        self.game_text += "\n" + self.mcp("output", {"call": self.game})[1]
        return self.game_text

    def play(self):
        before = self.windows()
        s0 = stats(self.ctl("stats"))
        self.note(f"bitscomp: build={s0.get('build')} renderer={s0.get('renderer')} output={s0.get('output')}")
        if not s0.get("commits_seen"):
            self.step("window", False, "bitscomp does not answer `ctl stats` on this socket: is the desktop running?")
            return
        time.sleep(WATCH_SECS)
        idle = stats(self.ctl("stats"))["commits"] - s0["commits"]
        err, text = self.mcp("gx_bringup", {"until": "jobs"})
        self.save("jobs-before.txt", text)
        j0 = jobs(text)
        self.step("chip-before", j0["device"] == "up", f"head {j0['head']}, device {j0['device']}, ended {j0['ended']}")
        _, text = self.mcp("run", {"path": "/bin/sh", "args": [LAUNCHER, *GAME_ARGS], "env": self.env, "timeout_ms": 1500}, wait=60)
        m = re.search(r"\(call (\d+)\)", text)
        if not m:
            self.step("window", False, f"the game ended at once: {text[-800:]}")
            return
        self.game, self.game_text = int(m.group(1)), text
        pid = re.search(r"pid (\d+) \S+ is running", text)
        self.pid = int(pid.group(1)) if pid else None
        try:
            line = self.await_window(before)
            if line:
                self.judge(line, s0, idle, j0)
                self.close()
        finally:
            self.stop_game()

    def stop_game(self):
        """The game does not stay on the node and its output is kept, also when the link is what failed: that error stays the
        one main() reports, and the run says the game may still run."""
        try:
            if not ended(self.game_output()):
                self.mcp("stop", {"call": self.game})
        except Unreachable as e:
            self.note(f"game call {self.game} may still run on the node: {e}")
        self.save("game.txt", self.game_text)

    def await_window(self, before):
        """The game's window once fullscreen (the game asks right after its first configure), or a FAIL."""
        line, end = None, time.time() + WINDOW_SECS
        while time.time() < end:
            text = self.game_output()
            if ended(text):
                self.step("window", False, f"the game ended before its window: {text[-1000:]}")
                return None
            line = find_window(self.ctl("list"), before, self.pid)
            if line and " fullscreen " in line:
                break
            time.sleep(2)
        if not line:
            s = stats(self.ctl("stats"))
            self.step("window", False, f"no window of the game (pid {self.pid}) within {WINDOW_SECS} s; renderer={s.get('renderer')}, dma-bufs refused {s['dmabuf_failed']} (refused: WY-17b or CONFIG bitscomp=gles); {self.game_text[-800:]}")
            return None
        st = line_state(line)
        good = st["fullscreen"] and st["visible"] and st["focused"] and not st["hung"] and st["app"] == "vkquake"
        self.step("window", good, line)
        return line

    def judge(self, line, s0, idle, j0):
        self.wid = re.search(r" id=(\d+) app_id=", line).group(1)
        err, text = self.mcp("snd", {})
        self.save("snd-during.txt", text)
        clients = re.search(r"\bclients=(\d+)", text)
        quiet = "mixer=idle" in text or (clients is not None and clients.group(1) == "0")
        self.step("silent", not err and quiet, (text.strip().splitlines() or ["no answer"])[1 if len(text.strip().splitlines()) > 1 else 0])

        s1 = stats(self.ctl("stats"))
        time.sleep(WATCH_SECS)
        s2 = stats(self.ctl("stats"))
        commits = s2["commits"] - s1["commits"]
        need = idle + MIN_COMMITS
        imported, failed = s2["dmabuf_imported"] - s0["dmabuf_imported"], s2["dmabuf_failed"] - s0["dmabuf_failed"]
        self.step("presenting", commits >= need and imported >= 1 and failed == 0,
                  f"{commits} commits in {WATCH_SECS} s (desktop alone {idle}, needed {need}), frames {s2['frames'] - s1['frames']}, dma-bufs taken {imported}, refused {failed}, renderer {s2.get('renderer')}")

        end, turn, last, size0, good = time.time() + PICTURE_SECS, 0, None, None, False
        while not good:
            path = os.path.join(self.a.out, f"quake-{turn % 2}.png")
            err, text = self.mcp("screen", {}, png=path)
            if err or not os.path.exists(path):
                self.step("picture", False, f"MCP screen: {text[-300:]}")
                return
            size0 = read_png(path)[:2]
            now = crop_hash(path, (0, 0, 1 << 20, 1 << 20))
            good = last is not None and now[0] != last[0] and now[1] >= MIN_COLOURS
            last, turn = now, turn + 1
            if time.time() > end:
                break
            if not good:
                time.sleep(2)
        self.step("picture", good, f"{size0[0]}x{size0[1]}: {'moves' if good else 'stands still or is flat'} after {turn} pictures, {last[1]} colours (quake-0.png, quake-1.png)")
        self.modes = (size_of(s0.get("output")), size_of(s2.get("output")), size0)

        err, text = self.mcp("gx_bringup", {"until": "jobs"})
        self.save("jobs-during.txt", text)
        good, detail = jobs_verdict(j0, jobs(text))
        self.step("chip-jobs", good, detail)

    def close(self):
        self.ctl(f"close {self.wid}")
        end = time.time() + CLOSE_SECS
        while time.time() < end and not ended(self.game_text):
            self.game_output()
            time.sleep(1)
        e = ended(self.game_text)
        gone = not re.search(rf" id={self.wid} app_id=", self.ctl("list"))
        self.step("close", e == ("exit", 0) and gone, f"the game ended {e}, window gone: {gone}")
        device = re.search(r"^Device: (.*)$", self.game_text, re.M)
        device = device.group(1).strip() if device else None
        self.step("device", device is not None and RADV_NAME in device, f"the game names {device}")
        game = game_mode(self.game_text)
        out0, out2, size0 = self.modes or (None, None, None)
        self.step("mode", game is not None and game == out0 == out2 == size0, f"the game {game}, the output before {out0}, during {out2}, the screenshot {size0}")
        err, text = self.mcp("gx_bringup", {"until": "jobs"})
        self.save("jobs-after.txt", text)
        j = jobs(text)
        s = stats(self.ctl("stats"))
        time.sleep(WATCH_SECS)
        s2 = stats(self.ctl("stats"))
        self.step("gx-d9", j["head"] == "PASS" and j["device"] == "up" and s2["frames"] >= s["frames"],
                  f"gx_bringup jobs: head {j['head']}, device {j['device']}, output {s2.get('output')}; lines in jobs-after.txt")

    def main(self):
        steps = (self.run_ktests, self.tree_check, self.sound_off, self.play)
        try:
            good, text = self.sh(f"rm -rf {WORK}; mkdir -p {WORK}/home && chmod 777 {WORK} {WORK}/home")
            if self.step("mcp", good, self.a.target if good else f"{self.a.target}: {text[-300:]}"):
                for f in steps:
                    try:
                        f()
                    except (OSError, ValueError, KeyError, IndexError, TypeError) as e:
                        self.step(f.__name__, False, f"{type(e).__name__}: {e}")
        except Unreachable as e:
            self.step("mcp", False, str(e))
        finally:
            try:
                self.sh(f"rm -rf {WORK}")
            except Unreachable as e:
                self.note(f"{WORK} stays on the node, the next run removes it: {e}")
        print(f"quake: {self.passed} PASS, {self.failed} FAIL (output in {self.a.out})", flush=True)
        return 1 if self.failed else 0


def args():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("target")
    tls = p.add_mutually_exclusive_group(required=True)
    tls.add_argument("--tls", help="key store for mutual TLS (the Mini-PC)")
    tls.add_argument("--plain", action="store_true", help="plain HTTP (the made-up answers of the logic's test only)")
    p.add_argument("--token-file", required=True)
    p.add_argument("--out", default=os.path.join(vk_check.ROOT, "target", "minipc", "quake"))
    p.add_argument("--socket", default="wayland-bits-0")
    p.add_argument("--env", action="append", metavar="KEY=VALUE", help="one more variable for the programs under test")
    p.add_argument("--xdg", default="/run/user/1000")
    return p.parse_args()


if __name__ == "__main__":
    sys.exit(Quake(args()).main())
