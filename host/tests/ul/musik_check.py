#!/usr/bin/env python3
"""GM-6m: bitsmusik in QEMU, driven over MCP (our own QEMU only; no audio device, so the kernel's mixer plays
into its null output: nothing audible anywhere).

Usage: musik_check.py <mcp port> <token>
1. A FLAC made here (a 330 Hz sine, mono 22.05 kHz, 20 s, VERBATIM subframes; TITLE=Sinus) and a list ABEND.M3U
   go to the stick's \\BITSOS2\\MUSIK\\ (write_file).
2. MCP's run (user 1000) starts `bitsmusik serve` on /run/user/1000/bitsmusik.sock; it keeps running.
3. `bitsmusik ctl` (user 1000): library shows 1 track and the list ABEND; play the list; status twice, 3 s apart:
   playing, title Sinus, position moving; stop; status stopped. Exit status 0 = green."""
import base64
import http.client
import json
import math
import struct
import sys
import time

PORT, TOKEN = int(sys.argv[1]), sys.argv[2]
SOCK = "/run/user/1000/bitsmusik.sock"
next_id = 0


def need(cond, what, detail=""):
    if not cond:
        raise SystemExit(f"RED: {what}\n{detail}")
    print(f"ok: {what}")


def rpc(c, method, params=None):
    global next_id
    next_id += 1
    c.request("POST", "/mcp", json.dumps({"jsonrpc": "2.0", "id": next_id, "method": method, "params": params or {}}),
              {"Authorization": f"Bearer {TOKEN}", "Content-Type": "application/json"})
    r = c.getresponse()
    j = json.loads(r.read() or b"null")
    if r.status != 200 or not j or "result" not in j:
        raise ValueError(f"{method}: HTTP {r.status} {j}")
    return j["result"]


def connect(wait_s):
    end = time.time() + wait_s
    while True:
        try:
            c = http.client.HTTPConnection("127.0.0.1", PORT, timeout=120)
            rpc(c, "initialize", {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "gm6m", "version": "1"}})
            return c
        except (OSError, http.client.HTTPException, ValueError) as e:
            if time.time() > end:
                raise SystemExit(f"RED: no MCP answer on 127.0.0.1:{PORT}: {e}")
            time.sleep(1)


def call(c, name, args):
    r = rpc(c, "tools/call", {"name": name, "arguments": args})
    return r.get("isError", False), "\n".join(i.get("text", "") for i in r["content"] if i["type"] == "text")


def put(c, path, data):
    step = 96 * 1024
    for off in range(0, len(data), step):
        err, t = call(c, "write_file", {"path": path, "data": base64.b64encode(data[off:off + step]).decode(), "offset": off, "parents": True})
        if err:
            raise SystemExit(f"RED: write_file {path} at {off}: {t}")


def ctl(c, line):
    err, t = call(c, "run", {"path": "/bin/bitsmusik", "args": ["ctl", "--socket", SOCK, line], "timeout_ms": 15000, "kill_after_ms": 15000})
    out = t.split("\n--- stdout (", 1)[1].split(") ---", 1)[1].split("\n--- stderr (", 1)[0] if "\n--- stdout (" in t else ""
    try:
        return json.loads(out.strip().splitlines()[0]), t
    except (ValueError, IndexError):
        return None, t


def crc8(b):
    c = 0
    for x in b:
        c ^= x
        for _ in range(8):
            c = ((c << 1) ^ 0x07) & 0xff if c & 0x80 else (c << 1) & 0xff
    return c


def crc16(b):
    c = 0
    for x in b:
        c ^= x << 8
        for _ in range(8):
            c = ((c << 1) ^ 0x8005) & 0xffff if c & 0x8000 else (c << 1) & 0xffff
    return c


def utf8(n):
    if n < 0x80:
        return bytes([n])
    if n < 0x800:
        return bytes([0xc0 | n >> 6, 0x80 | n & 0x3f])
    return bytes([0xe0 | n >> 12, 0x80 | (n >> 6) & 0x3f, 0x80 | n & 0x3f])


def flac(rate, secs, comments):
    """Mono 16-bit FLAC, blocks of 4096, VERBATIM subframes (as user/services/bitsmusik/tests/common)."""
    bs, n = 4096, int(rate * secs)
    pcm = [int(math.sin(i * 330 * 2 * math.pi / rate) * 12000) for i in range(n)]
    b = bytearray(b"fLaC" + bytes([0, 0, 0, 34]) + struct.pack(">HH", bs, bs) + bytes(6))
    b += struct.pack(">Q", rate << 44 | 0 << 41 | 15 << 36 | n) + bytes(16)
    vc = struct.pack("<I", 4) + b"test" + struct.pack("<I", len(comments))
    for x in comments:
        vc += struct.pack("<I", len(x.encode())) + x.encode()
    b += bytes([0x80 | 4]) + struct.pack(">I", len(vc))[1:] + vc
    code = {22050: 6, 44100: 9, 48000: 10}[rate]
    for k, start in enumerate(range(0, n, bs)):
        ln = min(bs, n - start)
        f = bytearray([0xff, 0xf8, (12 << 4 if ln == bs else 7 << 4) | code, 4 << 1]) + utf8(k)
        if ln != bs:
            f += struct.pack(">H", ln - 1)
        f.append(crc8(f))
        f.append(0x02)
        f += struct.pack(f">{ln}h", *pcm[start:start + ln])
        f += struct.pack(">H", crc16(f))
        b += f
    return bytes(b)


def main():
    c = connect(240)
    put(c, "/boot/BITSOS2/MUSIK/SINUS.FLAC", flac(22050, 20, ["TITLE=Sinus", "ARTIST=Prüfton", "ALBUM=Töne"]))
    put(c, "/boot/BITSOS2/MUSIK/ABEND.M3U", b"#EXTM3U\nsinus.flac\n")
    err, started = call(c, "run", {"path": "/bin/bitsmusik", "args": ["serve", "--socket", SOCK], "timeout_ms": 2000})
    need("Still running" in started and "(call " in started, "bitsmusik serve runs on as user 1000", started)
    job = int(started.split("(call ", 1)[1].split(")", 1)[0])
    lib = None
    for _ in range(40):
        lib, t = ctl(c, "library")
        if lib and lib.get("scanning") is False:
            break
        time.sleep(0.5)
    need(lib is not None and lib.get("ok") is True, "library answers", t)
    need(lib.get("tracks") == 1 and {"name": "ABEND", "tracks": 1} in lib.get("lists", []), "the stick's MUSIK folder: 1 track, list ABEND", json.dumps(lib))
    need("/boot/BITSOS2/MUSIK" in lib.get("folders", []), "the folder is the stick's", json.dumps(lib))
    r, t = ctl(c, '{"op":"play","list":"ABEND"}')
    need(r is not None and r.get("ok") is True, "play the list ABEND", t)
    time.sleep(2)
    a, t1 = ctl(c, "status")
    time.sleep(3)
    b, t2 = ctl(c, "status")
    need(a is not None and b is not None, "status answers", t1 + t2)
    need(a.get("state") == "playing" and a.get("title") == "Sinus" and a.get("list") == "ABEND", "status: playing Sinus from ABEND", json.dumps(a))
    need(b.get("state") == "playing" and b.get("position", 0) > a.get("position", 0) + 1.0, "the position moves (null output, by the clock)", json.dumps(a) + "\n" + json.dumps(b))
    need(abs((a.get("duration") or 0) - 20.0) < 0.1, "duration 20 s", json.dumps(a))
    # No audio device in this QEMU: the kernel's mixer plays into its null output (nothing audible) and says none.
    need(a.get("output") == "none" and a.get("route") == "none", "output none: the mixer's null output", json.dumps(a))
    r, t = ctl(c, "stop")
    need(r is not None and r.get("ok") is True, "stop", t)
    time.sleep(0.5)
    s, t = ctl(c, "status")
    need(s is not None and s.get("state") == "stopped", "stopped", t)
    need(s.get("list") == "ABEND" and s.get("route") in ("none", "hdmi", "jack", "usb", "auto"), "stopped: the last list and its output stay for the start card", t)
    print(f"output word in QEMU: {a['output']}, idle route: {s.get('route')}")
    # A uid-1000 service cannot write /dev/kmsg (root's, 0644 as on Linux): its B6 events are its stderr, shown
    # by the run answer (output so far) and `output` while it runs; then the job is stopped.
    _, more = call(c, "output", {"call": job})
    ev = started + "\n" + more
    print(more)
    call(c, "stop", {"call": job})
    for e in ("bitsmusik serve.start socket=/run/user/1000/bitsmusik.sock", "bitsmusik scan.done folders=1 tracks=1",
              "bitsmusik play.queue kind=list name=ABEND tracks=1", "bitsmusik pcm.open rate=22050 channels=1",
              "bitsmusik play.start title=Sinus", "bitsmusik play.stop why=stop"):
        need(e in ev, f"event: {e}", ev)
    print("ul musik: green")


main()
