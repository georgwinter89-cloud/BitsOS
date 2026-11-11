#!/usr/bin/env python3
"""WY-16 scenario client (41-v2-dmabuf.sh): dma-buf windows and explicit sync on V2, over MCP.

Usage: v2_dmabuf.py <mcp host port> <token> <qmp socket> <run dir>
Exit 0 = green. Timing is never judged (QEMU): only what happened and in which order."""
import os
import re
import sys
import time

sys.dont_write_bytecode = True  # no __pycache__ in the tree
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
sys.argv = sys.argv[:5] + ["-"]
import v2_check as v  # noqa: E402
from v2_check import Job, call, connect, differing, need, screen  # noqa: E402

ENV = dict(v.ENV)


def ctl(c, sock, *words):
    _, text, _ = call(c, "run", {"path": "/bin/bitscomp", "args": ["ctl", "--socket", sock, *words], "env": ENV, "timeout_ms": 20000})
    return text


def compositor(c, sock):
    comp = Job(c, "/bin/bitscomp", ["--socket", sock, "--fixed-status"], sock=sock)
    comp.wait_for(rf"WAYLAND_DISPLAY={sock} .*backend=fbdev size=1280x800", f"bitscomp on {sock} started on fbdev")
    v.check_build(c, comp.text, deep=False)
    return comp


def once(c, sock, args):
    """A wyclient that ends by itself (a refusal, a protocol error): its whole output."""
    _, text, _ = call(c, "run", {"path": "/bin/wyclient", "args": args, "env": dict(ENV, WAYLAND_DISPLAY=sock), "timeout_ms": 20000})
    return text


def mapped(c, sock, title):
    end = time.time() + 30
    while f'title="{title}"' not in ctl(c, sock, "list"):
        if time.time() > end:
            raise SystemExit(f"RED: {title} did not map\n{ctl(c, sock, 'list')}")
        time.sleep(0.3)


def stat(c, sock, key):
    m = re.search(rf"\b{key}=(\d+)", ctl(c, sock, "stats"))
    return int(m.group(1)) if m else -1


def quit_(c, sock):
    ctl(c, sock, "quit")
    time.sleep(1.5)


def main():
    c = connect(300)
    renderer = "gles" if v.GLES else "pixman"

    # The reference: a wl_shm window.
    comp = compositor(c, "wayland-1")
    Job(c, "/bin/wyclient", ["--title", "one", "--pattern", "0"], sock="wayland-1")
    mapped(c, "wayland-1", "one")
    time.sleep(3)
    w, h, ref = screen(c, "the wl_shm window")
    quit_(c, "wayland-1")

    comp = compositor(c, "wayland-2")
    m = re.search(r"dmabuf: zwp_linux_dmabuf_v1 for renderer (\w+), main device (\d+):(\d+) \((\S+)\), (\d+) formats: (\S+)", comp.text)
    need(m is not None, "bitscomp advertises zwp_linux_dmabuf_v1", comp.text[-2500:])
    # Both read by CPU here (Pixman, GLES on llvmpipe), WY-17b; the syncobj global stays on the node.
    need(m.groups()[:4] == (renderer, "0", "0", "none") and m.group(6) == "XR24:LINEAR",
         f"renderer {renderer}, no main device (0:0), XRGB8888 LINEAR first", m.group(0))
    sync = "syncobj: wp_linux_drm_syncobj_manager_v1 on /dev/dri/renderD128" in comp.text
    need(sync, "explicit sync on the fake GPU's render node", comp.text[-2500:])

    one = Job(c, "/bin/wyclient", ["--title", "one", "--pattern", "0", "--dmabuf"], sock="wayland-2")
    one.wait_for(r"dmabuf feedback main_device=0:0 formats=\d+ first=XR24:LINEAR tranches=1", "the client read the feedback")
    one.wait_for(r"dmabuf created 0", "the udmabuf buffer was taken")
    mapped(c, "wayland-2", "one")
    time.sleep(3)
    w2, h2, px = screen(c, "the dma-buf window")
    d = differing(px, ref, w)
    need((w2, h2) == (w, h) and d < w * h // 200, f"the dma-buf window looks like the wl_shm one ({d} pixels differ)")
    need(stat(c, "wayland-2", "dmabuf_imported") >= 1, "ctl stats counts the import")

    t = once(c, "wayland-2", ["--title", "mod", "--dmabuf", "--modifier", "0x0200000000000001"])
    need("dmabuf failed" in t, "a non-linear modifier is answered failed", t[-1500:])
    # Pixman refuses it in its import; a CPU-reading GLES (llvmpipe) already in the probe.
    comp.wait_for(
        rf"dmabuf: ({renderer} refuses a \d+x\d+ XR24:0x200000000000001 buffer|refused before import: XR24:0x200000000000001)",
        "bitscomp says why",
    )

    anim = Job(c, "/bin/wyclient", ["--title", "anim", "--dmabuf", "--syncobj", "--animate"], sock="wayland-2")
    anim.wait_for(r"frames n=\d+ .* sync_released=[1-9]\d*", "the animating client gets buffers back through release points", 40)
    late = Job(c, "/bin/wyclient", ["--title", "late", "--dmabuf", "--syncobj", "--acquire-delay", "3000", "--pattern", "1"], sock="wayland-2")
    late.wait_for(r"syncobj acquire signalled point=", "the late acquire point signalled", 40)
    mapped(c, "wayland-2", "late")
    need(stat(c, "wayland-2", "acquire_waits") >= 1, "bitscomp held a commit until its acquire point signalled (acquire_waits)")
    need(stat(c, "wayland-2", "release_points") >= 1, "bitscomp signalled release points after its frames")
    for kind, code in (("no-release", 5), ("shm", 2), ("conflict", 6)):
        t = once(c, "wayland-2", ["--title", "brk", "--dmabuf", "--syncobj", "--syncobj-break", kind])
        need(f"Protocol error {code} on object wp_linux_drm_syncobj_surface_v1" in t, f"--syncobj-break {kind}: protocol error {code}", t[-1500:])
    need(stat(c, "wayland-2", "frames") > 0, "bitscomp still draws after the protocol errors")
    print(ctl(c, "wayland-2", "stats").strip().splitlines()[-1], flush=True)
    quit_(c, "wayland-2")


if __name__ == "__main__":
    main()
