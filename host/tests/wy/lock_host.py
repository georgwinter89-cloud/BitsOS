#!/usr/bin/env python3
"""WY-20 scenario client (31-mouselook.sh): relative motion and pointer constraints against a headless bitscomp.

Usage: lock_host.py <bitscomp> <wyclient> <Xwayland> <run dir> <x11raw.py>. Exit 0 = green."""
import atexit
import os
import re
import socket
import subprocess
import sys
import threading
import time

BIN, CLIENT, XW, RUN, X11RAW = sys.argv[1:6]
SCALE = 1.25  # The default theme's; `moved` works in output pixels, the pointer in logical ones.
ALT, TAB, ESC, F7 = 56, 15, 1, 65
STARTED = []


def need(cond, what, detail=""):
    if not cond:
        raise SystemExit(f"RED: {what}\n{detail}")
    print(f"ok: {what}", flush=True)


atexit.register(lambda: [p.kill() for p in STARTED])


class Lines:
    """A child's output lines, kept; a test waits for one of them."""

    def __init__(self, p):
        self.p, self.lines, self.seen = p, [], 0
        STARTED.append(p)
        threading.Thread(target=self._read, daemon=True).start()

    def _read(self):
        for line in self.p.stdout:
            self.lines.append(line.strip())

    def expect(self, pattern, what, timeout=10):
        """The next line matching `pattern` after the ones already consumed."""
        end = time.time() + timeout
        while time.time() < end:
            for i in range(self.seen, len(self.lines)):
                if re.fullmatch(pattern, self.lines[i]):
                    self.seen = i + 1
                    print(f"ok: {what}: {self.lines[i]}", flush=True)
                    return self.lines[i]
            time.sleep(0.02)
        raise SystemExit(f"RED: {what}: no line {pattern}\n" + "\n".join(self.lines[-15:]))

    def since(self):
        """The lines not consumed yet; they count as consumed now."""
        out = self.lines[self.seen:]
        self.seen = len(self.lines)
        return out


class Comp:
    def __init__(self, name, *args):
        self.sock, self.log = f"wy31-{name}-{os.getpid()}", f"{RUN}/{name}.log"
        self.env = dict(os.environ, XDG_RUNTIME_DIR=f"{RUN}/xdg")
        for k in ("DISPLAY", "WAYLAND_DISPLAY"):
            self.env.pop(k, None)
        os.makedirs(f"{RUN}/xdg", mode=0o700, exist_ok=True)
        self.p = subprocess.Popen(
            [BIN, "--backend", "headless", "--size", "1280x800", "--socket", self.sock, "--fixed-status", "--mcp", "none", "--settings", "none", "--installer", "none", *args],
            stdout=open(self.log, "w"), stderr=subprocess.STDOUT, env=self.env)
        STARTED.append(self.p)
        end = time.time() + 30
        while "WAYLAND_DISPLAY=" not in self.text():
            if time.time() > end or self.p.poll() is not None:
                raise SystemExit(f"RED: {name}: the compositor did not start\n{self.text()[-1500:]}")
            time.sleep(0.05)

    def text(self):
        with open(self.log, errors="replace") as f:
            return f.read()

    def ctl(self, *words):
        r = subprocess.run([BIN, "ctl", "--socket", self.sock, *map(str, words)], capture_output=True, text=True, env=self.env, timeout=30)
        if r.returncode != 0:
            raise SystemExit(f"RED: ctl {words}: {r.stderr}{r.stdout}\n{self.text()[-1000:]}")
        return r.stdout

    def client(self, title, *args):
        p = subprocess.Popen([CLIENT, "--title", title, *args], stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True,
                             env=dict(self.env, WAYLAND_DISPLAY=self.sock))
        c = Lines(p)
        self.wait(lambda: f'title="{title}"' in self.ctl("list"), f"window {title} on the desk")
        return c

    def wait(self, cond, what, timeout=10):
        end = time.time() + timeout
        while not cond():
            if time.time() > end:
                raise SystemExit(f"RED: {what}\n{self.ctl('list')}{self.ctl('lock')}{self.ctl('cursor')}{self.text()[-800:]}")
            time.sleep(0.02)

    def middle(self, title):
        """The content's middle of a window in output pixels."""
        for line in self.ctl("hot").splitlines():
            m = re.match(r"win .* content=(\d+),(\d+) .*", line)
            if m and line.endswith(f'title="{title}"'):
                return int(m[1]), int(m[2])
        raise SystemExit(f"RED: no hot line of window {title}\n{self.ctl('hot')}")

    def state(self):
        return re.search(r"state=(\w+)", self.ctl("lock"))[1]

    def pointer(self):
        m = re.search(r"pointer=([-\d.]+),([-\d.]+)", self.ctl("lock"))
        return float(m[1]), float(m[2])

    def visible(self):
        return "fake_visible=true" in self.ctl("cursor")

    def focused(self, title):
        return any("focused=true" in l and f'title="{title}"' in l for l in self.ctl("list").splitlines())

    def quit(self):
        self.ctl("quit")
        try:
            self.p.wait(10)
        except subprocess.TimeoutExpired:
            self.p.kill()
            raise SystemExit("RED: bitscomp did not end")


def nothing_more(c, pattern, what, wait=0.3):
    time.sleep(wait)
    bad = [l for l in c.since() if re.fullmatch(pattern, l)]
    need(not bad, what, str(bad[:5]))


def lock_and_focus(c):
    print("== a lock, by the rules", flush=True)
    a = c.client("a", "--pattern", "0", "--relative", "--lock", "persistent")
    b = c.client("b", "--pattern", "1", "--relative")
    need(not [l for l in a.lines + b.lines if "no manager" in l or "no constraints" in l], "both globals are offered")
    c.wait(lambda: c.focused("b"), "the new window has the focus")
    x, y = c.middle("a")
    c.ctl("pointer", x, y)
    a.expect(r"pointer enter .*", "the pointer over a")
    time.sleep(0.3)
    need(c.state() == "none" and "locked" not in a.since(), "over a window without the focus: no lock")
    need(c.visible(), "the pointer stays")

    c.ctl("click", x, y)
    a.expect(r"locked", "a click gives a the focus and the lock")
    c.wait(lambda: not c.visible(), "the hardware cursor goes")
    at = c.pointer()

    print("== relative only", flush=True)
    a.since()
    c.ctl("rel", 2, 1)
    a.expect(r"relative 1\.60 0\.80 2\.00 1\.00", "a small move: surface coordinates, the device's counts")
    c.ctl("rel", 40, 0)
    a.expect(r"relative 80\.00 0\.00 40\.00 0\.00", "a fast one: accelerated, raw stays the device's")
    # Far past the screen's edge: a game keeps turning.
    for _ in range(60):
        c.ctl("rel", 100, 0)
    need(c.pointer() == at, "the pointer stood still")
    need(len([l for l in a.lines if l == "relative 200.00 0.00 100.00 0.00"]) == 60, "and every report arrived, past the screen's edge too")
    c.ctl("pointer", 50, 50)
    a.expect(r"relative .*", "an absolute device counts by its distance")
    need(c.pointer() == at, "and moves nothing")
    nothing_more(a, r"motion .*|pointer (enter|leave).*", "no wl_pointer motion, no enter or leave")
    need(b.since() and not [l for l in b.lines if l.startswith("relative")], "the other window gets nothing")

    print("== the keyboard is free", flush=True)
    c.ctl("key", ESC)
    a.expect(r"key 1 pressed", "Esc reaches the game")
    need(c.state() == "locked", "and the lock stays")
    c.ctl("key", f"+{ALT}", TAB, f"-{ALT}")
    a.expect(r"unlocked", "Alt+Tab: focus loss releases")
    c.wait(lambda: c.focused("b"), "the desktop's shortcut worked")
    need(c.state() == "none", "nothing held")
    c.wait(c.visible, "the hardware cursor is back")
    c.ctl("rel", 3, 0)
    c.wait(lambda: c.pointer() != at, "and the pointer moves again")
    a.expect(r"motion .*", "wl_pointer motion again")
    c.ctl("key", f"+{ALT}", TAB, f"-{ALT}")
    a.expect(r"locked", "the focus back: a persistent lock comes back")

    print("== its end", flush=True)
    c.ctl("key", F7)
    a.expect(r"constraint destroyed", "the game destroys it")
    c.wait(lambda: c.state() == "none", "nothing held")
    c.wait(c.visible, "the hardware cursor is back")

    d = c.client("d", "--pattern", "2", "--relative", "--lock", "persistent")
    x, y = c.middle("d")
    c.ctl("click", x, y)
    d.expect(r"locked", "another lock")
    c.wait(lambda: not c.visible(), "hidden")
    d.p.kill()
    d.p.wait()
    c.wait(lambda: c.state() == "none", "the client gone: released")
    c.wait(c.visible, "the hardware cursor is back")

    e = c.client("e", "--pattern", "3", "--lock", "persistent")
    x, y = c.middle("e")
    c.ctl("click", x, y)
    e.expect(r"locked", "and another")
    c.ctl("key", "+29", f"+{ALT}", 45, f"-{ALT}", "-29")  # Ctrl+Alt+X closes the window
    e.expect(r"close", "the shortcut closes the window under the lock")
    c.wait(lambda: c.state() == "none", "the window gone: released")
    c.wait(c.visible, "the hardware cursor is back")

    print("== oneshot, and the hint", flush=True)
    o = c.client("o", "--pattern", "1", "--lock", "oneshot")
    x, y = c.middle("o")
    c.ctl("click", x, y)
    o.expect(r"locked", "a oneshot lock")
    c.ctl("key", f"+{ALT}", TAB, f"-{ALT}")
    o.expect(r"unlocked", "released")
    c.ctl("click", x, y)
    c.wait(lambda: c.focused("o"), "the focus back")
    time.sleep(0.3)
    need(c.state() == "none" and "locked" not in o.since(), "a oneshot does not come back")
    o.p.kill()
    o.p.wait()

    h = c.client("h", "--pattern", "1", "--lock", "persistent", "--hint", "20,30")
    x, y = c.middle("h")
    c.ctl("click", x, y)
    m = h.expect(r"pointer enter .*", "the pointer over h")
    local = list(map(float, m.split()[2:]))
    h.expect(r"locked", "locked")
    px, py = c.pointer()
    origin = px - local[0], py - local[1]
    time.sleep(0.2)
    c.ctl("key", F7)
    h.expect(r"constraint destroyed", "destroyed")
    c.wait(lambda: abs(c.pointer()[0] - origin[0] - 20) < 1.5 and abs(c.pointer()[1] - origin[1] - 30) < 1.5, "the pointer goes where the hint says")
    h.p.kill()
    h.p.wait()
    print("== a hint is only a wish: outside the surface or the region the pointer stays", flush=True)
    for title, args, what in (
        ("far", ("--hint", "100000,100000"), "far past the surface"),
        ("neg", ("--hint", "-5,-5"), "before the surface"),
        ("rg", ("--hint", "1,1", "--region", "20,20,100000,100000"), "inside the surface, outside the region"),
    ):
        w = c.client(title, "--pattern", "1", "--lock", "persistent", *args)
        x, y = c.middle(title)
        c.ctl("click", x, y)
        w.expect(r"locked", f"locked ({what})")
        before = c.pointer()
        time.sleep(0.2)
        c.ctl("key", F7)
        w.expect(r"constraint destroyed", "destroyed")
        time.sleep(0.2)
        need(c.pointer() == before, f"a hint {what}: the pointer stays", f"{before} {c.ctl('lock')}")
        w.p.kill()
        w.p.wait()

    print("== a drag dropped by Esc gives the lock back without a mouse report", flush=True)
    d = c.client("d", "--pattern", "1", "--lock", "persistent")
    c.wait(lambda: c.focused("d"), "the new window has the focus")
    for line in c.ctl("hot").splitlines():
        m = re.match(r"win .* header=(\d+),(\d+) .* content=(\d+),(\d+) .*", line)
        if m and line.endswith('title="d"'):
            hx, hy, x, y = map(int, m.groups())
            break
    else:
        raise SystemExit(f"RED: no hot line of d\n{c.ctl('hot')}")
    c.ctl("press", "left", hx, hy)
    c.ctl("pointer", x, y)
    time.sleep(0.2)
    need(c.state() == "none", "the drag holds the lock back", c.ctl("lock"))
    c.ctl("key", ESC)
    d.expect(r"locked", "the drag dropped: locked, and nothing moved the pointer")
    need(c.state() == "locked", "held", c.ctl("lock"))
    c.ctl("release", "left")
    d.p.kill()
    d.p.wait()
    for p in (a, b):
        p.p.kill()
        p.p.wait()
    c.wait(lambda: "title=" not in c.ctl("list"), "all windows gone")


def confinement(c):
    print("== a confinement", flush=True)
    d = c.client("d", "--pattern", "2", "--relative", "--confine", "persistent")
    other = c.client("other", "--pattern", "1")
    x, y = c.middle("d")
    c.ctl("click", x, y)
    m = d.expect(r"pointer enter .*", "over d")
    local = list(map(float, m.split()[2:]))
    d.expect(r"confined", "the focused window confines")
    need("hidden=false" in c.ctl("lock") and c.visible(), "the pointer stays visible")
    px, py = c.pointer()
    origin = px - local[0], py - local[1]
    size = d.lines and [l for l in d.lines if l.startswith("configure")][-1].split()[1:]
    w, h = int(size[0]), int(size[1])
    d.since()
    c.ctl("rel", 3000, 0)
    c.wait(lambda: c.pointer()[0] > px, "it moves")
    need(abs(c.pointer()[0] - (origin[0] + w - 1)) < 1.5, "it stops at the surface's right edge", c.ctl("lock"))
    d.expect(r"relative .*", "with the relative motion")
    d.expect(r"motion .*", "and the motion")
    c.ctl("rel", -6000, -6000)
    c.wait(lambda: c.pointer()[0] < origin[0] + 2, "back to the left edge")
    need(abs(c.pointer()[1] - origin[1]) < 1.5, "and the top", c.ctl("lock"))
    need(c.state() == "confined" and c.focused("d"), "still in the window")
    c.ctl("key", f"+{ALT}", TAB, f"-{ALT}")
    d.expect(r"unconfined", "focus loss releases")
    for p in (d, other):
        p.p.kill()
        p.p.wait()
    c.wait(lambda: "title=" not in c.ctl("list"), "all windows gone")

    print("== with a region", flush=True)
    r = c.client("r", "--pattern", "2", "--confine", "persistent", "--region", "10,10,100,100")
    x, y = c.middle("r")
    c.ctl("pointer", x, y)
    m = r.expect(r"pointer enter .*", "over r, outside its region")
    local = list(map(float, m.split()[2:]))
    px, py = c.pointer()
    origin = px - local[0], py - local[1]
    time.sleep(0.3)
    need(c.state() == "none" and "confined" not in r.since(), "outside its region: not yet")
    c.ctl("pointer", (origin[0] + 50) * SCALE, (origin[1] + 50) * SCALE)
    r.expect(r"confined", "inside: confined")
    c.ctl("rel", 3000, 0)
    time.sleep(0.2)
    need(abs(c.pointer()[1] - origin[1] - 50) < 1.5 and c.pointer()[0] <= origin[0] + 110, "the region holds it", c.ctl("lock"))
    r.p.kill()
    r.p.wait()
    c.wait(lambda: c.state() == "none", "released with the client")


def overlay():
    print("== the desktop's own dialog takes the pointer back", flush=True)
    path = f"{RUN}/install.sock"
    if os.path.exists(path):
        os.unlink(path)
    # All bitscomp needs to open its grant dialog is an installer on the line.
    srv = socket.socket(socket.AF_UNIX)
    srv.bind(path)
    srv.listen(1)
    c = Comp("overlay", "--pointer", "fake", "--xwayland", "none", "--installer", path)
    srv.settimeout(10)
    conn, _ = srv.accept()
    a = c.client("a", "--pattern", "0", "--lock", "persistent")
    x, y = c.middle("a")
    c.ctl("click", x, y)
    a.expect(r"locked", "locked")
    c.ctl("phys", "key", "+29", f"+{ALT}", 34, f"-{ALT}", "-29")  # Ctrl+Alt+G, from the real keyboard
    a.expect(r"unlocked", "the dialog releases it")
    need(c.state() == "none", "nothing held", c.ctl("lock"))
    c.wait(c.visible, "the pointer is there to answer it")
    c.ctl("phys", "key", ESC)
    a.expect(r"locked", "the dialog gone: back")
    a.p.kill()
    a.p.wait()
    c.quit()
    conn.close()
    srv.close()


def software_pointer():
    print("== the software pointer", flush=True)
    c = Comp("soft", "--pointer", "soft", "--xwayland", "none")
    time.sleep(3)  # the start animation is over
    a = c.client("a", "--pattern", "0", "--lock", "persistent")
    x, y = c.middle("a")
    c.ctl("click", x, y)
    a.expect(r"locked", "locked")
    time.sleep(1)

    def shot(name):
        path = f"{RUN}/soft-{name}.ppm"
        c.ctl("shot", path)
        return open(path, "rb").read()

    hidden = shot("locked")
    c.ctl("key", F7)
    a.expect(r"constraint destroyed", "destroyed")
    time.sleep(1)
    shown = shot("free")
    need(len(hidden) == len(shown) and hidden != shown, "the frames carry the pointer only when it is free")
    head = len(hidden) - 1280 * 800 * 3
    far = [i for i in range(head, len(hidden), 3) if hidden[i:i + 3] != shown[i:i + 3]
           and not (abs((i - head) // 3 % 1280 - x) < 64 and abs((i - head) // 3 // 1280 - y) < 64)]
    need(not far, "and nothing else changed", str(far[:5]))
    a.p.kill()
    a.p.wait()
    c.quit()


def xwayland():
    print("== Xwayland: XI2 raw motion and the constraints", flush=True)
    c = Comp("x", "--pointer", "fake", "--xwayland", XW)
    c.wait(lambda: "xwayland on demand" in c.text(), "bitscomp holds a display", 20)
    disp = re.search(r"xwayland on demand: DISPLAY=(:\d+)", c.text())[1]
    env = dict(c.env, DISPLAY=disp, WAYLAND_DISPLAY=c.sock)
    x = Lines(subprocess.Popen([sys.executable, "-I", X11RAW], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, env=env, bufsize=1))

    def cmd(line):
        x.p.stdin.write(line + "\n")
        x.p.stdin.flush()
        x.expect(line + r".*", f"X11 client: {line}", 15)

    x.expect(r"ready .*", "the X11 client is up on Xwayland", 30)
    c.wait(lambda: " x11=0x" in c.ctl("list"), "its window is on the desk", 20)
    line = next(l for l in c.ctl("list").splitlines() if " x11=0x" in l)
    m = re.search(r"frame=(\d+),(\d+) (\d+)x(\d+)", line)
    # The middle of its frame is the content (bitscomp's list counts in output pixels here).
    mx, my = int(m[1]) + int(m[3]) // 2, int(m[2]) + int(m[4]) // 2
    c.ctl("click", mx, my)
    time.sleep(0.5)
    for _ in range(3):
        c.ctl("rel", 7, 3)
    x.expect(r"raw 7\.00 3\.00", "ctl rel reaches XI2 as raw motion, unaccelerated", 10)
    c.ctl("rel", 40, 0)
    x.expect(r"raw 40\.00 0\.00", "a fast move too: raw stays the device's counts", 10)
    cmd("grab")
    c.wait(lambda: c.state() == "confined", "a pointer grab confined to the window is a confinement", 10)
    cmd("ungrab")
    c.wait(lambda: c.state() == "none", "the ungrab ends it", 10)
    cmd("hide")
    c.wait(lambda: c.state() == "locked", "a hidden cursor's warp is a lock", 10)
    c.wait(lambda: not c.visible(), "the hardware cursor goes", 10)
    at = c.pointer()
    x.since()
    c.ctl("rel", 5, 5)
    x.expect(r"raw 5\.00 5\.00", "locked: raw motion goes on", 10)
    need(c.pointer() == at, "and the pointer stands")
    cmd("show")
    c.wait(lambda: c.state() == "none", "the cursor back: released", 10)
    x.p.kill()
    x.p.wait()
    c.quit()


c = Comp("main", "--pointer", "fake", "--xwayland", "none")
time.sleep(1)
lock_and_focus(c)
confinement(c)
c.quit()
overlay()
software_pointer()
xwayland()
print("GREEN: mouse look on the host")
