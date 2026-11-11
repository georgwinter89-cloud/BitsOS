#!/usr/bin/env python3
"""WY-17b scenario client (42-v2-radv-window.sh): RADV's window buffers and a Pixman bitscomp on the fake 780M, over MCP.

Usage: v2_radv_window.py <mcp host port> <token> <qmp socket> <run dir>
Exit 0 = green. The fake draws what it can: this judges what bitscomp took, not the picture (vk.sh on the Mini-PC does)."""
import os
import re
import sys
import time

sys.dont_write_bytecode = True  # no __pycache__ in the tree
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
sys.argv = sys.argv[:5] + ["-"]
import v2_check as v  # noqa: E402
from v2_check import Job, call, connect, need  # noqa: E402

MESA_RUN = "/boot/BITSOS2/mesa/run"
HOME = "/tmp/wyr/home"
RADV = {"MESA_VK_DRIVERS": "radeon_icd", "HOME": HOME}


def ctl(c, sock, *words):
    _, text, _ = call(c, "run", {"path": "/bin/bitscomp", "args": ["ctl", "--socket", sock, *words], "env": v.ENV, "timeout_ms": 20000})
    return text


def stats(c, sock):
    text = ctl(c, sock, "stats")
    return {k: int(n) for k, n in re.findall(r"\b(commits|dmabuf_imported|dmabuf_failed)=(\d+)", text)}, text


def vkcube(c, sock, until):
    """Runs RADV's vkcube on `sock` until `until(stats)` or 120 s; returns the last stats, the program's output and its
    window line. The program is stopped in every case."""
    env = dict(v.ENV, WAYLAND_DISPLAY=sock, **RADV)
    _, text, _ = call(c, "run", {"path": "/bin/sh", "args": [MESA_RUN, "vkcube", "--wsi", "wayland"], "env": env, "timeout_ms": 1500})
    m = re.search(r"\(call (\d+)\)", text)
    need(m is not None, "vkcube keeps running", text)
    n = int(m.group(1))
    s, whole = {}, ""
    try:
        end = time.time() + 120
        while time.time() < end:
            _, out, _ = call(c, "output", {"call": n})
            text += "\n" + out
            s, whole = stats(c, sock)
            if until(s) or re.search(r"ended|exit code|exited", out):
                break
            time.sleep(1)
    finally:
        call(c, "stop", {"call": n})
    open(os.path.join(v.RUN, f"vkcube-{sock}.out"), "w").write(text)
    return s, text, whole


def main():
    c = connect(300)
    _, t, _ = call(c, "run", {"path": "/bin/sh", "args": ["-c", f"mkdir -p {HOME} /tmp/wy; ls /dev/dri"], "env": v.ENV, "timeout_ms": 10000})
    need("renderD128" in t, "/dev/dri/renderD128 (drm=fake)", t)

    # The new way: no main device, a copy Pixman can map, explicit sync still there.
    comp = Job(c, "/bin/bitscomp", ["--socket", "wayland-1", "--stats", "5"], sock="wayland-1")
    comp.wait_for(r"zwp_linux_dmabuf_v1 for renderer pixman, main device 0:0 \(none\), \d+ formats: XR24:LINEAR", "Pixman names no main device, XRGB8888 LINEAR first")
    comp.wait_for(r"syncobj: wp_linux_drm_syncobj_manager_v1 on /dev/dri/renderD128", "explicit sync stays on the render node")
    s, text, whole = vkcube(c, "wayland-1", lambda s: s.get("dmabuf_imported", 0) >= 1 or s.get("dmabuf_failed", 0) >= 1)
    print(f"no main device: {s}", flush=True)
    need(not re.search(r"Cannot connect to wayland|vkCreate\w+ failed|VK_ERROR", text), "vkcube made its device and swapchain", text[-2000:])
    need(s.get("dmabuf_imported", 0) >= 1 and s.get("dmabuf_failed", 0) == 0, "RADV's buffers were taken, none refused", whole + comp.poll()[-1500:])
    need("refuses" not in comp.poll().split("main device 0:0")[-1], "bitscomp logs no refusal", comp.text[-1500:])

    # A GL client finds no device to render on either: Mesa's EGL goes to llvmpipe and wl_shm. It must still get a window.
    before = ctl(c, "wayland-1", "list")
    env = dict(v.ENV, WAYLAND_DISPLAY="wayland-1", HOME=HOME)
    _, text, _ = call(c, "run", {"path": "/bin/sh", "args": [MESA_RUN, "eglgears_wayland"], "env": env, "timeout_ms": 1500})
    m = re.search(r"\(call (\d+)\)", text)
    need(m is not None, "eglgears_wayland keeps running", text)
    try:
        for _ in range(60):
            now = ctl(c, "wayland-1", "list")
            if now.count(" id=") > before.count(" id="):
                break
            time.sleep(1)
        need(now.count(" id=") > before.count(" id="), "eglgears_wayland gets its window without a main device", now + text[-1500:])
    finally:
        call(c, "stop", {"call": int(m.group(1))})
    s, whole = stats(c, "wayland-1")
    need(s.get("dmabuf_failed", 0) == 0, "and bitscomp refused nothing of it", whole)
    ctl(c, "wayland-1", "quit")
    time.sleep(2)

    # The old way, for the difference: RADV finds its own node, makes VRAM images, and Pixman cannot map them.
    comp = Job(c, "/bin/bitscomp", ["--socket", "wayland-2", "--stats", "5", "--main-device", "render"], sock="wayland-2")
    comp.wait_for(r"for renderer pixman, main device 226:128 \(/dev/dri/renderD128\)", "--main-device render names the node")
    s, text, whole = vkcube(c, "wayland-2", lambda s: s.get("dmabuf_failed", 0) >= 1)
    print(f"main device 226:128: {s}", flush=True)
    need(s.get("dmabuf_failed", 0) >= 1, "with the node named, RADV's VRAM images are refused", whole + comp.poll()[-1500:])
    comp.wait_for(r"dmabuf: pixman refuses [^\n]*PermissionDenied", "the reason: EPERM on mapping, as on Linux")
    ctl(c, "wayland-2", "quit")


if __name__ == "__main__":
    main()
