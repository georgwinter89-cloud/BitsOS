#!/usr/bin/env python3
"""WY-19 scenario client (89-effects.sh): the effect level drawn by a headless bitscomp on the host.

Usage: fx_host.py <rescue build> <gpu build> <wyclient> <run dir> <ref dir>
Exit 0 = green. Set WY_FX_REF=1 to write the reference pictures instead of comparing them."""
import atexit
import os
import re
import subprocess
import sys
import time

sys.dont_write_bytecode = True
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "tools"))
import img  # noqa: E402

RESCUE, GPU, CLIENT, RUN, REF = sys.argv[1:6]
SIZE = "2560x1440"
W, H = 2560, 1440


STARTED = []


def start(*a, **k):
    """A process that goes with this script, however it ends."""
    p = subprocess.Popen(*a, **k)
    STARTED.append(p)
    return p


atexit.register(lambda: [p.kill() for p in STARTED])


def need(cond, what, detail=""):
    if not cond:
        raise SystemExit(f"RED: {what}\n{detail}")
    print(f"ok: {what}", flush=True)


class Comp:
    """One compositor with one window."""

    def __init__(self, name, argv):
        self.name, self.sock, self.log = name, f"wyfx-{name}-{os.getpid()}", f"{RUN}/{name}.log"
        self.env = dict(os.environ, XDG_RUNTIME_DIR=f"{RUN}/xdg")
        self.env.pop("DISPLAY", None)
        self.env.pop("WAYLAND_DISPLAY", None)
        self.p = start(
            [*argv, "--backend", "headless", "--size", SIZE, "--socket", self.sock, "--fixed-status", "--mcp", "none", "--settings", "none", "--installer", "none"],
            stdout=open(self.log, "w"), stderr=subprocess.STDOUT, env=self.env)
        end = time.time() + 30
        while "WAYLAND_DISPLAY=" not in self.text():
            if time.time() >= end or self.p.poll() is not None:
                raise SystemExit(f"RED: {name}: the compositor did not start\n{self.text()[-1500:]}")
            time.sleep(0.1)
        self.client = start([CLIENT, "--title", "one", "--pattern", "0"], env=dict(self.env, WAYLAND_DISPLAY=self.sock),
                                       stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        end = time.time() + 20
        while 'title="one"' not in self.ctl("list"):
            if time.time() > end:
                raise SystemExit(f"RED: {name}: the window did not map\n{self.text()[-1500:]}")
            time.sleep(0.1)
        time.sleep(3)  # the start animation and the window's opening are over

    def text(self):
        with open(self.log, errors="replace") as f:
            return f.read()

    def ctl(self, *words):
        r = subprocess.run([RESCUE, "ctl", "--socket", self.sock, *words], capture_output=True, text=True, env=self.env, timeout=30)
        return r.stdout + r.stderr

    def fx(self):
        return self.ctl("fx").strip()

    def field(self, line, key):
        m = re.search(rf"\b{key}=(\S+)", line)
        return m.group(1) if m else None

    def shot(self, name, settle=0.6):
        time.sleep(settle)
        path = f"{RUN}/{self.name}-{name}.ppm"
        self.ctl("shot", path)
        return img.read_ppm(path)[2]

    def frames_over(self, secs):
        self.ctl("stats", "reset")
        time.sleep(secs)
        return int(self.field(self.ctl("stats"), "frames"))

    def quit(self):
        self.ctl("quit")
        self.client.kill()
        try:
            self.p.wait(10)
        except subprocess.TimeoutExpired:
            self.p.kill()
        self.client.wait()


def at(px, x, y):
    i = (y * W + x) * 3
    return px[i], px[i + 1], px[i + 2]


def lum(px, x, y):
    return sum(at(px, x, y))


def box(px, x0, y0, x1, y1):
    return b"".join(px[(y * W + x0) * 3:(y * W + x1) * 3] for y in range(y0, y1))


def row_range(px, x0, y0, x1, y1):
    """The largest spread of brightness along one row of a box (the dots show here; the scanlines, whole rows, do not)."""
    worst = 0
    for y in range(y0, y1):
        v = [lum(px, x, y) for x in range(x0, x1)]
        worst = max(worst, max(v) - min(v))
    return worst


def differing(a, b, tol=0):
    n = 0
    for i in range(0, len(a), 3):
        if a[i] != b[i] or a[i + 1] != b[i + 1] or a[i + 2] != b[i + 2]:
            if max(abs(a[i] - b[i]), abs(a[i + 1] - b[i + 1]), abs(a[i + 2] - b[i + 2])) > tol:
                n += 1
    return n


def reference(c, name, px):
    """Against the picture in REF, within the screenshot tolerance."""
    path = f"{REF}/effects-{name}.png"
    tmp = f"{RUN}/{c.name}-ref-{name}.ppm"
    with open(tmp, "wb") as f:
        f.write(f"P6\n{W} {H}\n255\n".encode() + px)
    if os.environ.get("WY_FX_REF") == "1":
        os.makedirs(REF, exist_ok=True)
        img.write_png(path, W, H, px)
        print(f"wrote {path}")
        return
    (_, _, want) = img.load(path)
    n = differing(want, px, tol=24)
    need(n <= 0.003 * W * H, f"reference {name}: {n} pixels differ (at most 0.3 %)", tmp)


def level(c, want):
    end = time.time() + 15
    while c.field(c.fx(), "level") != want:
        if time.time() > end:
            raise SystemExit(f"RED: level {want} not reached: {c.fx()}\n{c.text()[-1200:]}")
        time.sleep(0.1)


def gpu_level_and_back(c):
    cheap = c.shot("cheap")
    need(c.frames_over(3) == 0, "cheap: a still desktop draws no frame")
    c.ctl("set", "theme.effects", "gpu")
    c.ctl("fx", "freeze", "4.5")
    level(c, "gpu")
    gpu = c.shot("gpu", settle=1.5)
    need(differing(cheap, gpu) > 10000, "gpu draws another picture than cheap")
    # The stage's glass: the dot grid under it goes; on the wallpaper beside it the dots stay. The box under "Als
    # Nächstes" has no panel, the box inside it has glass.
    glass_gpu, glass_cheap = row_range(gpu, 1960, 660, 2460, 780), row_range(cheap, 1960, 660, 2460, 780)
    print(f"dots under the glass: spread along a row {glass_cheap} cheap, {glass_gpu} gpu")
    need(glass_gpu * 3 < glass_cheap, "the glass blurs the dots away", f"{glass_cheap} / {glass_gpu}")
    bare_gpu = row_range(gpu, 1960, 950, 2460, 1060)
    need(bare_gpu * 2 > glass_cheap, "beside the glass they stay", f"{bare_gpu}")
    # A shadow under the panel, soft: darker just below its edge than further down.
    near = sum(lum(gpu, x, 906) for x in range(2000, 2400, 16))
    far = sum(lum(gpu, x, 1040) for x in range(2000, 2400, 16))
    need(near < far, "a soft shadow under the panel", f"{near} / {far}")
    reference(c, "gpu-dark", gpu)
    again = c.shot("gpu-again", settle=0.2)
    need(differing(gpu, again) == 0, "frozen clocks: the same picture again")
    need(c.frames_over(3) == 0, "frozen clocks ask for no frame")
    live_backdrop(c)

    # The beam: at the clock's start above the screen, later a band, and nothing else in the frame's margin moves.
    c.ctl("fx", "freeze", "0")
    t0 = c.shot("t0")
    margin = lambda px: box(px, 2510, 900, 2560, 1090)
    need(margin(t0) == margin(gpu), "the beam is not at the margin's lower part at either time")
    band = sum(lum(gpu, 2530, y) for y in range(800, 825)) - sum(lum(t0, 2530, y) for y in range(800, 825))
    need(band > 0, "the band brightens what it crosses", str(band))
    disc = box(gpu, 2100, 240, 2310, 460) != box(t0, 2100, 240, 2310, 460)
    need(disc, "the radar has turned")
    return cheap, gpu


def live_backdrop(c):
    """A client drawing beside the glass costs no blur; under a glass (a revealed classic bar) it does, and shows."""
    moving = start([CLIENT, "--title", "moving", "--pattern", "1", "--animate"], env=dict(c.env, WAYLAND_DISPLAY=c.sock),
                              stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    time.sleep(3)
    before = int(c.field(c.fx(), "blurs"))
    frames = c.frames_over(3)
    after = int(c.field(c.fx(), "blurs"))
    need(frames > 3 and after == before, f"a client drawing beside the glass: {frames} frames, no blur", c.fx())
    c.ctl("set", "theme.layout.preset", "classic")
    time.sleep(1)
    c.ctl("sidebar")
    time.sleep(1)
    c.ctl("reveal", "right")
    time.sleep(2)
    before = int(c.field(c.fx(), "blurs"))
    time.sleep(3)
    after = int(c.field(c.fx(), "blurs"))
    need(after > before, f"a client drawing under a revealed bar's glass: the blur follows ({before} to {after})", c.ctl("bars"))
    c.ctl("reveal", "none")
    c.ctl("sidebar")
    c.ctl("unset", "theme.layout.preset")
    moving.kill()
    moving.wait()
    time.sleep(2)


def live_back(c, cheap):
    c.ctl("fx", "freeze", "off")
    c.ctl("set", "theme.effects", "cheap")
    level(c, "cheap")
    back = c.shot("cheap-again", settle=1.5)
    need(differing(cheap, back) == 0, "back at cheap: the same picture to the bit", str(differing(cheap, back)))
    need(c.frames_over(3) == 0, "and no frame on a still desktop")
    need(c.field(c.fx(), "moving") == "0", "nothing moves", c.fx())


def motion(c):
    c.ctl("set", "theme.effects", "gpu")
    level(c, "gpu")
    time.sleep(1)
    need(c.frames_over(2) > 3, "gpu, motion medium: ambient frames come")
    need(c.field(c.fx(), "moving") == "1", "the clock asks for them", c.fx())
    c.ctl("set", "theme.motion", "off")
    time.sleep(1)
    need(c.frames_over(3) == 0, "gpu, motion off: blur and shadows stay, no frame", c.fx())
    need(c.field(c.fx(), "moving") == "0", "the clock is off", c.fx())
    c.ctl("set", "theme.motion", "calm")
    c.ctl("fx", "freeze", "4.5")
    calm_a = c.shot("calm-a", settle=1.0)
    c.ctl("fx", "freeze", "0")
    calm_b = c.shot("calm-b", settle=1.0)
    need(box(calm_a, 2510, 780, 2560, 830) == box(calm_b, 2510, 780, 2560, 830), "calm has no beam")
    need(box(calm_a, 2100, 240, 2310, 460) == box(calm_b, 2100, 240, 2310, 460), "and no sweeping radar")
    c.ctl("fx", "freeze", "off")
    time.sleep(0.5)
    need(c.frames_over(2) > 0, "but its rings still turn")
    c.ctl("set", "theme.motion", "medium")


def transitions(c):
    c.ctl("fx", "freeze", "2")
    level(c, "gpu")
    one = c.shot("ws1", settle=1.5)
    c.ctl("ws", "new")
    time.sleep(0.2)
    c.ctl("fx", "stop")
    two = c.shot("ws2", settle=1.5)
    need(differing(one, two) > 10000, "the second workspace looks different")
    c.ctl("ws", "1")
    c.ctl("fx", "hold", "0")
    time.sleep(0.5)
    c.ctl("fx", "stop")
    time.sleep(1.2)
    # Workspace 1 to 2 with the wipe standing a third of the way (its curve starts slowly): the middle shows the new scene, the corners the old.
    c.ctl("ws", "2")
    c.ctl("fx", "hold", "0.35")
    mid = c.shot("wipe", settle=1.2)
    centre = (1280 - 60, 806 - 60, 1280 + 60, 806 + 60)
    need(box(mid, *centre) == box(two, *centre), "wipe: the new scene in the hole")
    corner = (2100, 980, 2500, 1060)
    need(box(mid, *corner) == box(one, *corner), "wipe: the old picture outside it")
    need(differing(mid, one) > 1000 and differing(mid, two) > 1000, "wipe: neither picture alone")
    reference(c, "wipe", mid)
    during = int(c.field(c.fx(), "textures"))
    c.ctl("fx", "stop")
    plain = c.shot("after-wipe", settle=1.0)
    need(differing(plain, two) == 0, "after it the plain new scene", str(differing(plain, two)))
    need(int(c.field(c.fx(), "textures")) < during, "and the old picture's texture goes back", f"{during}: {c.fx()}")

    # The primitives on the same two pictures: each differs from both and goes without a trace.
    for what, words in [
        ("glitch", ["glitch", "0.3"]),
        ("carousel", ["carousel", "0.5"]),
        ("morph", ["morph", "0", "0", "1280", "720", "400", "300", "640", "360", "0.6"]),
        ("fade", ["fade", "0.5"]),
    ]:
        c.ctl("ws", "1")
        c.ctl("fx", "stop")
        time.sleep(0.8)
        c.ctl("ws", "2")
        r = c.ctl("fx", "turn", *words)
        need(what in r, f"{what}: plays", r)
        pic = c.shot(what, settle=1.2)
        need(differing(pic, two, tol=2) > 1000, f"{what}: it shows over the new scene")
        c.ctl("fx", "stop")
        gone = c.shot(f"after-{what}", settle=1.0)
        need(differing(gone, two) == 0, f"{what}: gone without a trace")
    c.ctl("fx", "title", "BitsOS", "0.15")
    blurred = c.shot("title-blur", settle=1.0)
    c.ctl("fx", "title", "BitsOS", "1")
    sharp = c.shot("title-sharp", settle=1.0)
    region = (900, 620, 1660, 820)
    need(box(blurred, *region) != box(sharp, *region), "title: the reveal's blur shows")
    c.ctl("fx", "stop")
    gone = c.shot("after-title", settle=1.0)
    need(differing(gone, two) == 0, "title: gone without a trace")
    # The demo primitives: a pulse, a ping and a thread.
    for what in ["pulse", "ping", "flow"]:
        need(c.ctl("fx", "demo", what, "600", "500", "600", "300" if what == "flow" else "40").strip() == "ok", f"demo {what}: taken")
        demo = c.shot(f"demo-{what}", settle=1.0)
        need(differing(demo, two, tol=2) > 50, f"demo {what}: draws")
        c.ctl("fx", "demo", "clear")
        gone = c.shot(f"after-demo-{what}", settle=1.0)
        need(differing(gone, two) == 0, f"demo {what}: gone without a trace")
    c.ctl("fx", "freeze", "off")


def bad_commands(c):
    """Words from the socket: refused with a reason, and the compositor goes on drawing."""
    c.ctl("fx", "freeze", "off")
    for words in [
        ["nonsense"], ["software", "maybe"], ["freeze", "NaN"], ["freeze", "inf"], ["freeze", "1e308"], ["wait", "NaN"],
        ["hold", "NaN"], ["hold", "inf"], ["play", "wipe", "NaN"], ["play", "morph", "1", "2", "3"],
        ["play", "morph", "a", "b", "c", "d", "e", "f", "g", "h"],
        ["play", "morph", "0", "0", "0", "0", "0", "0", "10", "10"],
        ["play", "morph", "2147483647", "2147483647", "2147483647", "2147483647", "0", "0", "10", "10"],
        ["turn", "morph", "-5", "0", "10", "10", "0", "0", "10", "10"],
        ["title"], ["title", "W" * 3000], ["demo"], ["demo", "pulse", "1", "2"], ["demo", "ping", "0", "0", "0", "0"],
        ["demo", "flow", "2147483647", "2147483647", "2147483647", "2147483647"], ["demo", "nothing", "0", "0", "10", "10"],
    ]:
        r = c.ctl("fx", *words)
        need("error:" in r, f"fx {' '.join(words)[:50]}: refused", r)
    line = c.fx()
    need(c.p.poll() is None and c.field(line, "transition") == "none" and c.field(line, "frozen") == "off", "nothing of it took hold", line)
    need(c.frames_over(2) > 0, "and the frames go on")


def late_frames(c, name="readback"):
    """A frame past the GPU wait is not shown: the next one shows it, though nothing else asks for one, and then all is still."""
    c.ctl("fx", "freeze", "4.5")
    c.ctl("ws", "1")
    c.ctl("fx", "stop")
    time.sleep(1.2)
    c.ctl("ws", "2")
    c.ctl("fx", "stop")
    in_time = c.shot("late-in-time", settle=1.2)
    c.ctl("ws", "1")
    c.ctl("fx", "stop")
    time.sleep(1.2)
    before = int(c.field(c.fx(), "late"))
    need(c.ctl("fx", "wait", "0").strip() == "ok", f"{name}: the wait is cut")
    c.ctl("ws", "2")
    c.ctl("fx", "stop")
    time.sleep(1.2)
    late = int(c.field(c.fx(), "late"))
    need(late > before, f"{name}: frames come back before their GPU work has ended", f"{before} -> {late}: {c.text()[-600:]}")
    need("outlasted" in c.text(), f"{name}: and it says so", c.text()[-600:])
    if name == "direct":
        # Its next frame redraws both windows, so at a wait of nothing it is late too: it tries on its own, and shows
        # when a frame comes in time.
        time.sleep(1)
        again = int(c.field(c.fx(), "late"))
        need(again > late, "direct: the next frame comes by itself", f"{late} -> {again}")
        need(c.ctl("fx", "wait", "off").strip() == "ok", "direct: the wait is whole again")
        time.sleep(1.2)
    pic = c.shot("late")
    n = differing(pic, in_time)
    need(n == 0, f"{name}: the late frame gets its next frame and shows", f"{n} pixels differ")
    need(c.frames_over(3) == 0, f"{name}: and after it no frame on a still desktop")
    c.ctl("fx", "wait", "off")
    c.ctl("fx", "freeze", "off")


def light(c):
    c.ctl("fx", "freeze", "4.5")
    c.ctl("set", "theme.mode", "light")
    px = c.shot("gpu-light", settle=2)
    reference(c, "gpu-light", px)
    c.ctl("set", "theme.mode", "dark")
    c.ctl("fx", "freeze", "off")


def fullscreen(c):
    """A fullscreen window has no effect on it and asks for no frame."""
    c.ctl("ws", "1")
    time.sleep(1.5)
    full = start([CLIENT, "--title", "full", "--pattern", "1", "--fullscreen"], env=dict(c.env, WAYLAND_DISPLAY=c.sock),
                            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    time.sleep(3)
    need("fullscreen" in c.ctl("list"), "the fullscreen window is there", c.ctl("list"))
    need(c.field(c.fx(), "moving") == "0", "a fullscreen window: the clock is off", c.fx())
    need(c.field(c.fx(), "elements") == "0", "a fullscreen window: no effect in its frame", c.fx())
    need(c.frames_over(2) == 0, "a fullscreen window: no frame comes from the level")
    full.kill()
    full.wait()
    time.sleep(2)
    need(c.field(c.fx(), "moving") == "1", "and the clock goes on when it is gone", c.fx())


def direct_path(read_gpu):
    """The direct path (WY-17, memfds standing in for fbdev's dma-bufs) draws the same level as the readback."""
    d = Comp("direct", [GPU, "--renderer", "gles", "--fx-on-software", "--scanout", "direct", "--fake-scanout", "ok"])
    need(d.field(d.ctl("stats"), "scanout") == "direct", "direct: ctl stats says direct", d.text()[-1000:])
    d.ctl("set", "theme.effects", "gpu")
    d.ctl("fx", "freeze", "4.5")
    level(d, "gpu")
    gpu = d.shot("gpu", settle=1.5)
    n = differing(read_gpu, gpu, tol=2)
    need(n <= 0.0005 * W * H, f"direct: the gpu level as the readback draws it ({n} pixels differ)")
    one = d.shot("ws1", settle=0.2)
    d.ctl("ws", "new")
    time.sleep(0.2)
    d.ctl("fx", "stop")
    two = d.shot("ws2", settle=1.5)
    d.ctl("ws", "1")
    d.ctl("fx", "stop")
    time.sleep(1.2)
    d.ctl("ws", "2")
    d.ctl("fx", "hold", "0.35")
    mid = d.shot("wipe", settle=1.2)
    centre = (1280 - 60, 806 - 60, 1280 + 60, 806 + 60)
    corner = (2100, 980, 2500, 1060)
    need(box(mid, *centre) == box(two, *centre) and box(mid, *corner) == box(one, *corner), "direct: the wipe's snapshot comes from the shown window")
    d.ctl("fx", "stop")
    plain = d.shot("after-wipe", settle=1.0)
    need(differing(plain, two) == 0, "direct: and goes without a trace")
    late_frames(d, "direct")
    need(d.field(d.ctl("stats"), "scanout") == "direct", "still direct", d.text()[-1000:])
    need("render:" not in d.text(), "no frame failed", d.text()[-1500:])
    d.quit()


def boot_scene():
    """The boot scene ends in the circle wipe at the gpu level (at the cheap level its fade: ds/60-boot.sh)."""
    path = f"{RUN}/boot.fixture"
    now = int(time.monotonic() * 1000)
    with open(path, "w", encoding="utf-8") as f:
        f.write("bitsos-boot 1\nseq 14\ndrawn 14\nscreen handed\ngeneration 49\n")
        f.write(f"step kernel done {now - 3000} Kern\nstep desktop running {now} /bin/bitscomp startet (versuch 1)\n")
    log = f"{RUN}/boot.log"
    env = dict(os.environ, XDG_RUNTIME_DIR=f"{RUN}/xdg", BITSCOMP_BOOT_FILE=path)
    env.pop("DISPLAY", None)
    env.pop("WAYLAND_DISPLAY", None)
    sock = f"wyfx-boot-{os.getpid()}"
    p = start([GPU, "--renderer", "gles", "--fx-on-software", "--backend", "headless", "--size", SIZE, "--socket", sock, "--fixed-status", "--mcp", "none", "--settings", "none", "--installer", "none", "--run", "sleep 30"],
              stdout=open(log, "w"), stderr=subprocess.STDOUT, env=env)
    ctl = lambda *w: subprocess.run([RESCUE, "ctl", "--socket", sock, *w], capture_output=True, text=True, env=env, timeout=30).stdout
    text = lambda: open(log, errors="replace").read()
    end = time.time() + 30
    while "boot scene: shown" not in text():
        if time.time() > end or p.poll() is not None:
            raise SystemExit(f"RED: no boot scene\n{text()[-1500:]}")
        time.sleep(0.05)
    ctl("set", "theme.effects", "gpu")
    end = time.time() + 30
    while "boot scene: desktop shown" not in text():
        if time.time() > end:
            raise SystemExit(f"RED: the scene did not end\n{text()[-1500:]}")
        time.sleep(0.1)
    need("(wipe;" in text(), "the boot scene ends in the wipe at the gpu level", text()[-1200:])
    fx = ctl("fx")
    need("level=gpu" in fx and "snapshots=1" in fx, "the scene is the wipe's old picture", fx)
    time.sleep(2)
    need("transition=none" in ctl("fx"), "and the wipe ends", ctl("fx"))
    ctl("quit")
    p.wait(10)


def main():
    c = Comp("gles", [GPU, "--renderer", "gles", "--fx-on-software"])
    need("renderer=gles" in c.text(), "the gpu build draws with GLES", c.text()[-800:])
    cheap, read_gpu = gpu_level_and_back(c)
    live_back(c, cheap)
    motion(c)
    transitions(c)
    bad_commands(c)
    late_frames(c)
    light(c)
    fullscreen(c)
    c.ctl("set", "theme.effects", "cheap")
    level(c, "cheap")
    need("render:" not in c.text(), "no frame failed", c.text()[-1500:])
    time.sleep(1)
    line = c.fx()
    need(c.field(line, "textures") == "0", "the cheap level holds none of the level's textures", line)
    need(c.field(line, "fails") == "0" and c.field(line, "retrying") == "0", "and no effect's work failed on the way", line)
    need("effects: shaders compiled" in c.text() and not re.search(r"effects: shader \w+:", c.text()), "the shaders compile", c.text()[-1500:])
    c.quit()

    direct_path(read_gpu)
    boot_scene()

    # A software rasteriser without the switch keeps the cheap level, says why, and follows the switch live.
    s = Comp("software", [GPU, "--renderer", "gles"])
    base = s.shot("cheap")
    s.ctl("set", "theme.effects", "gpu")
    time.sleep(1)
    s.ctl("pointer", "5", "5")
    time.sleep(1)
    s.ctl("pointer", "9", "9")
    time.sleep(1)
    need(s.field(s.fx(), "level") == "cheap" and s.field(s.fx(), "chosen") == "gpu", "llvmpipe: gpu chosen, cheap drawn", s.fx())
    need("the renderer is a software rasteriser" in s.text(), "and it says why", s.text()[-800:])
    s.ctl("fx", "software", "on")
    level(s, "gpu")
    s.ctl("fx", "software", "off")
    level(s, "cheap")
    s.quit()

    # The rescue build: Pixman, the same.
    p = Comp("pixman", [RESCUE])
    p.ctl("set", "theme.effects", "gpu")
    time.sleep(1)
    p.ctl("pointer", "5", "5")
    time.sleep(1)
    line = p.fx()
    need(p.field(line, "level") == "cheap" and p.field(line, "renderer") == "pixman", "Pixman: the cheap level", line)
    need("renderer pixman has no shaders" in p.text(), "and it says why", p.text()[-800:])
    time.sleep(1)
    need(p.frames_over(3) == 0, "Pixman: no frame on a still desktop at theme.effects=gpu")
    p.quit()
    print("GREEN")


main()
