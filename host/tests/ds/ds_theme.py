#!/usr/bin/env python3
"""DS-1 scenario client (20-theme.sh): the design v2 theme from V2's settingsd, applied live by bitscomp, over MCP.

Usage: ds_theme.py <mcp host port> <token> <qmp socket> <run dir> <reference dir>
Each picture is compared (MCP screen and QEMU screendump) with the host reference of ds_reference.py. Exit 0 = green."""
import base64
import http.client
import json
import os
import re
import socket
import sys
import time

sys.dont_write_bytecode = True  # no __pycache__ in the tree
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from ds_img import decode_png, differing, read_ppm  # noqa: E402

PORT, TOKEN, QMP, RUN, REF = int(sys.argv[1]), sys.argv[2], sys.argv[3], sys.argv[4], sys.argv[5]
SOCK = "wayland-1"
ENV = {"PATH": "/bin:/usr/bin:/boot/BITSOS2/bin", "HOME": "/", "XDG_RUNTIME_DIR": "/tmp/ds", "WAYLAND_DISPLAY": SOCK}
DARK = "mode=dark accent=#7fcfa0 bg=#121a16"
LIGHT = "mode=light accent=#2f6b4f bg=#e3e7e3"
# settingsd before CR DS-1-1 carries the WY-5 defaults; these keys exist there and are set to the design's values.
DESIGN = {
    "dark": {"bg": "#121a16", "surface": "#1b2520", "surface2": "#0c120f", "paper": "#1b2520", "border": "#2b3731",
             "text": "#e8eee9", "text_dim": "#a3b0a8"},
    "light": {"bg": "#e3e7e3", "surface": "#fbfcfa", "surface2": "#d5dbd5", "paper": "#fbfcfa", "border": "#c3cbc3",
              "text": "#15201a", "text_dim": "#4e5b52"},
}
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
            rpc(c, "initialize", {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "ds1", "version": "1"}})
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


def start(c, path, args):
    """A program started with MCP `run` that keeps running; returns its call number and what it printed so far."""
    err, text, _ = call(c, "run", {"path": path, "args": args, "env": ENV, "timeout_ms": 1500})
    m = re.search(r"\(call (\d+)\)", text)
    need(m is not None, f"{path} {' '.join(args)} keeps running", text)
    return int(m.group(1)), text


def wait_output(c, job, pattern, what, secs=20):
    (n, text), end = job, time.time() + secs
    while not re.search(pattern, text):
        if time.time() > end:
            raise SystemExit(f"RED: {what}\n{text[-3000:]}")
        text += "\n" + call(c, "output", {"call": n})[1]
        time.sleep(0.3)
    print(f"ok: {what}", flush=True)


def ctl(c, *words):
    return call(c, "run", {"path": "/bin/bitscomp", "args": ["ctl", "--socket", SOCK, *words], "env": ENV, "timeout_ms": 20000})[1]


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


def setting(c, tool, args):
    err, text, _ = call(c, tool, args)
    need(not err, f"{tool} {args}", text)


def wait_theme(c, pattern, what, secs):
    t0 = time.time()
    while not re.search(pattern, ctl(c, "theme")):
        if time.time() - t0 > secs:
            raise SystemExit(f"RED: {what}\n{ctl(c, 'theme')}")
        time.sleep(0.05)
    ms = int((time.time() - t0) * 1000)
    print(f"ok: {what} after {ms} ms", flush=True)
    return ms


def compare(c, q, n, ref, box, what):
    rw, rh, rpx = read_ppm(os.path.join(REF, ref))
    time.sleep(0.4)
    err, text, imgs = call(c, "screen", {})
    need(not err and imgs, f"MCP screen ({what})", text)
    w, h, px = decode_png(imgs[0])
    need((w, h) == (rw, rh), f"{what}: size {w}x{h} == reference")
    d = differing(px, rpx, w, box)
    need(d < w * h // 200, f"{what}: MCP screen == host reference {ref} ({d} pixels differ)")
    dw, dh, dpx = q.screendump(n)
    need((dw, dh) == (w, h) and differing(px, dpx, w) < w * h // 200, f"{what}: MCP screen == QEMU screendump")
    return px


def main():
    c = connect(300)
    q = Qmp(QMP)
    px_x, px_y = map(int, open(os.path.join(REF, "pointer.txt")).read().split())
    box = (px_x - 8, px_y - 4, px_x + 44, px_y + 48)  # Adwaita's 48 px pointer (hotspot 6,2), drawn in software
    end = time.time() + 60
    while "settings_set" not in str(rpc(c, "tools/list")):
        if time.time() > end:
            raise SystemExit("RED: no settings_set tool (settingsd not running on V2?)")
        time.sleep(1)
    print("ok: settingsd's MCP tools are there", flush=True)
    comp = start(c, "/bin/bitscomp", ["--socket", SOCK, "--fixed-status"])
    wait_output(c, comp, r"backend=fbdev size=1280x800", "bitscomp started on fbdev at 1280x800")
    wait_theme(c, r"settingsd=true", "bitscomp reads V2's settingsd", 20)
    if DARK not in ctl(c, "theme"):
        print("note: settingsd has the WY-5 defaults (CR DS-1-1 open): the design's values are set", flush=True)
        for mode, keys in DESIGN.items():
            for k, val in keys.items():
                setting(c, "settings_set", {"key": f"theme.{mode}.{k}", "value": val})
        for k, val in [("theme.accent", "#7fcfa0"), ("theme.font.ui", "Figtree"), ("theme.font.mono", "JetBrains Mono")]:
            setting(c, "settings_set", {"key": k, "value": val})
    wait_theme(c, DARK, "the default theme is Absicht dunkel", 5)
    for n, title in enumerate(["one", "two"]):
        start(c, "/bin/wyclient", ["--title", title, "--pattern", str(n)])
        end = time.time() + 20
        while f'title="{title}"' not in ctl(c, "list"):
            if time.time() > end:
                raise SystemExit(f"RED: wyclient {title} did not map\n{ctl(c, 'list')}")
            time.sleep(0.3)
    err, text, _ = call(c, "send_mouse", {"x": px_x, "y": px_y})
    need(not err, f"send_mouse to {px_x},{px_y}", text)
    dark = compare(c, q, 1, "dark.ppm", box, "dark")

    setting(c, "settings_set", {"key": "theme.mode", "value": "light"})
    ms = wait_theme(c, LIGHT, "theme.mode light: Absicht hell with its own accent", 5)
    need(ms <= 1000, f"light within 1 s ({ms} ms)")
    compare(c, q, 2, "light.ppm", box, "light")

    setting(c, "settings_set", {"key": "theme.accent", "value": "#00e5ff"})
    ms = wait_theme(c, r"mode=light accent=#00e5ff", "the user's accent (cyan) over hell", 5)
    need(ms <= 1000, f"accent within 1 s ({ms} ms)")
    compare(c, q, 3, "accent.ppm", box, "accent")

    setting(c, "settings_undo", {"n": 2})
    wait_theme(c, DARK, "settings_undo restored Absicht dunkel", 5)
    back = compare(c, q, 4, "dark.ppm", box, "undone")
    need(differing(back, dark, 1280, box, tol=0) == 0, "undo gives the first picture back exactly")

    setting(c, "settings_set", {"key": "theme.font.ui", "value": "Exo 2"})
    time.sleep(0.5)
    compare(c, q, 5, "exo.ppm", box, "Exo 2 chosen as the interface font")
    setting(c, "settings_undo", {"n": 1})
    wait_theme(c, DARK, "settings_undo restored Figtree's theme", 5)
    # WY-13: after the theme changes (new Theme fields, lazy font loads) the still desktop draws no frame.
    time.sleep(1.5)
    frames = lambda: int(re.search(r"frames=(\d+)", ctl(c, "stats")).group(1))
    f0 = frames()
    time.sleep(2.0)
    f1 = frames()
    need(f1 == f0, f"the still desktop drew 0 frames in 2 s (frames={f0}, then {f1})")
    out = ctl(c, "quit")
    print(f"ok: bitscomp ctl quit ({out.strip()[:40]})", flush=True)


if __name__ == "__main__":
    main()
