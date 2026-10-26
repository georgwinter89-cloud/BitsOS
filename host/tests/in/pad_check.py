#!/usr/bin/env python3
"""IN-2b scenario: gamepads over QEMU usb-redir (usbredir_pad.mjs), read back through the MCP tool `input`.

Usage: pad_check.py <host port> <token> <pad port 1> <pad port 2> <pad port 3>
Exit status 0 = green. Every step prints one line. Timing is never judged here (QEMU); the 1 ms rate is a
Mini-PC check."""
import http.client
import json
import os
import re
import subprocess
import sys
import time

PORT, TOKEN = int(sys.argv[1]), sys.argv[2]
PAD1, PAD2, PAD3 = (int(a) for a in sys.argv[3:6])
ABS_X, ABS_Y, ABS_MISC = 0, 1, 40
BTN_TRIGGER, BTN_THUMB2 = 0x120, 0x122
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
    j = json.loads(data) if data else None
    if r.status != 200 or j is None or "result" not in j:
        raise ValueError(f"{method}: HTTP {r.status} {j}")
    return j["result"]


def connect(wait_s):
    end = time.time() + wait_s
    while True:
        try:
            c = http.client.HTTPConnection("127.0.0.1", PORT, timeout=90)
            rpc(c, "initialize", {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "in2b", "version": "1"}})
            return c
        except (OSError, http.client.HTTPException, ValueError) as e:
            if time.time() > end:
                raise SystemExit(f"RED: no MCP answer on 127.0.0.1:{PORT}: {e}")
            time.sleep(1)


def tool(c, args=None):
    r = rpc(c, "tools/call", {"name": "input", "arguments": args or {}})
    text = "\n".join(i.get("text", "") for i in r["content"] if i["type"] == "text")
    if r.get("isError"):
        raise SystemExit(f"RED: input tool failed: {text}")
    return text


def devices(text):
    return {int(m[1]): {"id": m[2], "abs": m[3], "held": m[4]} for m in
            re.finditer(r'^device (\d+) name="[^"]*" phys=\S+ id=(\S+) .*? abs=(\S+) leds=.*? held=(\S+)$', text, re.M)}


def hid_lines(text):
    return re.findall(r"^hid .*$", text, re.M)


def events(text):
    return [(int(m[1]), m[2], int(m[3]), int(m[4])) for m in
            re.finditer(r"^ev \d+ t=\S+ dev=(\d+) (\w+) code=(\d+) value=(-?\d+)$", text, re.M)]


def next_seq(c):
    return int(re.search(r"next_seq=(\d+)", tool(c))[1])


class Pads:
    """The node side: plugs pads into QEMU's usb-redir chardevs and sets their reports."""

    def __init__(self):
        here = os.path.dirname(os.path.abspath(__file__))
        self.p = subprocess.Popen(["node", os.path.join(here, "usbredir_pad.mjs")], stdin=subprocess.PIPE,
                                  stdout=subprocess.PIPE, text=True)

    def cmd(self, **m):
        self.p.stdin.write(json.dumps(m) + "\n")
        self.p.stdin.flush()
        a = json.loads(self.p.stdout.readline())
        if "error" in a:
            raise SystemExit(f"RED: pad model: {a['error']}")
        return a

    def close(self):
        self.p.stdin.close()
        self.p.wait(timeout=10)


def wait_device(c, ident, what, timeout=60):
    end = time.time() + timeout
    while True:
        text = tool(c)
        found = [n for n, d in devices(text).items() if d["id"] == ident]
        if found:
            need(True, f"{what}: input device {found[0]}")
            return found[0], text
        if time.time() > end:
            raise SystemExit(f"RED: {what}: no input device {ident}\n{text}")
        time.sleep(1)


def wait_events(c, since, dev, want, what, timeout=15):
    end = time.time() + timeout
    got = []
    while time.time() < end:
        got = [(k, code, v) for (d, k, code, v) in events(tool(c, {"since": since, "wait_ms": 1000, "max": 1000}))
               if d == dev and k != "MSC"]
        it = iter(got)
        if all(w in it for w in want):
            need(True, what)
            return
    raise SystemExit(f"RED: {what}: want {want}\ngot {got}")


def strings_asked(pads, port):
    return [l for l in pads.cmd(cmd="state", port=port)["log"] if l.startswith("string ")]


def dragonrise_round(c, pads, nth):
    pads.cmd(cmd="plug", port=PAD1, pad="dragonrise")
    # Input device numbers are reused as Linux reuses eventN: the gone one may come back under its number.
    dev, text = wait_device(c, "0079:0006", f"DragonRise plugged in ({nth})")
    hid = [l for l in hid_lines(text) if "pad=true" in l and "binterval=10" in l]
    need(len(hid) == 1, "DragonRise interface is a pad with bInterval 10", text)
    need("pad_poll_want_us=1000" in hid[0], "DragonRise wants 1 ms polling (pad_poll_want_us=1000)", hid[0])
    need("0:0..255" in text and "16:-1..1" in text, "sticks 0..255 and the hat -1..1", text)
    need(" out_ep=0x01 " in hid[0], "its OUT endpoint is seen (not written)", hid[0])
    s = next_seq(c)
    pads.cmd(cmd="report", port=PAD1, hex="00007f7f7f4f20c0")
    wait_events(c, s, dev, [("ABS", ABS_X, 0), ("ABS", ABS_Y, 0), ("KEY", BTN_THUMB2, 1)], "stick left/up and button 3 arrive")
    s = next_seq(c)
    pads.cmd(cmd="unplug", port=PAD1)
    wait_events(c, s, dev, [("KEY", BTN_THUMB2, 0)], "unplug with buttons held releases them")
    end = time.time() + 30
    while dev in devices(tool(c)):
        if time.time() > end:
            raise SystemExit(f"RED: DragonRise input device {dev} still there after the unplug")
        time.sleep(1)
    need(True, "its input device is gone")
    return dev


def main():
    pads = Pads()
    try:
        c = connect(180)
        dragonrise_round(c, pads, 1)
        dragonrise_round(c, pads, 2)
        need(not strings_asked(pads, PAD1), "no string descriptor asked for (the DragonRise trap)")

        pads.cmd(cmd="plug", port=PAD2, pad="ds3")
        dev, text = wait_device(c, "054c:0268", "DualShock 3 plugged in")
        log = pads.cmd(cmd="state", port=PAD2)["log"]
        need(any(l.startswith("control a1 1 3f2 0 17") for l in log), "GET_REPORT of feature report 0xF2 sent", "\n".join(log))
        need(not [l for l in log if l.startswith("string ")], "no string descriptor asked for (DualShock 3)", "\n".join(log))
        report = bytearray(49)
        report[0] = 1
        report[6:10] = b"\x80" * 4
        report[2] = 0x01
        report[6] = 0
        report[10] = 0x40
        s = next_seq(c)
        pads.cmd(cmd="report", port=PAD2, hex=report.hex())
        wait_events(c, s, dev, [("KEY", BTN_TRIGGER, 1), ("ABS", ABS_X, 0), ("ABS", ABS_MISC, 0x40)],
                    "DualShock 3 reports after its wake-up: Select, stick, pressure")

        pads.cmd(cmd="plug", port=PAD3, pad="highpad")
        _, text = wait_device(c, "1209:5009", "high speed pad plugged in")
        hid = [l for l in hid_lines(text) if "binterval=7" in l]
        need(len(hid) == 1 and "poll_us=8000" in hid[0] and "pad_poll_want_us=1000" in hid[0],
             "high speed pad: bInterval 7 = 8 ms, wants 1 ms", text)

        text = tool(c, {"rate_ms": 1000})
        rates = re.findall(r"^rate .* per_s=(\S+)", text, re.M)
        need(len(rates) >= 2 and all(float(r) >= 0 for r in rates), f"report rates measured (not judged in QEMU): {rates}", text)
        for port in (PAD2, PAD3):
            pads.cmd(cmd="unplug", port=port)
        end = time.time() + 30
        while any(d["id"] in ("054c:0268", "1209:5009") for d in devices(tool(c)).values()):
            if time.time() > end:
                raise SystemExit("RED: pads still there after the unplug")
            time.sleep(1)
        need(True, "every pad unplugged, its input device gone")
    finally:
        pads.close()


main()
