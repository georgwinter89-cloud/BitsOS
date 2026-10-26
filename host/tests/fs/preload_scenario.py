#!/usr/bin/env python3
"""FS-13: the start-up preload over MCP on a running V2 (host/tests/fs/52-preload.sh). Boot 1: a program reads
/boot/BITSOS2/preload-big.bin cold, its session ends and the record is saved on /data. Reload: the new kernel's
preload reads the recorded ranges before anything else; the program's second start reads (almost) nothing from
the stick (usb.msd.read_bytes). Then the fs.preload ktests, the status line and `preload clear`.

FS-15: the reader is BusyBox copied to the stick (/boot/BITSOS2/fs15/busybox): what its cold start read from the stick
is in the record (its exec's own head read included), and after the Reload nothing of it is read again.

Arguments: <mcp port> <token> <serial log> <http port> <file bytes> <reader exe bytes>"""
import http.client
import json
import re
import sys
import time

PORT, TOKEN, LOG, HTTP, SIZE, EXE_SIZE = int(sys.argv[1]), sys.argv[2], sys.argv[3], sys.argv[4], int(sys.argv[5]), int(sys.argv[6])
BIG = "/boot/BITSOS2/preload-big.bin"
EXE = "/boot/BITSOS2/fs15/busybox"
# Fixer F4: started like Brave (`run /bin/sh /boot/BITSOS2/bin/brave`): sh runs a launcher script on the stick that
# exec's the program, so exec's own reads happen while the process still records for sh.
LAUNCH = "/boot/BITSOS2/fs15/launch"
READER = f"sh {LAUNCH} (exec {EXE} cat {BIG} > /dev/null)"
next_id = 0


def need(cond, what, detail=""):
    if not cond:
        try:
            tail = open(LOG, "rb").read()[-3000:].decode("utf-8", "replace")
        except OSError:
            tail = ""
        raise SystemExit(f"RED: {what}\n{detail}\n--- serial tail ---\n{tail}")
    print(f"ok: {what}", flush=True)


def rpc(c, method, params=None):
    global next_id
    next_id += 1
    c.request("POST", "/mcp", json.dumps({"jsonrpc": "2.0", "id": next_id, "method": method, "params": params or {}}),
              {"Authorization": f"Bearer {TOKEN}", "Content-Type": "application/json"})
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
            c = http.client.HTTPConnection("127.0.0.1", PORT, timeout=90)
            rpc(c, "initialize", {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "fs13-preload", "version": "1"}})
            return c
        except (OSError, http.client.HTTPException, ValueError) as e:
            if time.time() > end:
                need(False, f"MCP answers on 127.0.0.1:{PORT} within {wait_s} s", str(e))
            time.sleep(0.5)


def call(c, name, args=None):
    r = rpc(c, "tools/call", {"name": name, "arguments": args or {}})
    return r.get("isError", False), "\n".join(i.get("text", "") for i in r["content"] if i["type"] == "text")


def serial():
    with open(LOG, "rb") as f:
        return f.read().decode("utf-8", "replace")


def wait_for(pattern, start, timeout_s, what):
    end = time.time() + timeout_s
    rx = re.compile(pattern)
    while True:
        text = serial()[start:]
        text = text[:text.rfind("\n") + 1]
        m = rx.search(text)
        if m:
            return m, start + m.end()
        if time.time() > end:
            need(False, f"{what} within {timeout_s} s", f"pattern: {pattern}")
        time.sleep(0.2)


def run_reader(c):
    err, t = call(c, "run", {"path": "sh", "argv": ["sh", LAUNCH], "timeout_ms": 120000})
    need(not err and "ended: exit 0" in t, f"`{READER}` ran", t[:400])


def msd_bytes(c):
    """usb.msd.read_bytes now (the metrics sampler runs every 2 s: wait for a round after the last read)."""
    time.sleep(2.5)
    err, t = call(c, "metrics", {"name": "usb.msd.read_bytes", "last": 3, "max": 1, "agg": "last"})
    for line in t.splitlines():
        if line.startswith("usb.msd.read_bytes "):
            vals = [v for v in line.split()[2:] if v != "-"]
            if vals:
                return int(float(vals[-1]))
    need(False, "metrics shows usb.msd.read_bytes", t[:600])


def status(c):
    err, t = call(c, "preload")
    need(not err and "state=" in t, "MCP preload answers", t[:400])
    return dict(kv.split("=", 1) for kv in t.strip().splitlines()[-1].split() if "=" in kv)


def main():
    c = connect(240)
    _, pos = wait_for(r"ext4 data\.mounted at=/data pool=true writable=true", 0, 240, "/data mounted at boot")
    m, pos = wait_for(r"vfs preload\.done programs=(\d+) files=(\d+) mb=(\d+) pages=(\d+)", 0, 120, "boot 1: the preload pass ran")
    need(m.group(4) == "0", f"boot 1: a new record, nothing preloaded ({m.group(0)})")
    s = status(c)
    need(s.get("state") == "done" and s.get("enabled") == "on" and s.get("load") == "new", f"boot 1 status: {s}")

    b0 = msd_bytes(c)
    start = len(serial())
    run_reader(c)
    b1 = msd_bytes(c)
    need(b1 - b0 >= SIZE * 0.9, f"boot 1: the cold read came from the stick ({(b1 - b0) >> 20} MiB for {SIZE >> 20} MiB)")
    m, _ = wait_for(r"vfs preload\.saved programs=(\d+) pages=(\d+)", start, 60, "boot 1: the record was saved after the window")
    need(int(m.group(1)) >= 1 and int(m.group(2)) >= SIZE // 4096 * 0.9, f"boot 1: the record holds the file ({m.group(0)})")
    s = status(c)
    need(int(s["record_programs"]) >= 1 and int(s["starts"]) >= 1, f"boot 1 status after the read: {s}")
    # FS-15: the reader's session holds what its cold start read from the stick (the exec's head read before the
    # hook included); what is left is FAT metadata (directories, the FAT) and other readers.
    m = re.search(r"vfs preload\.session exe=" + re.escape(EXE) + r" files=(\d+) pages=(\d+)", serial()[start:])
    need(m is not None, f"boot 1: a preload.session line for {EXE}")
    rec_bytes = int(m.group(2)) * 4096
    print(f"FS-15 boot 1: read from the stick {b1 - b0} bytes, recorded for {EXE} {rec_bytes} bytes ({m.group(0)}); status {s}")
    need(rec_bytes >= (b1 - b0) - (256 << 10), f"boot 1: the record holds the cold start's reads ({rec_bytes} recorded, {b1 - b0} read)")
    need(all(k in s for k in ("unknown_pages", "dropped_pages", "other_pages", "kthread_pages", "stale", "renewed")), f"status has the FS-15 counts: {s}")

    start = len(serial())
    err, t = call(c, "reload", {"url": f"http://10.0.2.2:{HTTP}/base/"})
    need(not err and "reloading once this answer is out" in t, "reload: answered", t[:300])
    m, pos = wait_for(r"kexec reload\.synced reload=(\d+) flushed=(\d+) failed=(\d+)", start, 60, "reload: written back before the stop")
    need(m.group(3) == "0", f"reload: sync_all failed=0 ({m.group(0)})")
    _, pos = wait_for(r"BitsOS V2 kernel: entry", pos, 60, "reload: the new kernel entered")
    m, pos2 = wait_for(r"vfs preload\.loaded programs=(\d+) pages=(\d+)", pos, 120, "reload: the record was loaded from /data")
    m, pos2 = wait_for(r"vfs preload\.done programs=(\d+) files=(\d+) mb=(\d+) pages=(\d+) reads=(\d+) ms=(\d+) stopped=(\w+)", pos, 120, "reload: the preload pass ran")
    need(int(m.group(4)) >= SIZE // 4096 * 0.9 and m.group(7) == "end", f"reload: the preload read the recorded file ({m.group(0)})")
    c = connect(120)
    s = status(c)
    need(s.get("state") == "done" and s.get("load") == "ok", f"after the reload: status {s}")

    b2 = msd_bytes(c)
    run_reader(c)
    b3 = msd_bytes(c)
    print(f"FS-15 after the reload: the second start read {b3 - b2} bytes from the stick")
    need(b3 - b2 <= 64 << 10, f"after the reload: the second start read {(b3 - b2) >> 10} KiB from the stick (the preload had {SIZE >> 20} MiB and {EXE}'s pages in the cache)")

    err, t = call(c, "ktest", {"filter": "fs.preload."})
    need(not err and re.search(r"passed\W+[3-9]", t) and re.search(r"failed\W+0", t), "fs.preload.* ktests pass in the preloaded kernel (Interactive read during a pass within the limit)", t[:800])

    start = len(serial())
    err, t = call(c, "preload", {"clear": True})
    need(not err and "record cleared" in t, "MCP preload clear answered", t[:300])
    # FS-13b: a save of the reader's session may come between `start` and the clear (the ktests take longer now):
    # the empty record is the first save after the service's `preload.cleared`.
    _, after = wait_for(r"vfs preload\.cleared", start, 30, "the service cleared the record")
    m, _ = wait_for(r"vfs preload\.saved programs=(\d+)", after, 30, "the cleared record was saved")
    need(m.group(1) == "0", f"the stored record is empty ({m.group(0)})")
    s = status(c)
    need(s["record_programs"] == "0", f"status after clear: {s}")


if __name__ == "__main__":
    main()
