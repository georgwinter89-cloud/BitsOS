#!/usr/bin/env python3
"""IN-4 scenario: the console terminal typed on through QEMU's USB keyboard (QMP send-key = physical keys).

Usage: tty_check.py shell|monitor <host port> <token> <qmp socket> <run dir>
shell (a normal boot: init keeps BusyBox ash on the console since UL-3a, prompt `/ # `): `/bin/abi-rawtty raw`
typed at the shell puts the console in raw mode; a, b and q typed on the keyboard arrive as 61 62 71, the VTIME
read times out, the settings are restored and the shell's prompt comes back; then `/bin/abi-rawtty wait` blocks in
a canonical read and Ctrl+C ends it with SIGINT (the shell's `echo $?` says 130); the evdev nodes of MCP's devices
and of the USB keyboard and mouse exist (MCP `log`).
monitor (CONFIG init=off: no session holds the console, so the kernel monitor `bits>` reads it): the same two
programs started with the monitor's `run`; the monitor reports `exit code 130` for the Ctrl+C.
Exit status 0 = green."""
import http.client
import json
import re
import socket
import sys
import time

MODE, PORT, TOKEN, QMP_PATH, RUN_DIR = sys.argv[1], int(sys.argv[2]), sys.argv[3], sys.argv[4], sys.argv[5]
next_id = 0

# Characters this test types -> QEMU qcodes (US key positions on the German layout: y and z swapped, '/' is
# Shift+7, '-' sits where the US slash is).
KEYS = {c: [c] for c in "abcdefghijklmnopqrstuvwx"}
KEYS.update({"y": ["z"], "z": ["y"], " ": ["spc"], "/": ["shift", "7"], "-": ["slash"], "\n": ["ret"],
             "$": ["shift", "4"], "?": ["shift", "minus"], "=": ["shift", "0"]})


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


# BusyBox ash's prompt for root in /, and the monitor's (crates/console/src/monitor.rs PROMPT), as console_check.py.
SHELL_PROMPT = "/ # "
MONITOR_PROMPT = "bits> "


def last_row(t):
    """The last non-empty screen row from MCP's text mode (after its header line)."""
    rows = [r for r in (t.split("\n", 1)[1].split("\n") if "\n" in t else []) if r.strip()]
    return rows[-1].rstrip() if rows else ""


def wait_prompt(c, prompt, what, timeout=20):
    """Waits until the prompt is the last line on the screen (a fresh prompt after a program ended)."""
    end = time.time() + timeout
    t = ""
    while time.time() < end:
        t = screen_text(c)
        if last_row(t) == prompt.rstrip():
            print(f"ok: {what}")
            return t
        time.sleep(0.3)
    raise SystemExit(f"RED: {what}: the last line is not {prompt.rstrip()!r}\n{t}")


def raw_and_ctrl_c(c, q, prompt, start):
    q.type(start + "/bin/abi-rawtty raw\n")
    wait_screen(c, ["raw: icanon 0 echo 0 isig 0 opost 0", "ready"], "the program set raw mode")
    for k in "abq":
        q.keys(k)
    t = wait_screen(c, ["restored"], "the program read q and restored the settings")
    # OPOST is off while the program runs: its newlines are line feeds without a carriage return (as on Linux),
    # so its lines stand in a staircase; read the hex bytes wherever they are.
    got = [w for m in re.finditer(r"got \d+:((?: [0-9a-f]{2})+)", t) for w in m.group(1).split()]
    need(got == ["61", "62", "71"], "physical keys arrived raw as 61 62 71", t)
    need("timed read = 0 after a wait" in t, "VMIN 0 VTIME 2 timed out with 0 bytes", t)
    wait_prompt(c, prompt, "the prompt is back")
    q.type(start + "/bin/abi-rawtty wait\n")
    wait_screen(c, ["waiting"], "the program waits in a canonical read")
    time.sleep(0.3)
    q.keys("ctrl", "c")
    return wait_screen(c, ["^C"], "Ctrl+C was echoed")


def shell(c, q):
    wait_screen(c, [SHELL_PROMPT.rstrip()], "the console shell's prompt", timeout=60)
    time.sleep(1)
    # The console's default level (warn) keeps Info lines off the screen already; off makes the screen the
    # programs' own (the monitor's `log level` is not reachable while the shell holds the console).
    tool(c, "send_keys", {"text": "echo off > /sys/bitsos/console_log\n"})
    wait_prompt(c, SHELL_PROMPT, "the shell took the console_log line")
    t = raw_and_ctrl_c(c, q, SHELL_PROMPT, "")
    need("read returned" not in t, "the read never returned data", t)
    wait_prompt(c, SHELL_PROMPT, "the shell's prompt is back after Ctrl+C")
    q.type("echo status=$?\n")
    wait_screen(c, ["status=130"], "the shell saw exit status 130 (SIGINT)")


def monitor(c, q):
    wait_screen(c, [MONITOR_PROMPT.rstrip()], "the monitor's prompt (init=off)", timeout=60)
    tool(c, "send_keys", {"text": "log level off\n"})
    time.sleep(0.5)
    q.keys("ctrl", "u")
    raw_and_ctrl_c(c, q, MONITOR_PROMPT, "run ")
    t = wait_screen(c, ["^C", "exit code 130"], "Ctrl+C ended it with SIGINT (130)")
    need("read returned" not in t, "the read never returned data", t)


def main():
    c = connect(240)
    q = Qmp(QMP_PATH)
    {"shell": shell, "monitor": monitor}[MODE](c, q)
    log = tool(c, "log", {"subsystem": "input", "name": "evdev.node", "max": 200})
    for n in ("BitsOS MCP keyboard", "BitsOS MCP pointer"):
        need(n in log, f"evdev node for {n}", log)
    need(log.count(" evdev.node ") >= 4, "evdev nodes for the USB keyboard and mouse too", log)
    print(f"in tty ({MODE}): green")


if __name__ == "__main__":
    main()
