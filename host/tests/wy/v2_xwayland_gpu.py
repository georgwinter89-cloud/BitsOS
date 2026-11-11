#!/usr/bin/env python3
"""WY-3b scenario client (47-v2-xwayland-gpu.sh): X11 programs on the fake 780M's render node through glamor and DRI3.

Usage: v2_xwayland_gpu.py <mcp host port> <token> <qmp socket> <run dir>
Exit 0 = green. Timing is never judged (QEMU), and the fake draws nothing: only what was asked and what arrived."""
import os
import re
import subprocess
import sys
import time

sys.dont_write_bytecode = True  # no __pycache__ in the tree
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
sys.argv = sys.argv[:5] + ["-"]
import v2_check as v  # noqa: E402
from v2_check import call, connect, need, shell  # noqa: E402

SOCK = "wayland-1"
HOME = "/tmp/wy3/home"
XENV = dict(v.ENV, DISPLAY=":0", HOME=HOME, WAYLAND_DISPLAY=SOCK)
NODE = "/dev/dri/renderD128"
MESA_RUN = "/boot/BITSOS2/mesa/run"
X_RUN = "/boot/BITSOS2/xwayland/run"
LD = "/boot/BITSOS2/mesa/lib/ld-linux-x86-64.so.2"
LIBS = "/boot/BITSOS2/xwayland/test/lib:/boot/BITSOS2/xwayland/lib:/boot/BITSOS2/mesa/lib"
BIN = "/boot/BITSOS2/xwayland/test/bin/"
JOBS = []  # what they said goes into the run directory


def xrun(c, *words, timeout_ms=30000, **env):
    """A program of the Xwayland tree (`run`) to its end."""
    _, text, _ = call(c, "run", {"path": "/bin/sh", "args": [X_RUN, *words], "env": dict(XENV, **env), "timeout_ms": timeout_ms})
    return text


class XJob(v.Job):
    """A program that keeps running, by the wrapper named."""

    def __init__(self, c, run, *words, **env):
        self.c = c
        _, text, _ = call(c, "run", {"path": "/bin/sh", "args": [run, *words], "env": dict(XENV, **env), "timeout_ms": 1500})
        m = re.search(r"\(call (\d+)\)", text)
        need(m is not None, f"{' '.join(words)} keeps running", text)
        self.n, self.text = int(m.group(1)), text
        JOBS.append(self)


def ctl(c, *words):
    _, text, _ = call(c, "run", {"path": "/bin/bitscomp", "args": ["ctl", "--socket", SOCK, *words], "env": v.ENV, "timeout_ms": 20000})
    return text


def stats(c):
    return {k: int(x) for k, x in re.findall(r"\b(frames|commits|dmabuf_imported|dmabuf_failed)=(\d+)", ctl(c, "stats"))}


def x11_window(c, what, secs=60):
    end = time.time() + secs
    while True:
        line = next((l for l in ctl(c, "list").splitlines() if " x11=0x" in l), None)
        if line:
            print(f"ok: {what} is a window of the desk: {line.strip()}", flush=True)
            return line
        if time.time() > end:
            raise SystemExit(f"RED: {what}: no X11 window\n{ctl(c, 'list')}\n{ctl(c, 'xwayland')}")
        time.sleep(0.5)


def presents(c, what, job, started, secs=60):
    """Commits rise while the program runs, it handed over dma-bufs since `started` (its stats before it ran), none refused."""
    before = stats(c)
    end = time.time() + secs
    while True:
        now = stats(c)
        if now["commits"] > before["commits"] + 5 and now["dmabuf_imported"] > started["dmabuf_imported"]:
            break
        if time.time() > end:
            raise SystemExit(f"RED: {what}: its frames do not arrive as dma-bufs: {before} -> {now}\n{job.poll()[-2000:]}")
        time.sleep(1)
    need(now["dmabuf_failed"] == 0, f"{what}: no dma-buf refused", str(now))
    print(f"ok: {what} presents through dma-bufs: {before} -> {now}", flush=True)


def close_all(c, job, what):
    ctl(c, "close")
    end = time.time() + 30
    while " x11=0x" in ctl(c, "list"):
        if time.time() > end:
            raise SystemExit(f"RED: {what} did not close\n{ctl(c, 'list')}\n{job.poll()[-1500:]}")
        time.sleep(0.5)
    print(f"ok: {what} closed", flush=True)


def start_comp(c, *extra):
    # The gpu build itself: the hand-over's check reads its frame back, and the fake draws nothing.
    comp = v.Job(c, v.GPU_EXE, ["--renderer", "gles", "--socket", SOCK, "--fixed-status", *extra], sock=SOCK)
    JOBS.append(comp)
    comp.wait_for(rf"WAYLAND_DISPLAY={SOCK} .*backend=fbdev size=1280x800", "bitscomp started on fbdev", secs=120)
    comp.wait_for(r"xwayland on demand: DISPLAY=:0 \(/boot/BITSOS2/xwayland/bin/Xwayland\)", "bitscomp holds X display :0")
    return comp


def stop_comp(c, comp):
    ctl(c, "quit")
    end = time.time() + 20
    while not re.search(r"\(ended|ended:", comp.poll()):
        if time.time() > end:
            break
        time.sleep(0.5)
    need("PANIC" not in comp.poll(), "no panic in bitscomp's output")


def main():
    try:
        run()
    finally:
        for i, job in enumerate(JOBS):
            with open(os.path.join(v.RUN, f"job-{i}.txt"), "w") as f:
                f.write(job.text)


def run():
    c = connect(300)
    shell(c, f"mkdir -p {HOME} /tmp/wy3 && chmod 777 /tmp/wy3")
    t = shell(c, f"ls -l {NODE}")
    need("renderD128" in t, "the fake's render node is up", t)

    comp = start_comp(c)
    m = re.search(r"renderer gles: (GL_RENDERER=[^;\n]*)", comp.text)
    need(m is not None and "llvmpipe" not in m.group(1), "bitscomp draws on the node (bitscomp_gl=hardware)", comp.text[-2000:])
    print("gles:", m.group(1), flush=True)
    comp.wait_for(r"main device 226:128 \(/dev/dri/renderD128\)", "linux-dmabuf names the render node")
    need("state=idle" in ctl(c, "xwayland"), "Xwayland is not started before an X11 client comes", ctl(c, "xwayland"))

    t = xrun(c, "xdpyinfo")
    comp.wait_for(rf"this start with glamor on {NODE}", "the first X11 client starts Xwayland with glamor")
    comp.wait_for(r"xwm: DRI3 is offered", "the window manager sees DRI3")
    need("    DRI3\n" in t and "    GLX\n" in t and "X.Org version: 24.1.14" in t, "xdpyinfo lists DRI3 and GLX", t[-3000:])
    st = ctl(c, "xwayland")
    need(f"dri3=true glamor={NODE}" in st, "ctl xwayland says glamor and DRI3", st)
    print(st.strip(), flush=True)

    started = stats(c)
    vk = XJob(c, MESA_RUN, "vkcube", "--wsi", "xcb", MESA_VK_DRIVERS="radeon_icd")
    x11_window(c, "vkcube --wsi xcb")
    presents(c, "vkcube --wsi xcb", vk, started)
    need("RADV" in vk.poll() or "780M" in vk.text, "vkcube runs on RADV", vk.text[-1500:])
    close_all(c, vk, "vkcube")

    t = xrun(c, "glxinfo", "-B")
    print("\n".join(l for l in t.splitlines() if re.search(r"direct rendering|OpenGL renderer|OpenGL core profile version", l)), flush=True)
    need("direct rendering: Yes" in t and re.search(r"OpenGL renderer string: .*(Radeon|radeonsi)", t) is not None,
         "glxinfo: direct rendering on the Radeon, not llvmpipe", t[-2500:])

    started = stats(c)
    gears = XJob(c, X_RUN, "glxgears")
    x11_window(c, "glxgears")
    presents(c, "glxgears", gears, started)
    close_all(c, gears, "glxgears")

    st = ctl(c, "xwayland")
    need(re.search(r"state=ready pid=\d+ starts=1 ", st) is not None, "one Xwayland served them all", st)
    stop_comp(c, comp)

    # A node lost while Xwayland runs (the file watched in its place goes): it ends, the next client is served in software.
    watched = "/tmp/wy3/node"
    shell(c, f"mkdir -p /tmp/wy3/nodedir; touch /tmp/wy3/nodefile; ln -s /tmp/wy3/nodefile {watched}")
    comp = start_comp(c, "--xwayland-glamor", f"force:{watched}")
    t = xrun(c, "xdpyinfo")
    need("    DRI3\n" in t, "xdpyinfo lists DRI3 with the watched file there", t[-3000:])
    # An answer that is not a lost device (EISDIR, in one rename): Xwayland stays.
    shell(c, f"ln -s /tmp/wy3/nodedir /tmp/wy3/nodenew; mv /tmp/wy3/nodenew {watched}")
    comp.wait_for(rf"the look at {watched} answers .*not a lost device, this Xwayland stays", "an answer that is no loss is named", secs=30)
    st = ctl(c, "xwayland")
    need("state=ready" in st and f"glamor={watched}" in st, "Xwayland stays on an answer that is no loss", st)
    shell(c, f"rm {watched}")
    comp.wait_for(rf"{watched} was lost: .*the next X11 client gets a software one", "the lost node is seen", secs=30)
    end = time.time() + 20
    while "state=idle" not in ctl(c, "xwayland"):
        if time.time() > end:
            raise SystemExit(f"RED: Xwayland still runs on the lost node\n{ctl(c, 'xwayland')}")
        time.sleep(0.5)
    t = xrun(c, "xdpyinfo")
    comp.wait_for(r"this start in software: .*was lost", "the next start is in software")
    need("X.Org version: 24.1.14" in t and "    DRI3\n" not in t, "the next X11 client gets its answer, without DRI3", t[-2000:])
    _, text, _ = call(c, "run", {"path": LD, "args": ["--inhibit-cache", "--library-path", LIBS, BIN + "xeyes"], "env": XENV, "timeout_ms": 1500})
    need("(call " in text, "xeyes keeps running", text)
    x11_window(c, "xeyes in software")
    st = ctl(c, "xwayland")
    need("dri3=false glamor=off" in st, "ctl xwayland says software and why", st)
    print(st.strip(), flush=True)
    stop_comp(c, comp)

    # A node beside a renderer that reads by CPU: its buffers would not map, so Xwayland stays in software.
    comp = start_comp(c, "--renderer", "pixman", "--main-device", "render")
    t = xrun(c, "xdpyinfo")
    comp.wait_for(r"this start in software: the renderer reads by CPU", "under Pixman, with the node named, Xwayland stays in software")
    need("    DRI3\n" not in t, "xdpyinfo lists no DRI3 under Pixman", t[-2000:])
    stop_comp(c, comp)
    minipc_script(c)


def script(c, name):
    """host/tests/minipc/x11gpu.sh against this boot; its answer, the token nowhere in it."""
    root = os.path.abspath(os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "..", ".."))
    token = os.path.join(v.RUN, "token")
    with open(token, "w") as f:
        f.write(sys.argv[2] + "\n")
    r = subprocess.run(["bash", os.path.join(root, "host/tests/minipc/x11gpu.sh"), f"127.0.0.1:{sys.argv[1]}", "--plain", "--token-file", token,
                        "--socket", SOCK, "--xdg", v.ENV["XDG_RUNTIME_DIR"], "--out", os.path.join(v.RUN, name)], capture_output=True, text=True, errors="replace", timeout=900)
    with open(os.path.join(v.RUN, f"{name}.txt"), "w") as f:
        f.write(r.stdout + r.stderr)
    need(sys.argv[2] not in r.stdout + r.stderr, "the token appears in no output of x11gpu.sh")
    print(r.stdout, flush=True)
    return r


def minipc_script(c):
    """host/tests/minipc/x11gpu.sh against this node: every step that needs no drawing and no real jobs must pass, and those that
    do must fail: the fake draws nothing, so a green here would mean the script judges nothing."""
    comp = start_comp(c)
    r = script(c, "x11gpu")
    done = {name: how for how, name in re.findall(r"^(PASS|FAIL) ([\w-]+)", r.stdout, re.M)}
    # Must fail here, and only these: no job counters on the fake. Its pictures say nothing either way (the fake draws
    # nothing, what `screen` shows is what the last renderer left): their steps must run, their verdict is the chip's.
    fail = {"chip-before", "vkcube-chip-jobs", "glxgears-chip-jobs", "gx-d9"}
    free = {"vkcube-picture", "glxgears-picture"}
    want = {"mcp", "tree", "desktop", "dri3", "node-only", "chip-before", "renderer", "vkcube-window", "vkcube-device", "vkcube-presenting", "vkcube-picture",
            "vkcube-chip-jobs", "vkcube-close", "glxgears-window", "glxgears-presenting", "glxgears-picture", "glxgears-chip-jobs", "glxgears-close", "gx-d9"}
    need(set(done) == want, "x11gpu.sh runs every step", f"{sorted(set(done) ^ want)}\n{r.stdout[-2500:]}")
    wrong = sorted(k for k, o in done.items() if k not in free and (o == "FAIL") != (k in fail))
    need(not wrong, "x11gpu.sh against the fake: only the steps that need the chip fail", f"{wrong}\n{r.stdout[-2500:]}")
    need(r.returncode == 1, "x11gpu.sh's exit says so", str(r.returncode))
    stop_comp(c, comp)

    # The same script on a desktop that cannot offer DRI3 must say so.
    comp = start_comp(c, "--renderer", "pixman")
    r = script(c, "x11gpu-pixman")
    done = {name: how for how, name in re.findall(r"^(PASS|FAIL) ([\w-]+)", r.stdout, re.M)}
    wrong = [k for k in ("desktop", "dri3", "renderer", "vkcube-window", "glxgears-window") if done.get(k) != "FAIL"]
    need(not wrong and r.returncode == 1, "x11gpu.sh on a Pixman desktop fails at the desktop, DRI3, the renderer and both windows", f"{wrong}\n{r.stdout[-2500:]}")
    stop_comp(c, comp)


if __name__ == "__main__":
    main()
