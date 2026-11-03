#!/usr/bin/env python3
"""WY-3 scenario client (94-xwayland.sh): X11 programs through Xwayland on V2, over MCP.

Usage: v2_xwayland.py <mcp host port> <token> <qmp socket> <run dir>
Exit 0 = green. Timing is never judged (QEMU): only what happened."""
import os
import re
import sys
import time

sys.dont_write_bytecode = True  # no __pycache__ in the tree
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
sys.argv = sys.argv[:5] + ["-"]
import v2_check as v  # noqa: E402
from v2_check import call, connect, differing, need, screen, shell  # noqa: E402

SOCK = "wayland-1"
LD = "/boot/BITSOS2/mesa/lib/ld-linux-x86-64.so.2"
LIBS = "/boot/BITSOS2/xwayland/test/lib:/boot/BITSOS2/xwayland/lib:/boot/BITSOS2/mesa/lib"
BIN = "/boot/BITSOS2/xwayland/test/bin/"
HOME = "/tmp/wy3/home"
XENV = dict(v.ENV, DISPLAY=":0", HOME=HOME, WAYLAND_DISPLAY=SOCK)


def xargs(prog, args):
    """The test clients are the distribution's builds: started through the Mesa tree's loader (V2's own /usr/lib
    loader belongs to the initramfs glibc), as the tree's `run` does."""
    return ["--inhibit-cache", "--library-path", LIBS, BIN + prog, *args]


def xrun(c, prog, *args, timeout_ms=20000):
    _, text, _ = call(c, "run", {"path": LD, "args": xargs(prog, args), "env": XENV, "timeout_ms": timeout_ms})
    return text


class XJob(v.Job):
    """A test client that keeps running (its output collected as Job does)."""

    def __init__(self, c, prog, *args):
        self.c = c
        _, text, _ = call(c, "run", {"path": LD, "args": xargs(prog, args), "env": XENV, "timeout_ms": 1500})
        m = re.search(r"\(call (\d+)\)", text)
        need(m is not None, f"{prog} {' '.join(args)} keeps running", text)
        self.n, self.text = int(m.group(1)), text


def ctl(c, *words):
    _, text, _ = call(c, "run", {"path": "/bin/bitscomp", "args": ["ctl", "--socket", SOCK, *words], "env": v.ENV, "timeout_ms": 20000})
    return text


def mapped(c, app_id, secs=40):
    end = time.time() + secs
    while True:
        line = next((l for l in ctl(c, "list").splitlines() if f"app_id={app_id} " in l and " x11=0x" in l), None)
        if line:
            print(f"ok: {app_id} is a window of the desk: {line.strip()}", flush=True)
            return line
        if time.time() > end:
            raise SystemExit(f"RED: no X11 window {app_id}\n{ctl(c, 'list')}\n{ctl(c, 'xwayland')}")
        time.sleep(0.5)


def gone(c, app_id, secs=20):
    end = time.time() + secs
    while f"app_id={app_id} " in ctl(c, "list"):
        if time.time() > end:
            raise SystemExit(f"RED: {app_id} is still there\n{ctl(c, 'list')}")
        time.sleep(0.5)


# Evdev key codes for the test's text on bitscomp's German layout (y and z swapped there).
KEYS = {c: k for k, c in zip(range(16, 26), "qwertzuiop")}
KEYS.update({c: k for k, c in zip(range(30, 39), "asdfghjkl")})
KEYS.update({c: k for k, c in zip(range(44, 51), "yxcvbnm")})


def main():
    c = connect(300)
    renderer = "gles" if v.GLES else "pixman"
    shell(c, f"mkdir -p {HOME} /tmp/wy3 && chmod 777 /tmp/wy3")

    comp = v.Job(c, "/bin/bitscomp", ["--socket", SOCK, "--fixed-status"], sock=SOCK)
    comp.wait_for(rf"WAYLAND_DISPLAY={SOCK} .*backend=fbdev size=1280x800", "bitscomp started on fbdev")
    v.check_build(c, comp.text, deep=False)
    comp.wait_for(r"xwayland on demand: DISPLAY=:0 \(/boot/BITSOS2/xwayland/bin/Xwayland\)", "bitscomp holds X display :0")
    need("state=idle" in ctl(c, "xwayland"), "Xwayland is not started before an X11 client comes", ctl(c, "xwayland"))
    w, h, before = screen(c, f"the empty desktop ({renderer})")

    t = xrun(c, "xdpyinfo")
    comp.wait_for(r"xwayland: ready on :0 after \d+ ms", "the first X11 client started Xwayland")
    comp.wait_for(r"xwm: window manager ready \(composite \d+ xfixes \d+ xres \d+\)", "bitscomp's X11 window manager runs")
    need("X.Org version: 24.1.14" in t and "dimensions:" in t, "xdpyinfo answers (Xwayland 24.1.14)", t[-1500:])
    need(re.search(r"Composite|XFIXES", t) is not None and "XInputExtension" in t and "RANDR" in t,
         "xdpyinfo lists Composite, XFIXES, XInputExtension and RANDR", t[-2500:])

    t = xrun(c, "xrandr")
    m = re.search(r"BITS-1 connected (\d+)x(\d+)\+0\+0", t)
    need(m is not None, "xrandr shows the one output BITS-1", t[-1500:])
    t = xrun(c, "xinput", "list")
    need("xwayland-pointer" in t and "xwayland-keyboard" in t and "Virtual core XTEST pointer" in t,
         "xinput list shows Xwayland's seat devices (XInput2)", t[-1500:])

    eyes = XJob(c, "xeyes")
    line = mapped(c, "XEyes")
    need("focused=true" in line, "xeyes got the focus", line)
    time.sleep(2)
    _, _, px = screen(c, "xeyes on the desktop")
    d = differing(px, before, w)
    need(d > w * h // 20, f"xeyes is drawn ({d} pixels changed)")

    clock = XJob(c, "xclock", "-update", "1")
    mapped(c, "XClock")

    # Keyboard into an X11 program: xterm's shell writes the line typed into it.
    typed = "/tmp/wy3/typed.txt"
    term = XJob(c, "xterm", "-T", "wy3", "-e", "/bin/sh", "-c", f"read l; echo $l > {typed}; sleep 30")
    line = mapped(c, "XTerm")
    need('title="wy3"' in line and "focused=true" in line, "xterm has its title and the focus", line)
    time.sleep(2)
    ctl(c, "key", *[str(KEYS[ch]) for ch in "hallo"], "28")
    end = time.time() + 15
    while shell(c, f"cat {typed} 2>/dev/null").strip() != "hallo":
        if time.time() > end:
            raise SystemExit(f"RED: xterm did not get the keys: {shell(c, f'cat {typed}')!r}\n{term.poll()[-1500:]}")
        time.sleep(0.5)
    print("ok: keys typed through bitscomp reach xterm's shell (hallo)", flush=True)
    _, _, imgs = call(c, "screen", {})
    if imgs:
        with open(os.path.join(v.RUN, f"x11-{renderer}.png"), "wb") as f:
            f.write(imgs[0])

    # The clipboard both ways (xclip on the X11 side, wl-copy/wl-paste on the Wayland side).
    shell(c, "printf 'von X11 nach Wayland' > /tmp/wy3/in.txt")
    xrun(c, "xclip", "-selection", "clipboard", "-i", "/tmp/wy3/in.txt", timeout_ms=5000)
    comp.wait_for(r"xsel: an X11 program owns the clipboard; offered to Wayland as text/plain;charset=utf-8", "the X11 clipboard is offered to Wayland")
    t = xrun(c, "wl-paste", "-n")
    need("von X11 nach Wayland" in t, "wl-paste reads what xclip copied (X11 -> Wayland)", t[-1500:])
    xrun(c, "wl-copy", "Grüße von Wayland", timeout_ms=5000)
    end = time.time() + 15
    while "clipboard=wayland->x11" not in ctl(c, "xwayland"):
        if time.time() > end:
            raise SystemExit(f"RED: the Wayland clipboard never went to X11\n{ctl(c, 'xwayland')}")
        time.sleep(0.5)
    t = xrun(c, "xclip", "-selection", "clipboard", "-o")
    need("Grüße von Wayland" in t, "xclip -o reads what wl-copy copied (Wayland -> X11)", t[-1500:])

    # Close through bitscomp: the focused window (xterm) gets WM_DELETE_WINDOW and ends.
    ctl(c, "close")
    end = time.time() + 20
    while ctl(c, "list").count(" x11=0x") >= 3:
        if time.time() > end:
            raise SystemExit(f"RED: ctl close did not close an X11 window\n{ctl(c, 'list')}")
        time.sleep(0.5)
    print("ok: ctl close closes the focused X11 window", flush=True)
    st = ctl(c, "xwayland")
    need(re.search(r"state=ready pid=\d+ starts=1 ", st) is not None, "Xwayland still runs, started once", st)
    for j in (eyes, clock, term):
        j.poll()
    m = re.search(r"xwayland display=.*", st)
    print(m.group(0) if m else st, flush=True)
    print(ctl(c, "list").strip(), flush=True)
    ctl(c, "quit")
    time.sleep(2)
    need("PANIC" not in comp.poll(), "no panic in bitscomp's output")


if __name__ == "__main__":
    main()
