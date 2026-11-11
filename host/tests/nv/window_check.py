#!/usr/bin/env python3
"""NV-2p, the boots with a keyboard (QEMU's USB keyboard over QMP = physical keys; MCP's send_keys = injected).

Usage: window_check.py setup|moved|poweroff <mcp port> <token> <qmp socket> <serial log> <serial> <part number> <first> <blocks>
setup: a boot with nvme_write=on, no nvme_data, /data on the stick's data.img. Puts a tree on /data; every way in
       that is not the keyboard is refused (MCP run of bitsdisk, MCP write_file, bitsdisk typed by MCP's send_keys, a
       wrong code); then bitsdisk at the console, typed "physically": CONFIG.TXT has the line, the pool is there, /data
       still on the stick until the restart. Then sync and a machine reset (QMP).
moved: the next boot: /data on the partition, /stickdata read-only with the tree.
poweroff: a file with no sync, then `poweroff` typed at the keyboard.
Exit status 0 = green."""
import http.client
import json
import os
import re
import socket
import sys
import time

MODE, PORT, TOKEN, QMP_PATH, LOG, SERIAL, NUMBER, FIRST, BLOCKS = sys.argv[1], int(sys.argv[2]), sys.argv[3], sys.argv[4], sys.argv[5], sys.argv[6], sys.argv[7], sys.argv[8], sys.argv[9]
CONFIG = "/boot/BITSOS2/CONFIG.TXT"
next_id = 0
# The console's keymap is German: character -> QEMU's qcode for the key in that place.
QCODE = {c: c for c in "abcdefghijklmnopqrstuvwxyz0123456789"}
QCODE.update({"y": "z", "z": "y", " ": "spc", "\n": "ret"})


def need(cond, what, detail=""):
    if not cond:
        raise SystemExit(f"RED: {what}\n{detail}")
    print(f"ok: {what}", flush=True)


def rpc(c, method, params=None):
    global next_id
    next_id += 1
    c.request("POST", "/mcp", json.dumps({"jsonrpc": "2.0", "id": next_id, "method": method, "params": params or {}}),
              {"Authorization": f"Bearer {TOKEN}", "Content-Type": "application/json"})
    r = c.getresponse()
    d = r.read()
    j = json.loads(d) if d else None
    if r.status != 200 or j is None or "result" not in j:
        raise ValueError(f"{method}: HTTP {r.status} {j}")
    return j["result"]


def connect(wait_s=180):
    end = time.time() + wait_s
    while True:
        try:
            c = http.client.HTTPConnection("127.0.0.1", PORT, timeout=300)
            rpc(c, "initialize", {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "nv2p", "version": "1"}})
            return c
        except (OSError, http.client.HTTPException, ValueError) as e:
            if time.time() > end:
                raise SystemExit(f"RED: no MCP answer on {PORT}: {e}")
            time.sleep(1)


def tool(c, name, args=None):
    r = rpc(c, "tools/call", {"name": name, "arguments": args or {}})
    return "\n".join(i.get("text", "") for i in r["content"] if i["type"] == "text"), r.get("isError", False)


def sh(c, cmd, ms=120000):
    t, err = tool(c, "run", {"path": "/bin/sh", "args": ["-c", cmd], "timeout_ms": ms})
    return t, err


def read_text(c, path):
    t, err = tool(c, "read_file", {"path": path, "encoding": "text"})
    if err:
        return None
    return json.loads(t).get("text", "")


def log_grep(pat, since=0):
    with open(LOG, "rb") as f:
        f.seek(since)
        return pat.encode() in f.read()


def wait_log(pat, since, secs, what):
    end = time.time() + secs
    while time.time() < end:
        if log_grep(pat, since):
            print(f"ok: {what}", flush=True)
            return
        time.sleep(0.2)
    raise SystemExit(f"RED: {what}: `{pat}` not in the log")


def log_size():
    with open(LOG, "rb") as f:
        return f.seek(0, 2)


def state(c):
    return read_text(c, "/run/bitsos/data-state") or ""


def wait_data(c):
    end = time.time() + 120
    while time.time() < end:
        s = state(c)
        if s.startswith(("mounted", "failed", "skipped")):
            return s
        time.sleep(0.5)
    raise SystemExit("RED: /data never decided")


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

    def type(self, text):
        for ch in text.lower():
            self.cmd("send-key", {"keys": [{"type": "qcode", "data": QCODE[ch]}], "hold-time": 40})
            time.sleep(0.06)


def inject(c, text):
    """MCP's send_keys: the keys reach the console and are marked injected."""
    t, err = tool(c, "send_keys", {"text": text})
    need(not err, "send_keys is answered", t)


def wait_screen(c, want, what, timeout=30):
    end = time.time() + timeout
    t = ""
    while time.time() < end:
        t, _ = tool(c, "screen", {"format": "text"})
        if want in t:
            print(f"ok: {what}", flush=True)
            return t
        time.sleep(0.3)
    raise SystemExit(f"RED: {what}: `{want}` not on the screen\n{t}")


def tree(c):
    """A tree of every kind of thing on /data: modes, a symlink, a hard link, an old time. /data/home and lost+found are root's."""
    t, err = sh(c, "cd /data/home/1000 && mkdir -p move/d/e && cd move && echo eins > a && chmod 640 a && ln a hard && ln -s a sym && "
                   "dd if=/dev/urandom of=d/e/big bs=1M count=5 2>/dev/null && chmod 750 d && touch -d '2021-03-04 05:06:07' a d/e; echo status=$?")
    need("status=0" in t, "the tree on /data is made", t)
    t, err = sh(c, "sync; stat -c '%u' /data/home")
    need("0" in t.split(), "/data/home is root's", t)


def setup():
    q = Qmp(QMP_PATH)
    c = connect()
    s = wait_data(c)
    need(s.startswith("mounted pool rw"), "/data on the stick's data.img, keys or no keys", s)
    t = read_text(c, "/proc/bitsos/data-window")
    need(t is not None and "why=no_nvme_data" in t, "no window at this boot", t)
    tree(c)
    before = read_text(c, CONFIG)
    # The way over the network: bitsdisk over MCP, the device itself, the file tool.
    t, err = tool(c, "run", {"path": "/bin/bitsdisk", "args": ["list"], "timeout_ms": 20000})
    need("nie ueber MCP" in t, "bitsdisk over MCP is refused", t)
    t, err = sh(c, "echo list > /dev/bitsos/disksetup; echo status=$?")
    need("status=0" not in t, "MCP's run cannot open the kernel's path, through the shell either", t)
    line = f"\nnvme_data={SERIAL}:00000000-0000-0000-0000-000000000000:{FIRST}:{BLOCKS}\n"
    t, err = tool(c, "write_file", {"path": CONFIG, "text": before + line})
    need(err and "CONFIG.TXT" in t, "write_file cannot add nvme_data", t)
    t, err = tool(c, "write_file", {"path": CONFIG, "text": line, "append": True})
    need(err and "CONFIG.TXT" in t, "write_file cannot append nvme_data", t)
    t, err = sh(c, f"echo '{line.strip()}' >> {CONFIG}; echo status=$?")
    need("status=0" not in t, "the shell cannot append nvme_data", t)
    t, err = tool(c, "settings_set", {"key": "kernel.nvme_data", "value": "x"})
    need(err or "refus" in t.lower() or "R8" in t, "settings_set cannot set it", t)
    need(read_text(c, CONFIG) == before, "CONFIG.TXT is what it was")
    # bitsdisk started at the keyboard, the code typed by MCP: injected, refused, nothing written.
    q.type("bitsdisk\n")
    wait_screen(c, "Welche Nummer", "bitsdisk asks which partition")
    screen = wait_screen(c, "[1] Partition " + NUMBER, "the raw partition is the only choice")
    need("[2]" not in screen, "nothing else is offered", screen)
    need("nicht waehlbar" in screen, "the Windows partitions are not", screen)
    q.type("1\n")
    wait_screen(c, "Zum Bestaetigen", "bitsdisk asks for the code")
    since = log_size()
    inject(c, SERIAL[-6:].lower() + "\n")
    wait_log("nvme setup.refused", since, 30, "the code typed over MCP is refused")
    wait_screen(c, "nicht von der Tastatur", "bitsdisk says so")
    need(read_text(c, CONFIG) == before, "CONFIG.TXT is what it was after the injected code")
    # Physically, but wrong.
    wait_screen(c, "/ #", "back at the shell")
    q.type("bitsdisk\n")
    wait_screen(c, "Welche Nummer", "bitsdisk again")
    q.type("1\n")
    time.sleep(0.5)
    since = log_size()
    q.type("000000\n")
    wait_log("nvme setup.refused", since, 30, "a wrong code is refused")
    need(read_text(c, CONFIG) == before, "CONFIG.TXT is what it was after the wrong code")
    # The real thing.
    wait_screen(c, "/ #", "back at the shell")
    q.type("bitsdisk\n")
    wait_screen(c, "Welche Nummer", "bitsdisk a third time")
    q.type("1\n")
    time.sleep(0.5)
    since = log_size()
    q.type(SERIAL[-6:] + "\n")
    wait_log("nvme setup.done", since, 300, "the typed code opens it: pool and ext4 made")
    wait_screen(c, "Fertig", "bitsdisk says it is done")
    after = read_text(c, CONFIG)
    want = f"nvme_data={SERIAL}:"
    need(after.startswith(before) and want in after and f":{FIRST}:{BLOCKS}" in after, "CONFIG.TXT has the line, nothing else changed", after)
    t = read_text(c, "/proc/bitsos/data-window")
    need("why=no_nvme_data" in t or "window=none" in t, "the window is closed again until the next boot", t)
    t, err = sh(c, "sync; echo synced")
    need("synced" in t, "synced", t)
    q.cmd("system_reset")


def moved():
    c = connect()
    s = wait_data(c)
    need(s == "mounted pool rw\n", "/data is mounted read-write on a pool", s)
    t = read_text(c, "/proc/bitsos/data-window")
    need(t is not None and "window=armed" in t and f"first={FIRST}" in t and f"blocks={BLOCKS}" in t, "the window is armed by itself", t)
    # /data does not wait for the stick; /stickdata follows when the stick is there.
    end = time.time() + 120
    while time.time() < end:
        t, err = sh(c, "grep -E ' /(data|stickdata) ' /proc/mounts")
        if "/stickdata" in t:
            break
        time.sleep(0.5)
    need(" /data ext4 rw" in t, "/data in /proc/mounts, read-write", t)
    need(" /stickdata ext4 ro" in t, "/stickdata in /proc/mounts, read-only", t)
    t, err = sh(c, "cat /stickdata/home/1000/move/a; echo x > /stickdata/home/1000/nope; echo status=$?; ls /stickdata")
    need("eins" in t and "status=0" not in t, "/stickdata holds the tree and refuses writes", t)
    t, err = tool(c, "run", {"path": "/bin/bitsdisk", "args": ["list"], "timeout_ms": 20000})
    need("nie ueber MCP" in t, "bitsdisk over MCP is still refused", t)
    # data.img as it stands at this boot; the script compares it after the power-off.
    t, err = sh(c, "sha256sum /boot/BITSOS2/data.img")
    m = re.search(r"\b[0-9a-f]{64}\b", t)
    need(m is not None, "data.img hashed", t)
    with open(os.path.join(os.path.dirname(LOG), "data-img.sum"), "w") as f:
        f.write(m.group(0))


def poweroff():
    q = Qmp(QMP_PATH)
    c = connect()
    t, err = sh(c, "echo 'kept over the power-off' > /data/home/1000/nv-before-poweroff; echo status=$?")
    need("status=0" in t, "a file on /data, no sync", t)
    q.type("poweroff\n")


if __name__ == "__main__":
    {"setup": setup, "moved": moved, "poweroff": poweroff}[MODE]()
    print(f"GREEN: {MODE}", flush=True)
