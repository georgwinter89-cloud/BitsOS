#!/usr/bin/env python3
"""UL-7: restart and power off from the console shell, driven over MCP and QMP (our own QEMU only).

Usage: power_check.py <reboot|poweroff> <mcp port> <token> <qmp socket>
QMP send-key is a USB keyboard (physical keys); MCP send_keys is injected (R8c/R8f).
- reboot: an injected `poweroff` is refused by init (R8f) and the machine stays; `poweroff` and
  `poweroff -f` from MCP's run are refused (EPERM); a program of the shell that traps SIGTERM is started by
  real keys, then `reboot` typed on the keyboard; the script returns once QEMU has gone (-no-reboot).
- poweroff: `poweroff` typed on the keyboard; returns once QEMU has gone (ACPI S5).
The shell scenario checks the serial log afterwards (order: SIGTERM first, then the reset or S5)."""
import base64
import http.client
import json
import socket
import sys
import time

MODE, PORT, TOKEN, QMP_PATH = sys.argv[1], int(sys.argv[2]), sys.argv[3], sys.argv[4]
PROMPT = "/ # "
next_id = 0

# QEMU qcodes for the characters typed here, as the console's German layout reads them (crates/console keymap).
KEYS = {c: ([], c) for c in "abcdefghijklmnoprstuvwx"}
KEYS.update({"y": ([], "z"), "z": ([], "y"), " ": ([], "spc"), ".": ([], "dot"), "-": ([], "slash"),
             "/": (["shift"], "7"), "&": (["shift"], "6"), "7": ([], "7"), "\n": ([], "ret")})


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
            rpc(c, "initialize", {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "ul7", "version": "1"}})
            return c
        except (OSError, http.client.HTTPException, ValueError) as e:
            if time.time() > end:
                raise SystemExit(f"RED: no MCP answer on 127.0.0.1:{PORT}: {e}")
            time.sleep(1)


def tool(c, name, args=None):
    r = rpc(c, "tools/call", {"name": name, "arguments": args or {}})
    return "\n".join(i.get("text", "") for i in r["content"] if i["type"] == "text"), r.get("isError", False)


def wait_screen(c, want, what, timeout=20):
    end = time.time() + timeout
    t = ""
    while time.time() < end:
        t = tool(c, "screen", {"format": "text"})[0]
        if want in t:
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
            line = self.f.readline()
            if not line:
                raise EOFError("QMP closed")
            r = json.loads(line)
            if "return" in r:
                return r["return"]
            if "error" in r:
                raise SystemExit(f"RED: qmp {name}: {r['error']}")

    def type(self, text):
        for ch in text:
            mods, code = KEYS[ch]
            self.cmd("send-key", {"keys": [{"type": "qcode", "data": n} for n in mods + [code]], "hold-time": 40})
            time.sleep(0.06)

    def gone(self, timeout):
        """True once QEMU has ended (its QMP socket closed)."""
        end = time.time() + timeout
        while time.time() < end:
            try:
                self.cmd("query-status")
            except (EOFError, OSError, ValueError):
                return True
            time.sleep(0.5)
        return False


TRAP = "trap 'echo ul7-got-term > /dev/kmsg; exit 0' TERM\nwhile :; do sleep 1; done\n"


def reboot(c, q):
    wait_screen(c, PROMPT.rstrip(), "the console shell's prompt", timeout=90)
    tool(c, "send_keys", {"text": "poweroff\n"})
    wait_screen(c, "power off refused", "an injected poweroff is refused by init (R8f)")
    t, _ = tool(c, "run", {"path": "/bin/cat", "args": ["/proc/bitsos/power"], "timeout_ms": 5000})
    need("poweroff=refused why=injected-confirm" in t, "/proc/bitsos/power names the injected Enter", t)
    time.sleep(1)
    wait_screen(c, PROMPT.rstrip(), "the machine stayed up (MCP answers, the prompt is there)")
    for args in ([], ["-f"]):
        t, _ = tool(c, "run", {"path": "/bin/poweroff", "args": args, "timeout_ms": 10000})
        need("Operation not permitted" in t, f"poweroff {' '.join(args)} from MCP's run: EPERM", t)
    t, _ = tool(c, "run", {"path": "/bin/reboot", "args": [], "timeout_ms": 10000})
    need("Operation not permitted" in t, "reboot from MCP's run (user 1000): EPERM", t)
    t, _ = tool(c, "run", {"path": "/bin/init", "args": ["--power", "poweroff"], "timeout_ms": 10000})
    need("refused user 1000 asks only through /bin/bitscomp" in t, "the power socket refuses user 1000 outside bitscomp", t)
    q.type("init --power status\n")
    wait_screen(c, "status poweroff=allowed", "root asks the power socket: a line typed on the keyboard allows")
    t, _ = tool(c, "run", {"path": "/bin/sh", "args": ["-c", "grep -H SigBlk /proc/[0-9]*/status"], "timeout_ms": 5000})
    blocked = [l for l in t.splitlines() if "SigBlk" in l and not l.startswith("/proc/1/") and int(l.split()[-1], 16) & 0x4a02]
    need(not blocked, "no program inherits init's blocked power signals", t)
    data = base64.b64encode(TRAP.encode()).decode()
    t, err = tool(c, "write_file", {"path": "/tmp/ul7-term.sh", "data": data})
    need(not err, "the SIGTERM probe written", t)
    q.type("sh /tmp/ul7-term.sh &\n")
    time.sleep(2)
    q.type("reboot\n")
    need(q.gone(120), "QEMU ended after reboot typed on the keyboard (-no-reboot)")


def poweroff(c, q):
    wait_screen(c, PROMPT.rstrip(), "the console shell's prompt", timeout=90)
    q.type("poweroff\n")
    need(q.gone(120), "QEMU ended after poweroff typed on the keyboard (ACPI S5)")


def main():
    c = connect(240)
    q = Qmp(QMP_PATH)
    time.sleep(2)
    {"reboot": reboot, "poweroff": poweroff}[MODE](c, q)
    print(f"ul power ({MODE}): green")


main()
