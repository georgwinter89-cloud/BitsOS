#!/usr/bin/env python3
"""IN-4 scenario: the console terminal typed on through QEMU's USB keyboard (QMP send-key = physical keys).

Usage: tty_check.py <host port> <token> <qmp socket> <run dir>
Steps: the monitor's `run /bin/abi-rawtty raw` puts the console in raw mode; a, b and q typed on the keyboard
arrive as 61 62 71, the VTIME read times out, the settings are restored and the prompt comes back; then
`run /bin/abi-rawtty wait` blocks in a canonical read and Ctrl+C ends it with SIGINT (exit code 130); the evdev
nodes of MCP's devices and of the USB keyboard and mouse exist (MCP `log`). Exit status 0 = green."""
import http.client
import json
import re
import socket
import sys
import time

PORT, TOKEN, QMP_PATH, RUN_DIR = int(sys.argv[1]), sys.argv[2], sys.argv[3], sys.argv[4]
next_id = 0

# Characters this test types -> QEMU qcodes (US key positions on the German layout: y and z swapped, '/' is
# Shift+7, '-' sits where the US slash is).
KEYS = {c: [c] for c in "abcdefghijklmnopqrstuvwx"}
KEYS.update({"y": ["z"], "z": ["y"], " ": ["spc"], "/": ["shift", "7"], "-": ["slash"], "\n": ["ret"]})


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
            rpc(c, "initialize", {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "in4", "version": "1"}})
            return c
        except (OSError, http.client.HTTPException, ValueError) as e:
            if time.time() > end:
                raise SystemExit(f"RED: no MCP answer on 127.0.0.1:{PORT}: {e}")
            time.sleep(1)


def tool(c, name, args=None):
    r = rpc(c, "tools/call", {"name": name, "arguments": args or {}})
    return "\n".join(i.get("text", "") for i in r["content"] if i["type"] == "text")


def screen_text(c):
    return tool(c, "screen", {"format": "text"})


def wait_screen(c, want, what, timeout=20):
    end = time.time() + timeout
    t = ""
    while time.time() < end:
        t = screen_text(c)
        if all(w in t for w in want):
            print(f"ok: {what}")
            return t
        time.sleep(0.3)
    raise SystemExit(f"RED: {what}: {want!r} not on the screen\n{t}")


class Qmp:
    def __init__(self, path):
        end = time.time() + 60
        while True:
            try:
                self.s = socket.socket(socket.AF_UNIX)
                self.s.settimeout(30)
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
        self.cmd("send-key", {"keys": [{"type": "qcode", "data": n} for n in names], "hold-time": 40})
        time.sleep(0.08)

    def type(self, text):
        for ch in text:
            self.keys(*KEYS[ch])


def main():
    c = connect(240)
    q = Qmp(QMP_PATH)
    wait_screen(c, ["bits>"], "the monitor's prompt")
    tool(c, "send_keys", {"text": "log level off\n"})
    time.sleep(0.5)
    q.keys("ctrl", "u")
    q.type("run /bin/abi-rawtty raw\n")
    wait_screen(c, ["raw: icanon 0 echo 0 isig 0 opost 0", "ready"], "the program set raw mode")
    for k in "abq":
        q.keys(k)
    t = wait_screen(c, ["restored"], "the program read q and restored the settings")
    # OPOST is off while the program runs: its newlines are line feeds without a carriage return (as on Linux),
    # so its lines stand in a staircase; read the hex bytes wherever they are.
    got = [w for m in re.finditer(r"got \d+:((?: [0-9a-f]{2})+)", t) for w in m.group(1).split()]
    need(got == ["61", "62", "71"], "physical keys arrived raw as 61 62 71", t)
    need("timed read = 0 after a wait" in t, "VMIN 0 VTIME 2 timed out with 0 bytes", t)
    wait_screen(c, ["bits> "], "the prompt is back")
    q.type("run /bin/abi-rawtty wait\n")
    wait_screen(c, ["waiting"], "the program waits in a canonical read")
    time.sleep(0.3)
    q.keys("ctrl", "c")
    t = wait_screen(c, ["^C", "exit code 130"], "Ctrl+C ended it with SIGINT (130)")
    need("read returned" not in t, "the read never returned data", t)
    log = tool(c, "log", {"subsystem": "input", "name": "evdev.node", "max": 200})
    for n in ("BitsOS MCP keyboard", "BitsOS MCP pointer"):
        need(n in log, f"evdev node for {n}", log)
    need(log.count(" evdev.node ") >= 4, "evdev nodes for the USB keyboard and mouse too", log)


if __name__ == "__main__":
    main()
