#!/usr/bin/env python3
"""GP-DISK scenario helper (host/tests/nv/45-gp-ssd.sh): the Gaming-PC's move onto its own SSD partitions in QEMU.

Usage: gp_ssd_check.py report <qmp socket> <serial log>
         waits for the profile's report page on the console and pages it away with the space bar.
       gp_ssd_check.py watch <qmp socket> <serial log> <out.json> <drive id>...
         from bitsboot's first line on (`stick: device path`), the block statistics of the named drives (QEMU's
         query-blockstats: reads, writes, flushes, discards); <out.json> holds the snapshot at that line and the last
         answer before QEMU went away. The unnamed NVMe drives must show no request after the snapshot.
       gp_ssd_check.py umzug <mcp port> <token> <qmp socket> <serial log>
         `bitsdisk umzug` typed at QEMU's USB keyboard (refused over MCP), the tree of window_check.py's setup on the
         new /data, the marker; a second run refuses; `poweroff` typed.
       gp_ssd_check.py ssd <mcp port> <token> <esp first> <esp blocks> <data first> <data blocks>
         booted from the SSD's ESP: /boot is that partition and writable, /data the window's pool, hw_info names the
         profile, the report page says which drive has the driver.
Exit status 0 = green."""
import http.client
import json
import os
import re
import socket
import sys
import time

MODE = sys.argv[1]
next_id = 0
QCODE = {c: c for c in "abcdefghijklmnopqrstuvwxyz0123456789"}
QCODE.update({"y": "z", "z": "y", " ": "spc", "\n": "ret"})


def need(cond, what, detail=""):
    if not cond:
        raise SystemExit(f"RED: {what}\n{detail}")
    print(f"ok: {what}", flush=True)


class Qmp:
    def __init__(self, path, wait_s=120):
        end = time.time() + wait_s
        while True:
            try:
                self.s = socket.socket(socket.AF_UNIX)
                self.s.settimeout(30)
                self.s.connect(path)
                break
            except OSError:
                if time.time() > end:
                    raise SystemExit(f"RED: no QMP socket {path}")
                time.sleep(0.2)
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
        for ch in text.lower():
            self.cmd("send-key", {"keys": [{"type": "qcode", "data": QCODE[ch]}], "hold-time": 40})
            time.sleep(0.06)


def watch(path, log, out, drives):
    """xtask may start QEMU again when OVMF stalls (a new process, the same socket path, the log written anew): the
    socket is opened only once the log shows bitsboot, so the snapshot is of the QEMU that runs it."""
    while True:
        try:
            with open(log, "rb") as f:
                if b"stick: device path" in f.read():
                    break
        except OSError:
            pass
        time.sleep(0.05)
    q = Qmp(path, 30)
    stats = lambda: {b["device"]: {k: b["stats"][k] for k in ("rd_operations", "wr_operations", "flush_operations", "unmap_operations")}
                     for b in q.cmd("query-blockstats") if b.get("device") in drives}
    snap = stats()
    last = snap
    while True:
        with open(out, "w") as f:
            json.dump({"at_loader": snap, "last": last}, f)
        time.sleep(0.3)
        try:
            last = stats()
        except (EOFError, OSError, ValueError):
            break
    with open(out, "w") as f:
        json.dump({"at_loader": snap, "last": last}, f)


def report(qmp, log):
    """The profile's report page takes the console after about 8 s and pages with any key (GP-ROUND): typed keys
    meant for the shell would be eaten. Wait for it, then page it away with the space bar as Georg does."""
    def seen(what):
        try:
            with open(log, "rb") as f:
                return what in f.read()
        except OSError:
            return False
    end = time.time() + 300
    while not seen(b"console report.shown"):
        if time.time() > end:
            raise SystemExit("RED: the report page never showed")
        time.sleep(0.2)
    q = Qmp(qmp)
    for _ in range(40):
        if seen(b"console report.ended"):
            print("ok: the report page is paged away", flush=True)
            return
        q.type(" ")
        time.sleep(1)
    raise SystemExit("RED: the report page does not end with the space bar")


def rpc(c, token, method, params=None):
    global next_id
    next_id += 1
    c.request("POST", "/mcp", json.dumps({"jsonrpc": "2.0", "id": next_id, "method": method, "params": params or {}}),
              {"Authorization": f"Bearer {token}", "Content-Type": "application/json"})
    r = c.getresponse()
    d = r.read()
    j = json.loads(d) if d else None
    if r.status != 200 or j is None or "result" not in j:
        raise ValueError(f"{method}: HTTP {r.status} {j}")
    return j["result"]


class Mcp:
    def __init__(self, port, token, wait_s=240):
        self.token = token
        end = time.time() + wait_s
        while True:
            try:
                self.c = http.client.HTTPConnection("127.0.0.1", port, timeout=300)
                rpc(self.c, token, "initialize", {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "gp-ssd", "version": "1"}})
                return
            except (OSError, http.client.HTTPException, ValueError) as e:
                if time.time() > end:
                    raise SystemExit(f"RED: no MCP answer on {port}: {e}")
                time.sleep(1)

    def tool(self, name, args=None):
        r = rpc(self.c, self.token, "tools/call", {"name": name, "arguments": args or {}})
        return "\n".join(i.get("text", "") for i in r["content"] if i["type"] == "text"), r.get("isError", False)

    def sh(self, cmd, ms=120000):
        return self.tool("run", {"path": "/bin/sh", "args": ["-c", cmd], "timeout_ms": ms})

    def read(self, path):
        t, err = self.tool("read_file", {"path": path, "encoding": "text"})
        return None if err else json.loads(t).get("text", "")

    def screen(self, want, what, timeout=60):
        end = time.time() + timeout
        t = ""
        while time.time() < end:
            t, _ = self.tool("screen", {"format": "text"})
            if want in t:
                print(f"ok: {what}", flush=True)
                return t
            time.sleep(0.3)
        raise SystemExit(f"RED: {what}: `{want}` not on the screen\n{t}")


def umzug(port, token, qmp):
    q = Qmp(qmp)
    m = Mcp(port, token)
    t, _ = m.tool("run", {"path": "/bin/bitsdisk", "args": ["umzug"], "timeout_ms": 20000})
    need("nie ueber MCP" in t, "bitsdisk umzug over MCP is refused", t)
    need(m.read("/data/.stickdata-moved") is None, "no marker before the move")
    m.screen("/ #", "the console's shell")
    q.type("bitsdisk umzug\n")
    screen = m.screen("Fertig: ", "bitsdisk umzug says it is done", 600)
    need("Nicht fertig" not in screen, "no difference", screen)
    t = m.read("/data/home/1000/move/a")
    need(t == "eins\n", "the tree from the stick is on the new /data", repr(t))
    t, _ = m.sh("stat -c '%a %Y' /data/home/1000/move/a /stickdata/home/1000/move/a; ls -l /data/home/1000/move/sym; cat /data/.stickdata-moved")
    lines = [l for l in t.split("\n") if re.fullmatch(r"[0-7]{3,4} [0-9]+", l.strip())]
    need(len(lines) == 2 and lines[0] == lines[1] and lines[0].startswith("640 "), "rights and time kept", t)
    need("-> a" in t and "moved from /stickdata" in t, "the link and the marker", t)
    t, _ = m.tool("data_move", {})
    need("already moved" in t, "the desk's data_move agrees: already moved", t)
    m.screen("/ #", "back at the shell")
    q.type("bitsdisk umzug\n")
    m.screen("schon uebernommen", "a second run refuses")
    m.sh("sync")
    q.type("poweroff\n")


def ssd(port, token, esp_first, esp_n, data_first, data_n):
    m = Mcp(port, token)
    t, _ = m.sh("grep -E ' /(boot|data) ' /proc/mounts")
    need("/dev/nvme0n1p2 /boot " in t, "/boot is the SSD's ESP", t)
    need(" /data ext4 rw" in t, "/data is mounted read-write", t)
    t = m.read("/proc/bitsos/data-window")
    need(t is not None and "window=armed" in t and f"first={data_first}" in t and f"boot_first={esp_first} boot_blocks={esp_n}" in t, "both windows of the SSD are open", t)
    end = time.time() + 120
    while time.time() < end:
        s = m.read("/run/bitsos/data-state") or ""
        if s.startswith("mounted pool rw"):
            break
        time.sleep(0.5)
    need(s.startswith("mounted pool rw"), "/data is the window's pool", s)
    t = m.read("/data/home/1000/move/a")
    need(t == "eins\n", "the moved tree is there after the restart", repr(t))
    t, _ = m.tool("write_file", {"path": "/boot/BITSOS2/GPSSD.TXT", "text": "GP-DISK: written on the SSD's ESP\r\n"})
    need("written" in t.lower() or "bytes" in t.lower(), "a file below \\BITSOS2\\ of the ESP", t)
    t, err = m.tool("write_file", {"path": "/boot/GPSSD.TXT", "text": "x"})
    need(err, "nothing outside \\BITSOS2\\", t)
    end = time.time() + 180
    while True:
        t, _ = m.tool("hw_info", {"section": "report"})
        if "Platten" in t and "not ready yet" not in t or time.time() > end:
            break
        time.sleep(2)
    need("Gestartet von der SSD" in t, "the report says it started from the SSD", t)
    need("Treiber an (in CONFIG.TXT mit gp_nvme freigegeben)" in t, "the report names the drive with the driver", t)
    need(t.count("kein Treiber, nicht gelesen (nicht in gp_nvme genannt)") == 2, "and the two without one", t)
    t, _ = m.tool("hw_info", {})
    need("profile=gaming-pc" in t or "gaming-pc" in t, "hw_info names the profile", t[:400])


if __name__ == "__main__":
    a = sys.argv
    if MODE == "watch":
        watch(a[2], a[3], a[4], set(a[5:]))
    elif MODE == "report":
        report(a[2], a[3])
    elif MODE == "umzug":
        umzug(int(a[2]), a[3], a[4])
    elif MODE == "ssd":
        ssd(int(a[2]), a[3], *[int(x) for x in a[4:8]])
    else:
        raise SystemExit(f"unknown mode {MODE}")
    print(f"GREEN: {MODE}", flush=True)
