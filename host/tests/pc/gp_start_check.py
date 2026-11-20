#!/usr/bin/env python3
"""GP-START (85-gp-start.sh): the first start of the Gaming-PC on a slow USB-2 stick without data.img, as in round 1.

Usage: gp_start_check.py <mcp port> <token> <qmp socket> <serial log>
Waits for /data (the kernel makes data.img on the throttled stick), reads settingsd's lines, turns the report page by
keys and types at the console (QMP send-key: the keyboard's own path), prints the kernel's console latency, checks
data.img is root's alone, then restarts the machine cleanly (MCP reboot; QEMU runs with -no-reboot and ends).
Exit 0 = green. GPS_BEFORE=1 only reports (the measurement of the code before GP-START)."""
import http.client
import json
import os
import re
import socket
import sys
import time

PORT, TOKEN, QMP, LOG = int(sys.argv[1]), sys.argv[2], sys.argv[3], sys.argv[4]
BEFORE = os.environ.get("GPS_BEFORE") == "1"
CREATE_MAX_S = int(os.environ.get("GPS_CREATE_MAX_S", "30"))
EXPECT_BUSY = os.environ.get("GPS_EXPECT_BUSY") == "1"
KEYS = os.environ.get("GPS_KEYS", "1") == "1"
next_id = 0
failed = False


def need(cond, what, detail=""):
    global failed
    if cond:
        print(f"  ok: {what}")
    elif BEFORE:
        print(f"  (before) not so: {what} {detail}")
    else:
        print(f"  RED: {what}\n{detail}")
        failed = True


def rpc(c, method, params=None):
    global next_id
    next_id += 1
    body = json.dumps({"jsonrpc": "2.0", "id": next_id, "method": method, "params": params or {}})
    c.request("POST", "/mcp", body, {"Authorization": f"Bearer {TOKEN}", "Content-Type": "application/json"})
    r = c.getresponse()
    data = r.read()
    j = json.loads(data) if data else None
    if r.status != 200 or not j or "result" not in j:
        raise ValueError(f"{method}: HTTP {r.status} {j}")
    return j["result"]


def connect(wait_s):
    end = time.time() + wait_s
    while True:
        try:
            c = http.client.HTTPConnection("127.0.0.1", PORT, timeout=60)
            rpc(c, "initialize", {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "gp-start", "version": "1"}})
            return c
        except (OSError, http.client.HTTPException, ValueError) as e:
            if time.time() > end:
                raise SystemExit(f"RED: no MCP answer on 127.0.0.1:{PORT}: {e}")
            time.sleep(1)


def call(_c, name, args=None):
    # A connection of its own per call (as mcp-call.mjs): the server closes an idle one during the long waits.
    r = rpc(connect(60), "tools/call", {"name": name, "arguments": args or {}})
    return r["isError"], "\n".join(i.get("text", "") for i in r["content"] if i["type"] == "text")


class Qmp:
    def __init__(self, path):
        self.s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.s.connect(path)
        self.f = self.s.makefile("rw", encoding="utf-8", newline="\n")
        json.loads(self.f.readline())
        self.cmd("qmp_capabilities")

    def cmd(self, name, args=None):
        self.f.write(json.dumps({"execute": name, "arguments": args or {}}) + "\n")
        self.f.flush()
        while True:
            m = json.loads(self.f.readline())
            if "return" in m or "error" in m:
                return m

    def key(self, name):
        r = self.cmd("send-key", {"keys": [{"type": "qcode", "data": name}], "hold-time": 40})
        if "error" in r:
            raise SystemExit(f"RED: QMP send-key: {r['error']}")


def log_text():
    with open(LOG, "rb") as f:
        return f.read().decode("utf-8", "replace")


def secs(line):
    m = re.match(r"\[\s*(\d+)\.(\d+)\]", line)
    return float(f"{m.group(1)}.{m.group(2)}") if m else None


def wait_log(fixed, wait_s):
    end = time.time() + wait_s
    while time.time() < end:
        for line in log_text().splitlines():
            if fixed in line:
                return line
        time.sleep(1)
    return None


c = connect(300)
print("--- /data on the slow stick")
start = wait_log("ext4 data.creating path=/boot/BITSOS2/data.img", 300)
need(start is not None, "the kernel makes data.img on the stick (there was none)")
made = wait_log("ext4 data.mounted at=/data", 1500)
need(made is not None and "created=true" in made, "/data is mounted from the new data.img", made or "")
created = next((l for l in log_text().splitlines() if "ext4 data.created path=/boot/BITSOS2/data.img" in l), "")
ms = int(re.search(r"ms=(\d+)", created).group(1)) if "ms=" in created else None
print(f"  data.img: creating at {secs(start or '') } s, mounted at {secs(made or '')} s, the making took {ms} ms")
need(ms is not None and ms <= CREATE_MAX_S * 1000, f"making data.img took at most {CREATE_MAX_S} s on the throttled stick", created)
grow = next((l for l in log_text().splitlines() if "ext4 data.grow" in l and "zeroed=false" in l), None)
need(grow is not None, "its clusters were allocated without writing zeros (data.grow zeroed=false)")

print("--- settingsd while data.img was made")
time.sleep(5)
if EXPECT_BUSY:
    wait_log("settingsd back_on_stick", 120)
sd = [l for l in log_text().splitlines() if "text=\"settingsd " in l or "text=settingsd " in l]
for l in sd:
    print("  " + l.strip())
need(not any("hung?" in l for l in sd), "settingsd never took the busy stick for a hung one")
need(not any("resync.failed" in l for l in sd), "no failed resync")
if EXPECT_BUSY:
    need(any("settingsd fallback reason=" in l and "busy: " in l for l in sd), "settingsd met the busy stick and said so")
    need(not any("] W " in l and "fallback" in l for l in sd), "that line is no warning")
if any("fallback" in l for l in sd):
    need(any("back_on_stick" in l for l in sd), "after its wait settingsd says in one line that the settings are on the stick")
    err, st = call(c, "run", {"path": "/bin/settings", "args": ["--json", "status"], "timeout_ms": 20000})
    print("  " + st.strip().replace("\n", "\n  "))
    need(not err and "\"place\":\"stick\"" in st.replace(" ", ""), "settingsd is on the stick again", st[:400])
err, ls = call(c, "list_dir", {"path": "/boot/BITSOS2"})
img = next((l for l in ls.splitlines() if l.rstrip().endswith(" data.img")), "")
print(f"  {img.strip()}")
need(re.match(r"\s*-0600\s+0:0\s", img) is not None, "data.img is root's alone (-0600 0:0)", ls[:800])

if not KEYS:
    try:
        call(c, "reboot")
    except (OSError, http.client.HTTPException, ValueError):
        pass
    sys.exit(1 if failed else 0)

print("--- keys on the report page and at the console (2560x1440, firmware picture)")
need(wait_log("I console report.shown", 200) is not None, "the report page is on the screen")
call(c, "latency", {"reset": True})
q = Qmp(QMP)
pages = 0
for _ in range(12):
    _, s = call(c, "screen", {"format": "text"})
    if "Seite " not in "\n".join(s.splitlines()[:4]):
        break
    q.key("spc")
    pages += 1
    time.sleep(0.6)
print(f"  {pages} keys turned the page and gave the console")
for k in ["e", "c", "h", "o", "spc", "h", "i"]:
    q.key(k)
    time.sleep(0.4)
q.key("ret")
time.sleep(1)
_, lat = call(c, "latency", {"last": 64})
for l in lat.splitlines():
    if l.startswith("console") or l.startswith("console:"):
        print("  " + l)
over = [l for l in log_text().splitlines() if "latency.over_budget path=console" in l]
print(f"  console frames over the 20 ms budget in the log: {len(over)}")
line = next((l for l in lat.splitlines() if l.startswith("console:")), "console: (none)")
m = re.search(r"max_us=([\d.]+)", line)
need(m is not None and "frames=0" not in line, "the console's frames were measured", line)

print("--- clean restart (QEMU ends with -no-reboot)")
try:
    call(c, "reboot")
except (OSError, http.client.HTTPException, ValueError):
    pass
sys.exit(1 if failed else 0)
