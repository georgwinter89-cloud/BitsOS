#!/usr/bin/env python3
"""WY-3b over MCP (x11gpu.sh): X11 programs in the running desktop on the real 780M, through Xwayland's glamor and DRI3. One
PASS or FAIL per step, exit 0 only when all passed; a step that cannot do its work is a FAIL (Georg 30.10.).

Usage: x11gpu_check.py <host[:port]> (--tls <key store> | --plain) --token-file <file> [--out <dir>] [--socket NAME] [--xdg DIR]

  mcp          MCP answers, the run's folder is made
  tree         the stick's Xwayland tree has glamor, the clients and both `run`s
  desktop      bitscomp answers, the gpu build drawing with gles (a Pixman or llvmpipe desktop offers no DRI3, by design)
  dri3         `xdpyinfo` lists DRI3 and GLX; `ctl xwayland` says glamor on the render node, DRI3 on
  node-only    Xwayland holds the render node and no card node: it can touch no mode (GX-D9)
  chip-before  `gx_bringup until=jobs` (reads only) answers, device up
  renderer     `glxinfo -B`: direct rendering on the Radeon, not llvmpipe
  vkcube-window, glxgears-window    `vkcube --wsi xcb` on RADV and glxgears: the program's own new X11 window, focused
  vkcube-device                     vkcube names the Radeon
  *-presenting bitscomp's commits rise beyond the desktop's own, it took the program's dma-bufs and refused none
  *-picture    the window's pixels change between two MCP `screen` pictures and are not one colour
  *-chip-jobs  between the reads around the program the rings signaled jobs, no fault, no hang, no reset, device up
  *-close      `ctl close ID`: the program ends and its window is gone
  gx-d9        `gx_bringup until=jobs` after both: PASS, device up, the output size unchanged, the desktop still commits"""
import argparse
import os
import re
import sys
import time

sys.dont_write_bytecode = True
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import vk_check  # noqa: E402
from quake_check import jobs, jobs_verdict  # noqa: E402
from vk_check import Check, Unreachable, ended, find_window, stats  # noqa: E402

WORK = "/tmp/wy-x11gpu"
MESA_RUN = vk_check.MESA + "/run"
XW = "/boot/BITSOS2/xwayland"
X_RUN = XW + "/run"
RADV_NAME = vk_check.RADV_NAME
# The renderer string of radeonsi on this chip; llvmpipe, or any other, is what this whole run is against.
GL_NAME = "AMD Radeon 780M"
NODE = "/dev/dri/renderD128"
# Bounds of a stuck program, not of the hardware.
WINDOW_SECS = 120
RUN_SECS = 120
CLOSE_SECS = 30
WATCH_SECS = 3
# Beyond the desktop's own in WATCH_SECS: a third of what 60 Hz would give, no timing is judged.
MIN_COMMITS = 60
PROGRAMS = (
    ("vkcube", MESA_RUN, ["vkcube", "--wsi", "xcb"], {"MESA_VK_DRIVERS": "radeon_icd"}),
    ("glxgears", X_RUN, ["glxgears"], {}),
)


class X11Gpu(Check):
    def __init__(self, a):
        a.profile, a.ref_dir = "real", None
        super().__init__(a)
        self.env = dict(self.env, HOME=f"{WORK}/home", DISPLAY=":0")
        self.output0, self.j0 = None, None

    def tree(self):
        kind, code, text = self.run(f"{XW}/bin/Xwayland", ["-help"], self.env, RUN_SECS)
        self.save("xwayland-help.txt", text)
        glamor = "-glamor" in text
        good, listed = self.sh(f"ls {X_RUN} {MESA_RUN} {XW}/lib/libGLX_mesa.so.0 {XW}/clients/glxgears {XW}/clients/glxinfo {XW}/clients/xdpyinfo {vk_check.MESA}/bin/vkcube")
        self.step("tree", glamor and good, f"Xwayland {kind} {code}, glamor {'built in' if glamor else 'missing'}" + ("" if good else f"; {listed[-400:]}"))

    def desktop(self):
        s = stats(self.ctl("stats"))
        self.output0 = s.get("output")
        good = s.get("commits_seen") and s.get("build") == "gpu" and s.get("renderer") == "gles"
        self.step("desktop", good, f"build={s.get('build')} renderer={s.get('renderer')} output={s.get('output')}; CONFIG bitscomp=gles or auto is what this run needs")
        return good

    def dri3(self):
        kind, code, text = self.run("/bin/sh", [X_RUN, "xdpyinfo"], self.env, RUN_SECS)
        self.save("xdpyinfo.txt", text)
        st = self.ctl("xwayland")
        self.save("ctl-xwayland.txt", st)
        have = [e for e in ("DRI3", "GLX") if re.search(rf"^\s+{e}$", text, re.M)]
        good = (kind, code) == ("exit", 0) and have == ["DRI3", "GLX"] and f"dri3=true glamor={NODE}" in st
        line = next((l.strip() for l in st.splitlines() if l.startswith("xwayland ")), st[-300:])
        self.step("dri3", good, f"xdpyinfo {kind} {code} lists {have}; {line}")
        pid = re.search(r"\bstate=ready pid=(\d+) ", st)
        if not pid:
            self.step("node-only", False, f"no running Xwayland in: {line}")
            return
        ok, fds = self.sh(f"ls -l /proc/{pid.group(1)}/fd")
        self.save("xwayland-fds.txt", fds)
        nodes = sorted(set(re.findall(r"/dev/dri/(\w+)", fds)))
        self.step("node-only", ok and nodes == ["renderD128"], f"Xwayland (pid {pid.group(1)}) holds {nodes}")

    def chip_before(self):
        err, text = self.mcp("gx_bringup", {"until": "jobs"})
        self.save("jobs-before.txt", text)
        self.j0 = jobs(text)
        self.step("chip-before", self.j0["device"] == "up", f"head {self.j0['head']}, device {self.j0['device']}, ended {self.j0['ended']}")

    def renderer(self):
        kind, code, text = self.run("/bin/sh", [X_RUN, "glxinfo", "-B"], self.env, RUN_SECS)
        self.save("glxinfo.txt", text)
        m = re.search(r"OpenGL renderer string: (.*)", text)
        name = m.group(1).strip() if m else None
        good = (kind, code) == ("exit", 0) and "direct rendering: Yes" in text and name is not None and GL_NAME in name
        self.step("renderer", good, f"glxinfo {kind} {code}, {name}")

    def windows(self):
        return {int(m.group(1)) for m in re.finditer(r" id=(\d+) app_id=", self.ctl("list"))}

    def program(self, name, runner, words, extra):
        before = self.windows()
        s0 = stats(self.ctl("stats"))
        time.sleep(WATCH_SECS)
        idle = stats(self.ctl("stats"))["commits"] - s0["commits"]
        err, text = self.mcp("gx_bringup", {"until": "jobs"})
        j0 = jobs(text)
        _, text = self.mcp("run", {"path": "/bin/sh", "args": [runner, *words], "env": dict(self.env, **extra), "timeout_ms": 1500}, wait=60)
        m = re.search(r"\(call (\d+)\)", text)
        if not m:
            self.step(f"{name}-window", False, f"{name} ended at once: {text[-800:]}")
            return
        call, said = int(m.group(1)), [text]
        pid = re.search(r"pid (\d+) \S+ is running", text)
        pid = int(pid.group(1)) if pid else None

        def output():
            said.append(self.mcp("output", {"call": call})[1])
            return "\n".join(said)

        try:
            line, end = None, time.time() + WINDOW_SECS
            while time.time() < end and not line:
                if ended(output()):
                    self.step(f"{name}-window", False, f"{name} ended before its window: {said[-1][-800:]}")
                    return
                line = find_window(self.ctl("list"), before, pid)
                time.sleep(1)
            if not line:
                s = stats(self.ctl("stats"))
                self.step(f"{name}-window", False, f"no window of {name} (pid {pid}) within {WINDOW_SECS} s, dma-bufs refused {s['dmabuf_failed'] - s0['dmabuf_failed']}; {said[-1][-600:]}\n{self.ctl('list')}")
                return
            self.step(f"{name}-window", " x11=0x" in line and "focused=true" in line, line)
            if name == "vkcube":
                sel = re.search(r"Selected GPU \d+: ([^,\n]*)", output())
                device = sel.group(1).strip() if sel else None
                self.step("vkcube-device", device is not None and RADV_NAME in device, f"vkcube names {device}")
            self.judge(name, line, s0, idle, j0)
            self.close(name, line, output)
        finally:
            if not ended(output()):
                self.mcp("stop", {"call": call})
            self.save(f"{name}.txt", "\n".join(said))

    def judge(self, name, line, s0, idle, j0):
        s1 = stats(self.ctl("stats"))
        time.sleep(WATCH_SECS)
        s2 = stats(self.ctl("stats"))
        commits = s2["commits"] - s1["commits"]
        need = idle + MIN_COMMITS
        imported, failed = s2["dmabuf_imported"] - s0["dmabuf_imported"], s2["dmabuf_failed"] - s0["dmabuf_failed"]
        self.step(f"{name}-presenting", commits >= need and imported >= 1 and failed == 0,
                  f"{commits} commits in {WATCH_SECS} s (desktop alone {idle}, needed {need}), dma-bufs taken {imported}, refused {failed}")
        rect = tuple(int(x) for x in re.search(r"frame=(-?\d+),(-?\d+) (\d+)x(\d+)", line).groups())
        seen = []
        for i in range(2):
            path = os.path.join(self.a.out, f"{name}-{i}.png")
            err, text = self.mcp("screen", {}, png=path)
            if err or not os.path.exists(path):
                self.step(f"{name}-picture", False, f"MCP screen: {text[-300:]}")
                break
            seen.append(vk_check.crop_hash(path, rect))
            time.sleep(1)
        if len(seen) == 2:
            (h0, colours), (h1, _) = seen
            self.step(f"{name}-picture", h0 != h1 and colours > 1, f"window {rect[2]}x{rect[3]}: {'changes' if h0 != h1 else 'stands still'}, {colours} colours ({name}-0.png, {name}-1.png)")
        err, text = self.mcp("gx_bringup", {"until": "jobs"})
        self.save(f"jobs-{name}.txt", text)
        good, detail = jobs_verdict(j0, jobs(text))
        self.step(f"{name}-chip-jobs", good, detail)

    def close(self, name, line, output):
        wid = re.search(r" id=(\d+) app_id=", line).group(1)
        self.ctl(f"close {wid}")
        end = time.time() + CLOSE_SECS
        while time.time() < end and not ended(output()):
            time.sleep(1)
        e = ended(output())
        gone = not re.search(rf" id={wid} app_id=", self.ctl("list"))
        # glxgears answers WM_DELETE_WINDOW by ending; what a closed X client exits with is its own affair.
        self.step(f"{name}-close", e is not None and gone, f"{name} ended {e}, window gone: {gone}")

    def gx_d9(self):
        err, text = self.mcp("gx_bringup", {"until": "jobs"})
        self.save("jobs-after.txt", text)
        j = jobs(text)
        s = stats(self.ctl("stats"))
        time.sleep(WATCH_SECS)
        s2 = stats(self.ctl("stats"))
        good = j["head"] == "PASS" and j["device"] == "up" and s2["output"] == self.output0 and s2["frames"] >= s["frames"]
        self.step("gx-d9", good, f"gx_bringup jobs: head {j['head']}, device {j['device']}, output {self.output0} -> {s2['output']}; lines in jobs-after.txt")

    def main(self):
        try:
            good, text = self.sh(f"rm -rf {WORK}; mkdir -p {WORK}/home && chmod 777 {WORK} {WORK}/home")
            if self.step("mcp", good, self.a.target if good else f"{self.a.target}: {text[-300:]}"):
                steps = [self.tree, self.desktop, self.dri3, self.chip_before, self.renderer]
                steps += [(lambda p=p: self.program(*p)) for p in PROGRAMS]
                steps.append(self.gx_d9)
                for f in steps:
                    try:
                        f()
                    except (OSError, ValueError, KeyError, IndexError, TypeError, AttributeError) as e:
                        self.step(getattr(f, "__name__", "program"), False, f"{type(e).__name__}: {e}")
        except Unreachable as e:
            self.step("mcp", False, str(e))
        finally:
            try:
                self.sh(f"rm -rf {WORK}")
            except Unreachable as e:
                self.note(f"{WORK} stays on the node, the next run removes it: {e}")
        print(f"x11gpu: {self.passed} PASS, {self.failed} FAIL (output in {self.a.out})", flush=True)
        return 1 if self.failed else 0


def args():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("target")
    tls = p.add_mutually_exclusive_group(required=True)
    tls.add_argument("--tls", help="key store for mutual TLS (the Mini-PC)")
    tls.add_argument("--plain", action="store_true", help="plain HTTP (the kernel's own server, as in QEMU: 47-v2-xwayland-gpu.sh)")
    p.add_argument("--token-file", required=True)
    p.add_argument("--out", default=os.path.join(vk_check.ROOT, "target", "minipc", "x11gpu"))
    p.add_argument("--socket", default="wayland-bits-0")
    p.add_argument("--env", action="append", metavar="KEY=VALUE", help="one more variable for the programs under test")
    p.add_argument("--xdg", default="/run/user/1000")
    return p.parse_args()


if __name__ == "__main__":
    sys.exit(X11Gpu(args()).main())
