#!/usr/bin/env python3
"""WY-1a scenario client (40-v2.sh): bitscomp on V2 driven over MCP.

Usage: v2_check.py <mcp host port> <token> <qmp socket> <run dir> <reference dir>
bitscomp and two wyclients through MCP `run` (they keep running: "Still running ... (call n)"), `bitscomp ctl` for
the tiles, MCP send_keys/send_mouse through the kernel's evdev path, the clients' stdout through `output`, MCP
`screen` against QMP screendump and the host reference, `ctl quit`, a SIGKILL and a SIGTERM for the console coming back.
Exit 0 = green."""
import base64
import http.client
import json
import os
import re
import socket
import sys
import time
import zlib

PORT, TOKEN, QMP, RUN, REF = int(sys.argv[1]), sys.argv[2], sys.argv[3], sys.argv[4], sys.argv[5]
SOCK = "wayland-1"
ENV = {"PATH": "/bin:/usr/bin:/boot/BITSOS2/bin", "HOME": "/", "XDG_RUNTIME_DIR": "/tmp/wy", "WAYLAND_DISPLAY": SOCK}
COLOURS = [(0xc0, 0x30, 0x30), (0x30, 0xa0, 0x40)]
next_id = 0


def need(cond, what, detail=""):
    if not cond:
        raise SystemExit(f"RED: {what}\n{detail}")
    print(f"ok: {what}", flush=True)


def rpc(c, method, params=None):
    global next_id
    next_id += 1
    body = {"jsonrpc": "2.0", "id": next_id, "method": method, "params": params or {}}
    c.request("POST", "/mcp", json.dumps(body), {"Authorization": f"Bearer {TOKEN}", "Content-Type": "application/json"})
    r = c.getresponse()
    data = r.read()
    if r.status != 200:
        raise ValueError(f"{method}: HTTP {r.status} {data[:200]}")
    return json.loads(data)


def connect(wait_s):
    end = time.time() + wait_s
    while True:
        try:
            c = http.client.HTTPConnection("127.0.0.1", PORT, timeout=120)
            rpc(c, "initialize", {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "wy1a", "version": "1"}})
            return c
        except (OSError, http.client.HTTPException, ValueError) as e:
            if time.time() > end:
                raise SystemExit(f"RED: no MCP answer on 127.0.0.1:{PORT}: {e}")
            time.sleep(1)


def call(c, name, args=None):
    j = rpc(c, "tools/call", {"name": name, "arguments": args or {}})
    if "error" in j:
        return True, f"rpc error {j['error']}", []
    r = j["result"]
    text = "\n".join(i.get("text", "") for i in r["content"] if i["type"] == "text")
    images = [base64.b64decode(i["data"]) for i in r["content"] if i["type"] == "image"]
    return r["isError"], text, images


class Job:
    """A program started with MCP `run` that keeps running; `text` collects what `output` showed."""

    def __init__(self, c, path, args, sock=SOCK):
        self.c = c
        env = dict(ENV, WAYLAND_DISPLAY=sock)
        err, text, _ = call(c, "run", {"path": path, "args": args, "env": env, "timeout_ms": 1500})
        m = re.search(r"\(call (\d+)\)", text)
        need(m is not None, f"{path} {' '.join(args)} keeps running", text)
        self.n, self.text = int(m.group(1)), text

    def poll(self):
        _, text, _ = call(self.c, "output", {"call": self.n})
        self.text += "\n" + text
        return self.text

    def wait_for(self, pattern, what, secs=20):
        end = time.time() + secs
        while not re.search(pattern, self.poll()):
            if time.time() > end:
                raise SystemExit(f"RED: {what}\n{self.text[-3000:]}")
            time.sleep(0.3)
        print(f"ok: {what}", flush=True)


def ctl(c, *words):
    _, text, _ = call(c, "run", {"path": "/bin/bitscomp", "args": ["ctl", "--socket", SOCK, *words], "env": ENV, "timeout_ms": 20000})
    return text


def decode_png(png):
    assert png[:8] == b"\x89PNG\r\n\x1a\n", "no PNG signature"
    at, idat, w, h = 8, b"", 0, 0
    while True:
        n = int.from_bytes(png[at:at + 4], "big")
        ty, data = png[at + 4:at + 8], png[at + 8:at + 8 + n]
        if ty == b"IHDR":
            w, h, depth, colour = int.from_bytes(data[:4], "big"), int.from_bytes(data[4:8], "big"), data[8], data[9]
            assert depth == 8 and colour in (2, 6), f"PNG depth {depth} colour type {colour}"
            bpp = 3 if colour == 2 else 4
        elif ty == b"IDAT":
            idat += data
        elif ty == b"IEND":
            break
        at += 12 + n
    raw, stride = zlib.decompress(idat), w * bpp
    rows, prev = [], bytearray(stride)
    for y in range(h):
        f, line = raw[y * (stride + 1)], raw[y * (stride + 1) + 1:(y + 1) * (stride + 1)]
        cur = bytearray(stride)
        for i in range(stride):
            a = cur[i - bpp] if i >= bpp else 0
            b = prev[i]
            cc = prev[i - bpp] if i >= bpp else 0
            p = [0, a, b, (a + b) // 2][f] if f < 4 else (lambda pp: a if abs(pp - a) <= abs(pp - b) and abs(pp - a) <= abs(pp - cc) else (b if abs(pp - b) <= abs(pp - cc) else cc))(a + b - cc)
            cur[i] = (line[i] + p) & 255
        rows.append(bytes(cur) if bpp == 3 else bytes(v for k, v in enumerate(cur) if k % 4 != 3))
        prev = cur
    return w, h, b"".join(rows)


def read_ppm(path):
    data = open(path, "rb").read()
    parts, at = [], 0
    while len(parts) < 4:
        while data[at:at + 1].isspace():
            at += 1
        start = at
        while not data[at:at + 1].isspace():
            at += 1
        parts.append(data[start:at])
    return int(parts[1]), int(parts[2]), data[at + 1:at + 1 + int(parts[1]) * int(parts[2]) * 3]


class Qmp:
    def __init__(self, path):
        end = time.time() + 30
        while True:
            try:
                self.s = socket.socket(socket.AF_UNIX)
                self.s.connect(path)
                break
            except OSError:
                if time.time() > end:
                    raise SystemExit(f"RED: no QMP socket at {path}")
                time.sleep(0.5)
        self.f = self.s.makefile("rwb")
        self.read()
        self.cmd("qmp_capabilities")

    def read(self):
        while True:
            m = json.loads(self.f.readline())
            if "event" not in m:
                return m

    def cmd(self, name, args=None):
        self.f.write(json.dumps({"execute": name, "arguments": args or {}}).encode() + b"\n")
        self.f.flush()
        m = self.read()
        if "error" in m:
            raise SystemExit(f"RED: QMP {name}: {m['error']}")
        return m["return"]

    def screendump(self, n):
        path = os.path.join(RUN, f"dump{n}.ppm")
        if os.path.exists(path):
            os.remove(path)
        self.cmd("screendump", {"filename": path})
        return read_ppm(path)


def differing(a, b, w, skip=None, tol=24):
    """Pixels whose channels differ by more than `tol`, outside the box `skip` (x0, y0, x1, y1)."""
    n = 0
    for i in range(0, min(len(a), len(b)), 3):
        if skip:
            x, y = (i // 3) % w, (i // 3) // w
            if skip[0] <= x < skip[2] and skip[1] <= y < skip[3]:
                continue
        if max(abs(a[i] - b[i]), abs(a[i + 1] - b[i + 1]), abs(a[i + 2] - b[i + 2])) > tol:
            n += 1
    return n


def count(px, rgb):
    t = bytes(rgb)
    return sum(1 for i in range(0, len(px), 3) if px[i:i + 3] == t)


def screen(c, what):
    err, text, imgs = call(c, "screen", {})
    need(not err and imgs, f"MCP screen ({what})", text)
    return decode_png(imgs[0])


def console_back(c, q, n, what):
    """The console's picture again: no client colour on MCP screen, and it matches the screendump."""
    end = time.time() + 5
    while True:
        w, h, px = screen(c, what)
        if count(px, COLOURS[0]) + count(px, COLOURS[1]) == 0:
            break
        if time.time() > end:
            raise SystemExit(f"RED: {what}: the clients' colours are still on the screen")
        time.sleep(0.5)
    dw, dh, dpx = q.screendump(n)
    need((dw, dh) == (w, h) and differing(px, dpx, w) < w * h // 200, f"{what}: MCP screen == screendump (console)")


def main():
    c = connect(300)
    q = Qmp(QMP)
    rw, rh, ref = read_ppm(os.path.join(REF, "ref.ppm"))
    px_x, px_y = map(int, open(os.path.join(REF, "pointer.txt")).read().split())

    # WY-8: this one with GX-3 CR 2's two windows (--fb-pan), the later ones and the other V2 scenarios with the
    # default one window, so both paths draw the screen the checks compare.
    comp = Job(c, "/bin/bitscomp", ["--socket", SOCK, "--stats", "5", "--fixed-status", "--fb-pan"])
    comp.wait_for(r"WAYLAND_DISPLAY=wayland-1 .*backend=fbdev size=1280x800", "bitscomp started on fbdev at 1280x800")
    need("fbdev: /dev/fb0 1280x800" in comp.text, "bitscomp mapped /dev/fb0", comp.text[-2000:])
    need("two windows (pan)" in comp.text, "--fb-pan: two windows shown by pan", comp.text[-2000:])
    need("pan to window" not in comp.text, "no pan refused", comp.text[-2000:])
    need(re.search(r"input: /dev/input/event\d+ \"BitsOS MCP keyboard\" keyboard \(grabbed\)", comp.text) is not None,
         "MCP's keyboard opened and grabbed", comp.text[-2000:])
    need(re.search(r"\"BitsOS MCP pointer\" pointer \(grabbed\)", comp.text) is not None, "MCP's pointer opened and grabbed", comp.text[-2000:])
    need("no inotify on" not in comp.text, "bitscomp watches /dev/input with inotify (FS-14), no 1 s rescan", comp.text[-2000:])

    one = Job(c, "/bin/wyclient", ["--title", "one", "--pattern", "0"])
    one.wait_for(r"configure \d+ \d+\n(.|\n)*configure", "client one configured by bitscomp")
    two = Job(c, "/bin/wyclient", ["--title", "two", "--pattern", "1"])
    two.wait_for(r"configure \d+ \d+\n(.|\n)*configure", "client two configured by bitscomp")
    end = time.time() + 20
    while True:
        listing = ctl(c, "list")
        if listing.count("visible=true") == 2 and 'title="one"' in listing and 'title="two"' in listing:
            break
        if time.time() > end:
            raise SystemExit(f"RED: two tiles\n{listing}")
        time.sleep(0.5)
    need(re.search(r"tile=0 .*title=\"one\"", listing) and re.search(r"tile=1 .*focused=true.*title=\"two\"", listing),
         "two tiles, two focused (bitscomp ctl list)", listing)
    need("symbols=pc_de" in two.poll() or "symbols=pc+de" in two.text, "the German keymap reached the client", two.text[-1500:])

    # Keys through the kernel: German "z" is KEY_Y (21) on the focused client two.
    err, text, _ = call(c, "send_keys", {"text": "z"})
    need(not err, "send_keys z", text)
    two.wait_for(r"key 21 pressed\n(.|\n)*key 21 released", "client two got KEY_Y (German z) pressed and released")

    # Pointer through the kernel: absolute move into one's body; then the screen against the reference.
    err, text, _ = call(c, "send_mouse", {"x": px_x, "y": px_y})
    need(not err, f"send_mouse to {px_x},{px_y}", text)
    one.wait_for(r"pointer enter", "client one saw the pointer enter")
    time.sleep(0.5)
    w, h, px = screen(c, "two tiles")
    need((w, h) == (rw, rh), f"screen size {w}x{h} == reference {rw}x{rh}")
    # WY-11: the pointer is Adwaita's default at 48 px (hotspot 6,2 in it) drawn in software (no hardware cursor).
    box = (px_x - 8, px_y - 4, px_x + 44, px_y + 48)
    d = differing(px, ref, w, box)
    open(os.path.join(RUN, "v2-screen.rgb"), "wb").write(px)
    need(d < w * h // 200, f"MCP screen == host reference outside the pointer ({d} pixels differ)")
    near = sum(1 for y in range(px_y, px_y + 30) for x in range(px_x, px_x + 20)
               if max(abs(px[(y * w + x) * 3 + k] - ref[(y * w + x) * 3 + k]) for k in range(3)) > 24)
    need(near > 30, f"bitscomp's pointer is drawn below right of the hot spot ({near} pixels differ from the reference)")
    dw, dh, dpx = q.screendump(1)
    need((dw, dh) == (w, h) and differing(px, dpx, w) < w * h // 200, "MCP screen == QEMU screendump")

    # Click (focus moves to one), a key to one, the wheel to one.
    err, text, _ = call(c, "send_mouse", {"button": "left"})
    need(not err, "send_mouse left click", text)
    one.wait_for(r"button 272 pressed\n(.|\n)*button 272 released", "client one got the left click")
    call(c, "send_keys", {"text": "a"})
    one.wait_for(r"key 30 pressed", "client one got KEY_A after the click focused it")
    call(c, "send_mouse", {"wheel": -1})
    one.wait_for(r"axis vertical 15\.0", "client one got one wheel step down")
    call(c, "send_mouse", {"dx": 40, "dy": 0})
    one.wait_for(r"motion \d+ \d+", "client one got a relative motion")

    # WY-8: a client's own fullscreen on V2's fbdev: F11 through the kernel's keyboard reaches the focused client
    # one, which asks for it (as Brave does); it covers the whole screen (no bars, not two), Esc gives the tile back.
    call(c, "send_keys", {"keys": ["f11"]})
    one.wait_for(r"key 87 pressed", "F11 reached client one")
    one.wait_for(r"asked fullscreen", "client one asked for fullscreen")
    one.wait_for(r"states fullscreen=1", "client one got the fullscreen state")
    tail = one.text[one.text.rindex("asked fullscreen"):]
    # Logical pixels: 1280x800 at bitscomp's scale 1.25.
    need("configure 1024 640" in tail, "client one's fullscreen size is the screen's (1024x640 logical)", tail[-1500:])
    listing = ctl(c, "list")
    need(re.search(r"fullscreen visible=true focused=true frame=0,0 1024x640 .*title=\"one\"", listing), "ctl list: one covers the screen", listing)
    end = time.time() + 10
    while True:
        fw, fh, px = screen(c, "fullscreen")
        share = count(px, COLOURS[0]) / (len(px) // 3)
        # The corners and the bars' places are the client's (the rest: its 80x80 square and the pointer arrow).
        spots = [(2, 2), (fw - 3, 2), (2, fh - 3), (fw - 3, fh - 3), (fw - 3, fh // 2), (fw // 2, 2), (fw // 2, fh - 3)]
        if share > 0.98 and all(bytes(px[(y * fw + x) * 3:(y * fw + x) * 3 + 3]) == bytes(COLOURS[0]) for x, y in spots):
            break
        if time.time() > end:
            open(os.path.join(RUN, "v2-fullscreen.rgb"), "wb").write(px)
            raise SystemExit(f"RED: fullscreen client one covers only {share:.3f} of the screen, or not its corners (v2-fullscreen.rgb)")
        time.sleep(0.5)
    print(f"ok: client one's colour on {share:.4f} of the screen (the rest: its square and the pointer)", flush=True)
    call(c, "send_keys", {"keys": ["esc"]})
    one.wait_for(r"asked unfullscreen", "Esc reached client one, which asked to leave")
    one.wait_for(r"states fullscreen=0", "client one left fullscreen")
    end = time.time() + 10
    while not re.search(r"tile=0 page=0/1 visible=true focused=true .*title=\"one\"", ctl(c, "list")):
        if time.time() > end:
            raise SystemExit("RED: one is not back in its tile")
        time.sleep(0.3)
    print("ok: one back in its tile", flush=True)
    stats = ctl(c, "stats")
    need(re.search(r"input_reports=[1-9]", stats) is not None, "bitscomp counted evdev reports", stats)
    print("stats:", next((l for l in stats.splitlines() if l.startswith("frames=")), "-"))
    bench = ctl(c, "bench", "10")
    need("full_paced_ms=" in bench, "ctl bench answers", bench)
    print("bench:", next((l for l in bench.splitlines() if l.startswith("bench")), "-"))

    # Quit: the console comes back, bitscomp ends cleanly, the clients see the compositor gone.
    ctl(c, "quit")
    comp.wait_for(r"bitscomp: ended", "bitscomp ended after ctl quit")
    console_back(c, q, 2, "after quit")
    need("pan to window" not in comp.text and "pan refused" not in comp.text, "--fb-pan: no pan was refused", comp.text[-2000:])

    # A killed compositor: the kernel gives the screen back by itself.
    again = Job(c, "/bin/bitscomp", ["--socket", "wayland-2"])
    again.wait_for(r"backend=fbdev", "second bitscomp started")
    need("one window" in again.text, "the default: one window (copy path)", again.text[-2000:])
    Job(c, "/bin/wyclient", ["--title", "three", "--pattern", "0"], "wayland-2").wait_for(r"configure \d+ \d+\n(.|\n)*configure", "client three configured")
    time.sleep(0.5)
    _, _, px = screen(c, "before the kill")
    need(count(px, COLOURS[0]) > 1000, "client three on the screen")
    err, text, _ = call(c, "stop", {"call": again.n})
    need(not err, "stop (SIGKILL) the second bitscomp", text)
    console_back(c, q, 3, "after SIGKILL")

    # SIGTERM: bitscomp's own clean end (signal handler, sockets removed, screen given back).
    term = Job(c, "/bin/bitscomp", ["--socket", "wayland-4"])
    term.wait_for(r"backend=fbdev", "third bitscomp started")
    pid = re.search(r"pid=(\d+) backend=fbdev", term.text).group(1)
    Job(c, "/bin/wyclient", ["--title", "four", "--pattern", "1"], "wayland-4").wait_for(r"configure \d+ \d+\n(.|\n)*configure", "client four configured")
    err, text, _ = call(c, "run", {"path": "/bin/kill", "args": ["-TERM", pid], "env": ENV, "timeout_ms": 5000})
    need(not err, f"kill -TERM {pid}", text)
    term.wait_for(r"bitscomp: ended", "bitscomp ended cleanly on SIGTERM")
    console_back(c, q, 5, "after SIGTERM")

    # From the BusyBox shell on the console: bitscomp's terminal goes to KD_GRAPHICS, Ctrl+Alt+Q gives it back.
    bg = bytes(ref[(rh - 1) * rw * 3 + (rw - 1) * 3:][:3])
    call(c, "send_keys", {"text": "bitscomp --socket wayland-3\n"})
    end = time.time() + 20
    while True:
        _, text, _ = call(c, "ps", {})
        _, _, px = screen(c, "bitscomp from the shell")
        if "bitscomp --socket wayland-3" in text and count(px, bg) > len(px) // 6:
            break
        if time.time() > end:
            raise SystemExit(f"RED: bitscomp from the shell did not take the screen\n{text}")
        time.sleep(1)
    print("ok: bitscomp started from the shell has the screen", flush=True)
    call(c, "send_keys", {"keys": ["ctrl+alt+q"]})
    end = time.time() + 20
    while "bitscomp --socket wayland-3" in call(c, "ps", {})[1]:
        if time.time() > end:
            raise SystemExit("RED: Ctrl+Alt+Q did not end bitscomp")
        time.sleep(0.5)
    print("ok: Ctrl+Alt+Q ended bitscomp", flush=True)
    console_back(c, q, 4, "after Ctrl+Alt+Q")
    _, text, _ = call(c, "screen", {"format": "text"})
    need("console: KD_GRAPHICS" in text and "console: KD_TEXT" in text, "the shell's bitscomp switched its terminal to KD_GRAPHICS and back", text[-2500:])


if __name__ == "__main__":
    main()
