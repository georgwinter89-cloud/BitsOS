#!/usr/bin/env python3
"""IN-3 scenario: the kernel console typed on through QEMU's USB keyboard (QMP send-key = physical keys) and
through MCP send_keys (injected keys).

Usage: console_check.py shell|monitor <host port> <token> <qmp socket> <run dir> <repo root>
shell (a normal boot: init keeps BusyBox ash on the console since UL-3a, prompt `/ # `; the monitor waits): IN-quiet:
the default console level (warn) keeps MCP's Info request lines off the screen; `echo off > /sys/bitsos/console_log`
+ clear; type a line with umlauts and AltGr characters; QEMU's screendump must equal the Rust model's picture pixel
for pixel (cargo test ... screendump_matches_model); an injected `reboot` line finds no reboot in the shell and the
machine stays up; a mouse move draws the pointer; at console_log info the request lines show and a half-typed
command stays whole below them.
monitor (CONFIG init=off: no session holds the console, so the kernel monitor `bits>` reads it): `log level off` +
clear leaves only the prompt; an injected `reboot` line is refused (R8c).
Exit status 0 = green."""
import http.client
import json
import os
import socket
import subprocess
import sys
import time

MODE, PORT, TOKEN, QMP_PATH, RUN_DIR, ROOT = sys.argv[1], int(sys.argv[2]), sys.argv[3], sys.argv[4], sys.argv[5], sys.argv[6]
next_id = 0

# Linux key code -> QEMU qcode (US positions); German characters -> (code, modifiers) as crates/console keymap.
QCODE = {2: "1", 3: "2", 4: "3", 5: "4", 6: "5", 7: "6", 8: "7", 9: "8", 10: "9", 11: "0", 12: "minus", 13: "equal",
         16: "q", 17: "w", 18: "e", 19: "r", 20: "t", 21: "y", 22: "u", 23: "i", 24: "o", 25: "p", 26: "bracket_left",
         27: "bracket_right", 28: "ret", 30: "a", 31: "s", 32: "d", 33: "f", 34: "g", 35: "h", 36: "j", 37: "k", 38: "l",
         39: "semicolon", 40: "apostrophe", 41: "grave_accent", 43: "backslash", 44: "z", 45: "x", 46: "c", 47: "v",
         48: "b", 49: "n", 50: "m", 51: "comma", 52: "dot", 53: "slash", 57: "spc", 86: "less"}
GERMAN = [(2, "1", "!", ""), (3, "2", "\"", "²"), (4, "3", "§", "³"), (5, "4", "$", ""), (6, "5", "%", ""),
          (7, "6", "&", ""), (8, "7", "/", "{"), (9, "8", "(", "["), (10, "9", ")", "]"), (11, "0", "=", "}"),
          (12, "ß", "?", "\\"), (16, "q", "Q", "@"), (17, "w", "W", ""), (18, "e", "E", "€"), (19, "r", "R", ""),
          (20, "t", "T", ""), (21, "z", "Z", ""), (22, "u", "U", ""), (23, "i", "I", ""), (24, "o", "O", ""),
          (25, "p", "P", ""), (26, "ü", "Ü", ""), (27, "+", "*", "~"), (30, "a", "A", ""), (31, "s", "S", ""),
          (32, "d", "D", ""), (33, "f", "F", ""), (34, "g", "G", ""), (35, "h", "H", ""), (36, "j", "J", ""),
          (37, "k", "K", ""), (38, "l", "L", ""), (39, "ö", "Ö", ""), (40, "ä", "Ä", ""), (41, "^", "°", ""),
          (43, "#", "'", ""), (44, "y", "Y", ""), (45, "x", "X", ""), (46, "c", "C", ""), (47, "v", "V", ""),
          (48, "b", "B", ""), (49, "n", "N", ""), (50, "m", "M", "µ"), (51, ",", ";", ""), (52, ".", ":", ""),
          (53, "-", "_", ""), (57, " ", "", ""), (86, "<", ">", "|")]
CHARS = {}
for code, p, s, a in GERMAN:
    for text, mods in ((a, ["alt_r"]), (s, ["shift"]), (p, [])):
        if text:
            CHARS[text] = (code, mods)


def need(cond, what, detail=""):
    if not cond:
        raise SystemExit(f"RED: {what}\n{detail}")
    print(f"ok: {what}")


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
            rpc(c, "initialize", {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "in3", "version": "1"}})
            return c
        except (OSError, http.client.HTTPException, ValueError) as e:
            if time.time() > end:
                raise SystemExit(f"RED: no MCP answer on 127.0.0.1:{PORT}: {e}")
            time.sleep(1)


def tool(c, name, args=None):
    r = rpc(c, "tools/call", {"name": name, "arguments": args or {}})
    return "\n".join(i.get("text", "") for i in r["content"] if i["type"] == "text"), r.get("isError", False)


def screen_text(c):
    return tool(c, "screen", {"format": "text"})[0]


def wait_screen(c, want, what, timeout=15):
    end = time.time() + timeout
    t = ""
    while time.time() < end:
        t = screen_text(c)
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
            r = json.loads(self.f.readline())
            if "return" in r:
                return r["return"]
            if "error" in r:
                raise SystemExit(f"RED: qmp {name}: {r['error']}")

    def keys(self, *names):
        self.cmd("send-key", {"keys": [{"type": "qcode", "data": n} for n in names], "hold-time": 40})
        time.sleep(0.06)

    def type(self, text):
        for ch in text:
            if ch == "\n":
                self.keys("ret")
                continue
            code, mods = CHARS[ch]
            self.keys(*mods, QCODE[code])


def screendump(q, name):
    path = os.path.join(RUN_DIR, name)
    if os.path.exists(path):
        os.remove(path)
    q.cmd("screendump", {"filename": path})
    return path


def ppm_pixel(path, x, y):
    with open(path, "rb") as f:
        data = f.read()
    parts, i = [], 0
    while len(parts) < 4:
        while data[i:i + 1].isspace():
            i += 1
        j = i
        while not data[j:j + 1].isspace():
            j += 1
        parts.append(data[i:j])
        i = j
    i += 1
    w = int(parts[1])
    o = i + (y * w + x) * 3
    return data[o] << 16 | data[o + 1] << 8 | data[o + 2]


# BusyBox ash's prompt for root in /, and the monitor's (crates/console/src/monitor.rs PROMPT).
SHELL_PROMPT = "/ # "
MONITOR_PROMPT = "bits> "
# BusyBox's line editor draws characters above CONFIG_LAST_SUPPORTED_WCHAR (user/ports/busybox: 767) as '?'
# while they are typed; the command itself gets them as typed.
LAST_SUPPORTED_WCHAR = 767


def grid(t):
    """The screen's rows from MCP's text mode (after its header line)."""
    return t.split("\n", 1)[1].split("\n") if "\n" in t else []


def only_prompt(c, prompt, typed):
    """Waits until the screen holds nothing but `prompt` (after clear)."""
    end = time.time() + 15
    t = ""
    while time.time() < end:
        t = screen_text(c)
        rows = [r for r in grid(t) if r.strip()]
        if rows == [prompt.rstrip()] and not any(w in t for w in typed):
            break
        time.sleep(0.3)
    need(rows == [prompt.rstrip()], "the screen holds only the prompt after clear", t)


def quiet_by_default(c, prompt, what):
    # IN-quiet: the console's default level is warn, so MCP's own request lines (Info) never reach the screen
    # (they are in the log all the same).
    wait_screen(c, prompt.rstrip(), what, timeout=60)
    for _ in range(3):
        t = screen_text(c)
    need("http.request" not in t, "no Info request lines on the quiet console", t)
    need("http.request" in tool(c, "log", {"name": "http.request", "max": 5})[0], "the request lines are in the log")


def shell(c, q):
    quiet_by_default(c, SHELL_PROMPT, "the console shell's prompt")
    # 1. A quiet, empty console: the shell sets the console's level through /sys/bitsos (the monitor's
    # `log level` is not reachable while the shell holds the console).
    q.type("echo off > /sys/bitsos/console_log\n")
    q.type("clear\n")
    only_prompt(c, SHELL_PROMPT, ["clear", "console_log"])
    # 2. Umlauts, sharp s and AltGr characters typed on the (QEMU USB) keyboard, quoted for the shell.
    text = "Grüße {@} [x] ~|\\ € ÄÖÜ äöü µ²³ °§ <>"
    q.type("echo '" + text + "'\n")
    # INT-A1 (IN-3 CR2): MCP's text mode reads the VT's own grid, so every character comes back as typed.
    wait_screen(c, "\n" + text + "\n", "the echoed line with umlauts and AltGr characters")
    drawn = "".join(ch if ord(ch) <= LAST_SUPPORTED_WCHAR else "?" for ch in text)
    time.sleep(0.5)
    t = screen_text(c)
    need(grid(t)[:1] == [SHELL_PROMPT + "echo '" + drawn + "'"], "the typed command line as the shell's line editor drew it", t)
    dump = screendump(q, "console.ppm")
    expect = os.path.join(RUN_DIR, "console.expect")
    with open(expect, "w", encoding="utf-8") as f:
        # INT-A1: the default face is Noto since MCP's text mode no longer needs VGA glyphs. The shell writes in
        # the console's default colour (7).
        f.write(f"noto {len(SHELL_PROMPT)} 2\n")
        f.write("7\t" + SHELL_PROMPT + "echo '" + drawn + "'\n")
        f.write("7\t" + text + "\n")
        f.write("7\t" + SHELL_PROMPT + "\n")
    env = dict(os.environ, BITSOS_SCREENDUMP=dump, BITSOS_SCREEN_EXPECT=expect)
    r = subprocess.run(["cargo", "test", "-q", "-p", "console", "--lib", "screendump_matches_model", "--", "--ignored", "--nocapture"],
                       cwd=ROOT, env=env, capture_output=True, text=True)
    need(r.returncode == 0, "QEMU screendump equals the Rust model pixel for pixel", r.stdout[-3000:] + r.stderr[-3000:])
    # 3. Injected input cannot restart the machine from the shell: it has no reboot (user/ports/busybox).
    tool(c, "send_keys", {"text": "reboot\n"})
    wait_screen(c, "reboot: not found", "an injected reboot line finds no reboot in the console shell")
    time.sleep(2)
    need(SHELL_PROMPT.strip() in screen_text(c), "the machine stayed up (MCP answers, the shell's prompt is back)")
    # 4. The pointer appears where the mouse moved it (start: screen centre).
    q.cmd("input-send-event", {"events": [{"type": "rel", "data": {"axis": "x", "value": 40}}, {"type": "rel", "data": {"axis": "y", "value": 30}}]})
    end = time.time() + 10
    ok = False
    while time.time() < end and not ok:
        p = screendump(q, "pointer.ppm")
        time.sleep(0.3)
        ok = ppm_pixel(p, 1320, 750) == 0x000000 and ppm_pixel(p, 1322, 756) == 0xFFFFFF
    need(ok, "the mouse pointer is drawn at its new place (centre + 40, 30)")
    # 5. IN-quiet: at console_log info MCP's request lines show; a command typed halfway stays whole below them.
    q.keys("ctrl", "u")  # an empty line to start from
    q.type("echo info > /sys/bitsos/console_log\n")
    q.type("echo half")
    end = time.time() + 15
    t, last = "", ""
    while time.time() < end:
        t = screen_text(c)
        lines = [l for l in t.split("\n") if l.strip()]
        last = lines[-1] if lines else ""
        if "http.request" in t and last.rstrip() == SHELL_PROMPT + "echo half":
            break
        time.sleep(0.3)
    need("http.request" in t, "request lines show at console_log info", t)
    need(last.rstrip() == SHELL_PROMPT + "echo half", "the half-typed command stays whole below the log lines", t)
    q.type(" done\n")
    wait_screen(c, "\nhalf done", "the command typed on after the log lines runs whole")
    print("in console (shell): green")


def monitor(c, q):
    quiet_by_default(c, MONITOR_PROMPT, "the monitor's prompt (init=off)")
    q.type("log level off\n")
    q.type("clear\n")
    only_prompt(c, MONITOR_PROMPT, ["clear", "log level"])
    # Injected input: the monitor's reboot is for the physical keyboard only (R8c).
    tool(c, "send_keys", {"text": "reboot\n"})
    wait_screen(c, "reboot: refused", "an injected reboot line is refused")
    print("in console (monitor): green")


def main():
    c = connect(240)
    q = Qmp(QMP_PATH)
    time.sleep(2)
    {"shell": shell, "monitor": monitor}[MODE](c, q)


main()
