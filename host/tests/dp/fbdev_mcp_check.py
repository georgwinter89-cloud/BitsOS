#!/usr/bin/env python3
"""DP-2 scenario client: an fbdev program's picture == MCP `screen` PNG == QEMU's screendump, and the console back
after the program ends.

Usage: fbdev_mcp_check.py <mcp host port> <token> <qmp socket> <run dir>
Starts `/bin/abi-fbdev draw 31 3 hold` through MCP `run` (killed by the kernel after kill_after_ms), reads its
"drawn a= b= band=r+n" line, then compares the MCP `screen` PNG with the program's own pattern (computed here,
independently of the kernel) and with QMP screendump. After the kill: MCP `screen` == screendump again (console),
and no longer the pattern. Exit 0 = green."""
import base64
import http.client
import json
import os
import re
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
            rpc(c, "initialize", {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "dp2-scenario", "version": "1"}})
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
        path = os.path.join(RUN, f"fbdump{n}.ppm")
        if os.path.exists(path):
            os.remove(path)
        self.cmd("screendump", {"filename": path})
        data = open(path, "rb").read()
        parts, at = [], 0
        while len(parts) < 4:
            while data[at:at + 1].isspace():
                at += 1
            start = at
            while not data[at:at + 1].isspace():
                at += 1
            parts.append(data[start:at])
        at += 1
        w, h = int(parts[1]), int(parts[2])
        return w, h, data[at:at + w * h * 3]


def add_bytes(a, b):
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
        if ty == b"IHDR":
            w, h = int.from_bytes(data[:4], "big"), int.from_bytes(data[4:8], "big")
        elif ty == b"IDAT":
            idat += data
        elif ty == b"IEND":
            break
        at += 12 + n
    raw = zlib.decompress(idat)
    stride = w * 3
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
        else:
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


def pattern(w, h, a, b, band, n):
    """user/abi-tests/c/fbdev.c `pat` as RGB bytes, written independently of the kernel."""
    out = bytearray(w * h * 3)
    for y in range(h):
        s = b if band <= y < band + n else a
        g = (y * 5 + s * 11) & 0xFF
        o = y * w * 3
        for x in range(w):
            out[o] = (x * 3 + s * 7) & 0xFF
            out[o + 1] = g
            out[o + 2] = (x ^ y ^ s) & 0xFF
            o += 3
    return bytes(out)


def first_diff(w, a, b):
    for i in range(0, min(len(a), len(b)), 3):
        if a[i:i + 3] != b[i:i + 3]:
            return f"first difference at ({i // 3 % w}, {i // 3 // w}): {a[i:i + 3].hex()} vs {b[i:i + 3].hex()}"
    return f"lengths {len(a)} vs {len(b)}"


def shot(c):
    err, text, imgs = call(c, "screen", {})
    if err or len(imgs) != 1:
        raise SystemExit(f"RED: MCP screen gave no PNG\n{text}")
    return decode_png(imgs[0])


def main():
    c = connect(240)
    q = Qmp(QMP)
    err, text, _ = call(c, "run", {"path": "/bin/abi-fbdev", "argv": ["abi-fbdev", "draw", "31", "3", "hold"], "timeout_ms": 8000, "kill_after_ms": 25000})
    m = re.search(r"drawn a=(\d+) b=(\d+) band=(\d+)\+(\d+) top=(\d+) double=(\d)", text)
    need(m is not None, "abi-fbdev drew its frames (run answered with the 'drawn' line)", text)
    a, b, band, n = (int(m.group(i)) for i in range(1, 5))
    w, h, png = shot(c)
    want = pattern(w, h, a, b, band, n)
    need(png == want, f"MCP screen PNG ({w}x{h}) == the program's picture (double={m.group(6)})", first_diff(w, png, want))
    dw, dh, dump = q.screendump(1)
    need((dw, dh) == (w, h) and dump == png, "QEMU screendump == MCP screen PNG", first_diff(w, dump, png))
    # The kernel ends the program (kill_after_ms); the console takes the screen back.
    end = time.time() + 60
    while True:
        w2, h2, png2 = shot(c)
        if png2 != want:
            break
        if time.time() > end:
            raise SystemExit("RED: the program's picture is still on the screen 60 s later")
        time.sleep(1)
    time.sleep(1)
    w2, h2, png2 = shot(c)
    _, _, dump2 = q.screendump(2)
    need(dump2 == png2, "after the program ended: QEMU screendump == MCP screen PNG (the console)", first_diff(w2, dump2, png2))
    print("fbdev mcp: green")


main()
