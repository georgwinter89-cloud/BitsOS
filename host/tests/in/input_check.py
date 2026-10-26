#!/usr/bin/env python3
"""IN-2a scenario: QMP input into QEMU's USB HID devices, read back through the MCP tool `input`.

Usage: input_check.py <host port> <token> <qmp socket>
Exit status 0 = green. Every step prints one line."""
import http.client
import json
import re
import socket
import sys
import time

PORT, TOKEN, QMP_PATH = int(sys.argv[1]), sys.argv[2], sys.argv[3]
EV_SYN, EV_KEY, EV_REL, EV_ABS = "SYN", "KEY", "REL", "ABS"
KEY_A, KEY_Q, KEY_X, KEY_LEFTSHIFT, KEY_RIGHTALT, KEY_CAPSLOCK = 30, 16, 45, 42, 100, 58
REL_X, REL_Y, REL_HWHEEL, REL_WHEEL, ABS_X, ABS_Y = 0, 1, 6, 8, 0, 1
BTN_LEFT, BTN_RIGHT, BTN_MIDDLE = 0x110, 0x111, 0x112
next_id = 0


def need(cond, what, detail=""):
    if not cond:
        raise SystemExit(f"RED: {what}\n{detail}")
    print(f"ok: {what}")


# ---- MCP (Streamable HTTP, JSON-RPC 2.0)

def post(c, body):
    c.request("POST", "/mcp", json.dumps(body), {"Authorization": f"Bearer {TOKEN}", "Content-Type": "application/json"})
    r = c.getresponse()
    data = r.read()
    return r.status, (json.loads(data) if data else None)


def rpc(c, method, params=None):
    global next_id
    next_id += 1
    st, j = post(c, {"jsonrpc": "2.0", "id": next_id, "method": method, "params": params or {}})
    if st != 200 or j is None or "result" not in j:
        raise ValueError(f"{method}: HTTP {st} {j}")
    return j["result"]


def connect(wait_s):
    end = time.time() + wait_s
    while True:
        try:
            c = http.client.HTTPConnection("127.0.0.1", PORT, timeout=90)
            rpc(c, "initialize", {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "in2a", "version": "1"}})
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


def state(text):
    devices = {}
    for m in re.finditer(r'^device (\d+) name="([^"]*)" phys=(\S+) .*? keyboard=(\w+) pointer=(\w+) keys=(\d+) rel=(\S+) abs=(\S+) leds=(\S+) .*? held=(\S+)$', text, re.M):
        devices[int(m[1])] = {"name": m[2], "phys": m[3], "keyboard": m[4] == "true", "pointer": m[5] == "true",
                              "rel": m[7], "abs": m[8], "leds": m[9], "held": m[10]}
    locks = re.search(r"^locks caps=(\w+) num=(\w+) scroll=(\w+) held=(\S+) .* next_seq=(\d+)$", text, re.M)
    events = [(int(m[1]), int(m[2]), m[3], int(m[4]), int(m[5])) for m in
              re.finditer(r"^ev (\d+) t=\S+ dev=(\d+) (\w+) code=(\d+) value=(-?\d+)$", text, re.M)]
    return devices, locks, events


def seq(c):
    return int(state(tool(c))[1][5])


def wait_events(c, since, want, what, timeout=10):
    """Waits until every (kind, code, value) of `want` came, in order, from one device (or from `dev`)."""
    end = time.time() + timeout
    got = []
    while time.time() < end:
        _, _, got = state(tool(c, {"since": since, "wait_ms": 1000, "max": 1000}))
        for dev in sorted({e[1] for e in got}):
            mine = [(k, code, v) for (_, d, k, code, v) in got if d == dev and k != "MSC"]
            it = iter(mine)
            if all(w in it for w in want):
                print(f"ok: {what} (device {dev})")
                return dev, got
    raise SystemExit(f"RED: {what}: want {want}\ngot {got}")


# ---- QMP

class Qmp:
    def __init__(self, path):
        end = time.time() + 60
        while True:
            try:
                self.s = socket.socket(socket.AF_UNIX)
                self.s.settimeout(20)
                self.s.connect(path)
                break
            except OSError:
                if time.time() > end:
                    raise SystemExit("RED: no QMP socket")
                time.sleep(0.5)
        self.f = self.s.makefile("rw")
        self.f.readline()
        self.cmd("qmp_capabilities")

    def cmd(self, name, args=None):
        self.f.write(json.dumps({"execute": name, "arguments": args or {}}) + "\n")
        self.f.flush()
        while True:
            r = json.loads(self.f.readline())
            if "return" in r:
                return r["return"]
            if "error" in r:
                raise SystemExit(f"RED: qmp {name}: {r['error']}")

    def keys(self, *names):
        self.cmd("send-key", {"keys": [{"type": "qcode", "data": n} for n in names], "hold-time": 50})

    def events(self, *evs):
        # No "device" argument: QEMU 11 aborts on it without a graphical console.
        self.cmd("input-send-event", {"events": list(evs)})


def key(name, down):
    return {"type": "key", "data": {"down": down, "key": {"type": "qcode", "data": name}}}


def btn(name, down):
    return {"type": "btn", "data": {"down": down, "button": name}}


def axis(kind, a, v):
    return {"type": kind, "data": {"axis": a, "value": v}}


def main():
    q = Qmp(QMP_PATH)
    c = connect(180)
    # Four HID devices on the second controller (plus nothing else that is HID): two keyboards with LEDs, a
    # relative and an absolute pointer.
    end = time.time() + 60
    while True:
        devs, locks, _ = state(tool(c))
        kbds = [n for n, d in devs.items() if d["keyboard"] and d["leds"] != "[]"]
        rel = [n for n, d in devs.items() if d["pointer"] and d["abs"] == "[]"]
        absd = [n for n, d in devs.items() if d["pointer"] and "0:0..32767" in d["abs"]]
        if len(kbds) >= 2 and rel and absd:
            break
        if time.time() > end:
            raise SystemExit(f"RED: HID devices not all bound: {devs}")
        time.sleep(1)
    need(True, f"input devices: keyboards {kbds}, mouse {rel}, tablet {absd}")
    text = tool(c)
    need(text.count("protocol=report") >= 4, "all four interfaces in the report protocol", text)

    s = seq(c)
    q.keys("shift", "a")
    wait_events(c, s, [(EV_KEY, KEY_LEFTSHIFT, 1), (EV_KEY, KEY_A, 1), (EV_SYN, 0, 0), (EV_KEY, KEY_A, 0), (EV_KEY, KEY_LEFTSHIFT, 0)],
                "Shift+A: press, frame, release")
    _, _, ev = state(tool(c, {"since": s, "max": 1000}))
    need(any(k == "MSC" and code == 4 and v == 0x70004 for (_, _, k, code, v) in ev), "MSC_SCAN with the HID usage 0x70004", ev)

    s = seq(c)
    q.keys("alt_r", "q")
    wait_events(c, s, [(EV_KEY, KEY_RIGHTALT, 1), (EV_KEY, KEY_Q, 1), (EV_KEY, KEY_Q, 0), (EV_KEY, KEY_RIGHTALT, 0)],
                "AltGr+Q (German @) as KEY_RIGHTALT + KEY_Q")

    s = seq(c)
    q.events(axis("rel", "x", 10), axis("rel", "y", -5))
    dev, _ = wait_events(c, s, [(EV_REL, REL_X, 10), (EV_REL, REL_Y, -5), (EV_SYN, 0, 0)], "mouse move dx=10 dy=-5")
    need(dev in rel, "the move came from the relative pointer")

    s = seq(c)
    q.events(btn("wheel-up", True))
    q.events(btn("wheel-up", False))
    wait_events(c, s, [(EV_REL, REL_WHEEL, 1)], "wheel up one notch")

    s = seq(c)
    q.events(btn("left", True))
    q.events(btn("left", False))
    wait_events(c, s, [(EV_KEY, BTN_LEFT, 1), (EV_KEY, BTN_LEFT, 0)], "left button down and up")


    s = seq(c)
    q.events(axis("abs", "x", 16384), axis("abs", "y", 8192))
    dev, _ = wait_events(c, s, [(EV_ABS, ABS_X, 16384), (EV_ABS, ABS_Y, 8192), (EV_SYN, 0, 0)], "tablet to (16384, 8192)")
    need(dev in absd, "the position came from the tablet")

    # Buttons go to the pointer QEMU polled last (input-send-event cannot pick a USB device here): with one
    # pointer unplugged, every button must come from the other. First the tablet alone, then a mouse plugged in
    # again (hot plug) alone.
    def pointers():
        devs, _, _ = state(tool(c))
        return ({n for n, d in devs.items() if d["pointer"] and d["abs"] == "[]"},
                {n for n, d in devs.items() if d["pointer"] and "0:0..32767" in d["abs"]}, devs)

    def wait_pointers(what, cond):
        end = time.time() + 15
        while True:
            r, a, devs = pointers()
            if cond(r, a):
                return r, a, devs
            if time.time() > end:
                raise SystemExit(f"RED: {what}: relative {r}, absolute {a}\n{tool(c)}")
            time.sleep(0.2)

    def buttons_and_wheels(only, who):
        for name, code in [("left", BTN_LEFT), ("right", BTN_RIGHT), ("middle", BTN_MIDDLE)]:
            s = seq(c)
            q.events(btn(name, True))
            q.events(btn(name, False))
            dev, _ = wait_events(c, s, [(EV_KEY, code, 1), (EV_KEY, code, 0)], f"{who}: {name} button down and up")
            need(dev == only, f"{who}: the {name} button came from device {only}")
        rel_caps = pointers()[2][only]["rel"].strip("[]").split(",")
        if str(REL_HWHEEL) in rel_caps:
            s = seq(c)
            for b in ["wheel-right", "wheel-left"]:
                q.events(btn(b, True))
                q.events(btn(b, False))
            dev, _ = wait_events(c, s, [(EV_REL, REL_HWHEEL, 1), (EV_REL, REL_HWHEEL, -1)], f"{who}: horizontal wheel right, then left")
            need(dev == only, f"{who}: the horizontal wheel came from device {only}")
        else:
            print(f"skip: {who} has no horizontal wheel in this QEMU (rel {rel_caps})")
        s = seq(c)
        q.events(btn("wheel-down", True))
        q.events(btn("wheel-down", False))
        dev, _ = wait_events(c, s, [(EV_REL, REL_WHEEL, -1)], f"{who}: wheel down one notch")
        need(dev == only, f"{who}: the wheel came from device {only}")

    q.cmd("device_del", {"id": "mouse1"})
    _, a, _ = wait_pointers("mouse1 unplugged", lambda r, a: not r and a)
    buttons_and_wheels(min(a), "tablet")
    q.cmd("device_add", {"driver": "usb-mouse", "bus": "xb.0", "port": "3", "id": "mouse2"})
    r, _, _ = wait_pointers("mouse2 plugged in", lambda r, a: r)
    need(True, f"mouse2 plugged in again: input device {min(r)}")
    s = seq(c)
    q.events(axis("rel", "x", -7), axis("rel", "y", 3))
    dev, _ = wait_events(c, s, [(EV_REL, REL_X, -7), (EV_REL, REL_Y, 3)], "move on the mouse plugged in again")
    need(dev in r, "the move came from mouse2")
    q.cmd("device_del", {"id": "tab1"})
    r, _, _ = wait_pointers("tab1 unplugged", lambda r, a: r and not a)
    buttons_and_wheels(min(r), "mouse")


    s = seq(c)
    q.keys("caps_lock")
    wait_events(c, s, [(EV_KEY, KEY_CAPSLOCK, 1), (EV_KEY, KEY_CAPSLOCK, 0)], "Caps Lock pressed")
    end = time.time() + 5
    while True:
        text = tool(c)
        if "locks caps=on" in text and text.count("leds=ok") >= 2:
            break
        if time.time() > end:
            raise SystemExit(f"RED: Caps Lock state or LEDs: {text}")
        time.sleep(0.2)
    need(True, "caps=on, both keyboards' LED reports sent")
    q.keys("caps_lock")
    end = time.time() + 5
    while "locks caps=off" not in tool(c):
        if time.time() > end:
            raise SystemExit("RED: Caps Lock did not turn off")
        time.sleep(0.2)
    need(True, "Caps Lock off again")

    # Unplug while a key is held: the key is released at once, by the keyboard that held it, and stays released.
    s = seq(c)
    q.events(key("x", True))
    holder, _ = wait_events(c, s, [(EV_KEY, KEY_X, 1)], "x held down")
    need(re.search(r"^locks .* held=\[45\]", tool(c), re.M) is not None, "x in the held-key union")
    def keyboards():
        devs, locks, _ = state(tool(c))
        return {n for n, d in devs.items() if d["keyboard"] and d["leds"] != "[]"}, locks

    for qid in ["kbd1", "kbd2"]:
        before, _ = keyboards()
        s = seq(c)
        q.cmd("device_del", {"id": qid})
        end = time.time() + 10
        while True:
            now, locks = keyboards()
            removed = before - now
            if removed:
                break
            if time.time() > end:
                raise SystemExit(f"RED: {qid} not removed (keyboards {now})")
            time.sleep(0.2)
        _, _, ev = state(tool(c, {"since": s, "max": 1000}))
        released = [(d, k, code, v) for (_, d, k, code, v) in ev if k == "KEY" and code == KEY_X and v == 0]
        if holder in removed:
            need(released == [(holder, "KEY", KEY_X, 0)], f"{qid} held x: KEY_X 0 from device {holder} at its unplug", ev)
            need(locks[4] == "[]", "nothing held after the unplug", tool(c))
            break
        need(not released and locks[4] == "[45]", f"{qid} did not hold x: x still held after its unplug", ev)
    else:
        raise SystemExit("RED: the holding keyboard never went away")
    time.sleep(0.5)
    devs, locks, _ = state(tool(c))
    need(locks[4] == "[]" and all(d["held"] == "[]" for d in devs.values()), "no key held anywhere at the end", tool(c))

    # Hot plug: a keyboard added after the unplug binds, gets an input device and its keys arrive.
    before, _ = keyboards()
    q.cmd("device_add", {"driver": "usb-kbd", "bus": "xb.0", "port": "1", "id": "kbd3"})
    end = time.time() + 15
    while True:
        now, _ = keyboards()
        added = now - before
        if added:
            break
        if time.time() > end:
            raise SystemExit(f"RED: the added keyboard did not appear (keyboards {now})\n{tool(c)}")
        time.sleep(0.2)
    new = added.pop()
    need(True, f"kbd3 added: input device {new}")
    s = seq(c)
    q.events(key("b", True))
    q.events(key("b", False))
    dev, _ = wait_events(c, s, [(EV_KEY, 48, 1), (EV_KEY, 48, 0)], "b typed on the added keyboard")
    need(dev == new, f"the key came from the added keyboard's device {new}")
    print("in input: every step green")


main()
