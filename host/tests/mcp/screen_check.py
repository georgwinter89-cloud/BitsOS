#!/usr/bin/env python3
"""MC-3 scenario client: compares MCP `screen` with QEMU's own picture of the display (QMP screendump), pixel for
pixel (full frame, a region, a scaled region), reads the console as text, records size and time, and sends keys
and mouse input (every event injected).

Usage: screen_check.py <mcp host port> <token> <qmp socket> <run dir>
Exit status 0 = green. Every step prints one line."""
import base64
import http.client
import json
import os
import socket
import sys
import time
import zlib
from itertools import accumulate

PORT, TOKEN, QMP, RUN = int(sys.argv[1]), sys.argv[2], sys.argv[3], sys.argv[4]
next_id = 0


def need(cond, what, detail=""):
    if not cond:
        raise SystemExit(f"RED: {what}\n{detail}")
    print(f"ok: {what}")


# ---- MCP ----

def post(c, body):
    c.request("POST", "/mcp", json.dumps(body), {"Authorization": f"Bearer {TOKEN}", "Content-Type": "application/json"})
    r = c.getresponse()
    data = r.read()
    return r.status, (json.loads(data) if data else None)


def rpc(c, method, params=None):
    global next_id
    next_id += 1
    st, j = post(c, {"jsonrpc": "2.0", "id": next_id, "method": method, "params": params or {}})
    if st != 200:
        raise ValueError(f"{method}: HTTP {st} {j}")
    return j


def connect(wait_s):
    end = time.time() + wait_s
    while True:
        try:
            c = http.client.HTTPConnection("127.0.0.1", PORT, timeout=60)
            rpc(c, "initialize", {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "mc3-scenario", "version": "1"}})
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


# ---- QMP ----

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
        self.read()  # greeting
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


def read_ppm(path):
    data = open(path, "rb").read()
    parts, at = [], 0
    while len(parts) < 4:  # P6, width, height, maxval (no comments from QEMU)
        while data[at:at + 1].isspace():
            at += 1
        start = at
        while not data[at:at + 1].isspace():
            at += 1
        parts.append(data[start:at])
    at += 1
    if parts[0] != b"P6" or parts[3] != b"255":
        raise SystemExit(f"RED: {path} is not a P6 PPM with maxval 255")
    w, h = int(parts[1]), int(parts[2])
    return w, h, data[at:at + w * h * 3]


# ---- PNG (8-bit RGB, as screen writes it; filters 0-4) ----

def add_bytes(a, b):
    """Bytewise (a + b) mod 256 of two equal-length byte strings, without a Python loop per byte."""
    n = len(a)
    x, y = int.from_bytes(a, "little"), int.from_bytes(b, "little")
    lo = int.from_bytes(b"\x7f" * n, "little")
    hi = int.from_bytes(b"\x80" * n, "little")
    return (((x & lo) + (y & lo)) ^ ((x ^ y) & hi)).to_bytes(n, "little")


def decode_png(png):
    assert png[:8] == b"\x89PNG\r\n\x1a\n", "no PNG signature"
    at, idat, w, h = 8, b"", 0, 0
    while True:
        n = int.from_bytes(png[at:at + 4], "big")
        ty, data = png[at + 4:at + 8], png[at + 8:at + 8 + n]
        assert zlib.crc32(ty + data) == int.from_bytes(png[at + 8 + n:at + 12 + n], "big"), f"CRC of {ty}"
        if ty == b"IHDR":
            w, h = int.from_bytes(data[:4], "big"), int.from_bytes(data[4:8], "big")
            assert data[8:] == bytes([8, 2, 0, 0, 0]), "not 8-bit RGB"
        elif ty == b"IDAT":
            idat += data
        elif ty == b"IEND":
            break
        at += 12 + n
    raw = zlib.decompress(idat)
    stride = w * 3
    assert len(raw) == (stride + 1) * h, "picture data size"
    rows, prev = [], bytes(stride)
    for y in range(h):
        f, line = raw[y * (stride + 1)], raw[y * (stride + 1) + 1:(y + 1) * (stride + 1)]
        if f == 0:
            cur = line
        elif f == 2:
            cur = add_bytes(line, prev)
        elif f == 1:
            out = bytearray(stride)
            for k in range(3):
                out[k::3] = bytes(map((255).__and__, accumulate(line[k::3])))
            cur = bytes(out)
        else:  # 3, 4: never written by BitsOS; slow but correct
            out = bytearray(stride)
            for i in range(stride):
                a = out[i - 3] if i >= 3 else 0
                b, c = prev[i], (prev[i - 3] if i >= 3 else 0)
                if f == 3:
                    p = (a + b) // 2
                else:
                    pp = a + b - c
                    pa, pb, pc = abs(pp - a), abs(pp - b), abs(pp - c)
                    p = a if pa <= pb and pa <= pc else (b if pb <= pc else c)
                out[i] = (line[i] + p) & 255
            cur = bytes(out)
        rows.append(cur)
        prev = cur
    return w, h, b"".join(rows)


def crop(w, rgb, x, y, cw, ch):
    return b"".join(rgb[((y + r) * w + x) * 3:((y + r) * w + x + cw) * 3] for r in range(ch))


def scale_down(w, h, rgb, s):
    ow, oh = (w + s - 1) // s, (h + s - 1) // s
    out = bytearray(ow * oh * 3)
    for oy in range(oh):
        ys = range(oy * s, min(oy * s + s, h))
        for ox in range(ow):
            xs = range(ox * s, min(ox * s + s, w))
            n = len(ys) * len(xs)
            for k in range(3):
                t = sum(rgb[(y * w + x) * 3 + k] for y in ys for x in xs)
                out[(oy * ow + ox) * 3 + k] = (t + n // 2) // n
    return ow, oh, bytes(out)


def matching_shot(c, q, args, cut, what):
    """screendump, screen, screendump: the PNG must equal one of the two dumps (the console may draw between)."""
    for attempt in range(6):
        w1, h1, d1 = q.screendump(2 * attempt)
        t = time.time()
        err, text, imgs = call(c, "screen", args)
        ms = (time.time() - t) * 1000
        w2, h2, d2 = q.screendump(2 * attempt + 1)
        need(not err and len(imgs) == 1, f"{what}: screen answered with one PNG", text)
        w, h, rgb = decode_png(imgs[0])
        if rgb in (cut(w1, h1, d1), cut(w2, h2, d2)):
            return w, h, w1, h1, text, ms, len(imgs[0])
        print(f"   {what}: picture changed during try {attempt + 1}, again")
        time.sleep(0.5)
    raise SystemExit(f"RED: {what}: the PNG never equalled QEMU's screendump in 6 tries")


def main():
    c = connect(120)
    q = Qmp(QMP)
    tools = {t["name"]: t for t in rpc(c, "tools/list")["result"]["tools"]}
    for name, safety in {"screen": "read", "send_keys": "change", "send_mouse": "change"}.items():
        t = tools.get(name)
        need(t is not None and t["annotations"]["owner"] == "MC" and t["annotations"]["safety"] == safety, f"tools/list has {name} (MC, {safety})", t)

    w, h, fw, fh, text, ms, size = matching_shot(c, q, {}, lambda w, h, d: d, "full frame")
    need((w, h) == (fw, fh), f"PNG {w}x{h} has the screendump's size")
    print(f"   size/time: {text}; round trip {ms:.0f} ms, {size} bytes PNG ({size / (w * h * 3) * 100:.1f} % of raw)")
    rx, ry, rw, rh = fw // 5, fh // 4, fw // 3, 90
    matching_shot(c, q, {"x": rx, "y": ry, "width": rw, "height": rh}, lambda w, h, d: crop(w, d, rx, ry, rw, rh), "region")
    sx, sy, sw, sh = 0, 0, 640, 200

    def scaled(w, h, d):
        return scale_down(sw, sh, crop(w, d, sx, sy, sw, sh), 2)[2]

    matching_shot(c, q, {"x": sx, "y": sy, "width": sw, "height": sh, "scale": 2}, scaled, "region scaled by 2")
    err, text, imgs = call(c, "screen", {"x": fw})
    need(err and not imgs, "a region outside the frame is refused", text)

    err, text, _ = call(c, "screen", {"format": "text"})
    need(not err and text.startswith("console ") and len(text.splitlines()) > 1, "screen text mode returns the console grid", text[:400])
    print("   " + "\n   ".join(text.splitlines()[:4]))

    err, text, _ = call(c, "send_keys", {"text": "echo {}\n", "keys": ["ctrl+alt+shift+d", "f1"]})
    need(not err and "injected" in text, "send_keys: every event injected", text)
    err, text, _ = call(c, "send_mouse", {"x": 10, "y": 20, "button": "left", "action": "double"})
    need(not err and "injected" in text, "send_mouse: every event injected", text)
    err, text, _ = call(c, "send_keys", {"text": "x", "injected": False})
    need(err, "send_keys refuses an injected:false argument", text)
    c.close()


main()
