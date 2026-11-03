#!/usr/bin/env python3
"""VS-7 (QEMU): commit times of the /data pool with the stick throttled through QEMU's monitor.

Usage: commit_time_check.py <mcp host port> <token> <monitor socket> <serial log>
"""
import base64
import http.client
import json
import os
import re
import socket
import sys
import time

PORT, TOKEN, MON, LOG = int(sys.argv[1]), sys.argv[2], sys.argv[3], sys.argv[4]
DIR = "/data/home/1000/vs7"
WRITE_BPS, READ_BPS = 2 << 20, 16 << 20
next_id = 0


def need(cond, what, detail=""):
    if not cond:
        raise SystemExit(f"RED: {what}{': ' + str(detail)[:1500] if detail else ''}")
    print(f"ok: {what}")


def rpc(c, method, params=None):
    global next_id
    next_id += 1
    body = {"jsonrpc": "2.0", "id": next_id, "method": method, "params": params or {}}
    c.request("POST", "/mcp", json.dumps(body), {"Authorization": f"Bearer {TOKEN}", "Content-Type": "application/json"})
    r = c.getresponse()
    data = r.read()
    if r.status != 200:
        raise ValueError(f"{method}: HTTP {r.status} {data[:200]}")
    return json.loads(data)


def connect(wait_s):
    end = time.time() + wait_s
    while True:
        try:
            c = http.client.HTTPConnection("127.0.0.1", PORT, timeout=300)
            rpc(c, "initialize", {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "vs7", "version": "1"}})
            return c
        except (OSError, http.client.HTTPException, ValueError) as e:
            if time.time() > end:
                raise SystemExit(f"RED: no MCP answer on 127.0.0.1:{PORT}: {e}")
            time.sleep(1)


def call(c, name, args=None):
    j = rpc(c, "tools/call", {"name": name, "arguments": args or {}})
    if "error" in j:
        return True, f"rpc error {j['error']}"
    r = j["result"]
    return r.get("isError", False), "\n".join(i.get("text", "") for i in r["content"] if i["type"] == "text")


def hmp(cmd):
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    s.settimeout(10)
    s.connect(MON)
    time.sleep(0.3)
    s.recv(65536)
    s.sendall((cmd + "\n").encode())
    time.sleep(0.5)
    out = s.recv(65536).decode(errors="replace")
    s.close()
    return out


def sh(c, line, timeout_ms=180000):
    return call(c, "run", {"path": "sh", "argv": ["sh", "-c", line], "timeout_ms": timeout_ms})


def commits(start):
    """Commit lines of the serial log after byte offset `start`, as dicts."""
    with open(LOG, "rb") as f:
        f.seek(start)
        text = f.read().decode(errors="replace")
    out = []
    for line in text.splitlines():
        m = re.search(r"bitsvol (commit\.\w+) (.*)", line)
        if m and "pool=" in line:
            kv = dict(re.findall(r"(\w+)=(\S+)", m.group(2)))
            kv["event"] = m.group(1)
            out.append(kv)
    return out


def summary(tag, cs):
    n = lambda k: [int(x.get(k, "0")) for x in cs]
    if not cs:
        print(f"{tag}: no commits")
        return
    print(f"{tag}: {len(cs)} commits, held max {max(n('ns')) / 1e6:.1f} ms sum {sum(n('ns')) / 1e6:.1f} ms, "
          f"lock wait max {max(n('lock_wait_ns')) / 1e6:.1f} ms, io sum {sum(n('io_ns')) / 1e6:.1f} ms, "
          f"flushes {sum(n('flushes'))}, own bytes {sum(n('bytes'))}, covered {sum(n('covered'))}, "
          f"before the lock {sum(n('pre_bytes'))} bytes in {sum(n('pre_ns')) / 1e6:.1f} ms")
    for x in sorted(cs, key=lambda x: -int(x.get("ns", "0")) - int(x.get("pre_ns", "0")))[:6]:
        print(f"  {x['event']} why={x.get('why')} ns={int(x['ns']) / 1e6:.1f}ms lock_wait={int(x['lock_wait_ns']) / 1e6:.1f}ms "
              f"io={int(x['io_ns']) / 1e6:.1f}ms flush={int(x['flush_ns']) / 1e6:.1f}ms flushes={x['flushes']} "
              f"bytes={x['bytes']} covered={x['covered']} pre={int(x['pre_ns']) / 1e6:.1f}ms/{x['pre_bytes']}B "
              f"budget={int(x['budget_ns']) / 1e6:.0f}ms")


def phase(c, tag, name, mib):
    at = os.path.getsize(LOG)
    t0 = time.time()
    err, t = sh(c, f"dd if=/dev/urandom of={DIR}/{name} bs=64k count={mib * 16} conv=fsync 2>&1")
    need(not err, f"{tag}: {mib} MiB written into /data with fsync ({time.time() - t0:.1f} s)", t)
    time.sleep(8)  # the journal's next commit (5 s) and the write-back of what dd left
    cs = commits(at)
    summary(tag, cs)
    need(cs and all(k in cs[0] for k in ("ns", "lock_wait_ns", "io_ns", "flushes", "bytes", "covered", "pre_ns", "pre_bytes", "budget_ns")),
         f"{tag}: commits are logged with their fields", cs[:3])
    covered = sum(int(x["covered"]) + int(x["pre_bytes"]) for x in cs)
    need(covered >= mib << 20, f"{tag}: the commits carried the {mib} MiB ({covered} bytes)")
    big = max(cs, key=lambda x: int(x["pre_bytes"]))
    need(int(big["pre_bytes"]) >= 1 << 20 and int(big["ns"]) * 4 < int(big["pre_ns"]),
         f"{tag}: the write-back ran before the pool lock (held {int(big['ns']) / 1e6:.1f} ms, before {int(big['pre_ns']) / 1e6:.1f} ms)", big)
    slow = [x for x in cs if x["event"] == "commit.slow"]
    need(not slow, f"{tag}: no commit over its budget", slow)
    return cs


def main():
    c = connect(600)
    end = time.time() + 300
    while True:
        err, t = call(c, "read_file", {"path": "/run/bitsos/data-state"})
        try:
            state = base64.b64decode(json.loads(t)["data"]).decode(errors="replace") if not err else t
        except (ValueError, KeyError):
            state = t
        if "mounted" in state:
            break
        if time.time() > end:
            raise SystemExit(f"RED: /data never mounted: {t[:300]}")
        time.sleep(2)
    need(True, "/data mounted")
    out = hmp(f"block_set_io_throttle stick {0} {READ_BPS} {WRITE_BPS} 0 0 0")
    need("rror" not in out, "the stick is throttled (QEMU monitor)", out)
    err, t = sh(c, f"mkdir -p {DIR}")
    need(not err, "test folder", t)
    a = phase(c, "alone", "a", 4)
    # The Background reader: SCHED_IDLE-like I/O as the FS-13 preload (ionice -c3 maps to Background, FS-12).
    err, t = sh(c, "ionice -c3 cat /boot/BITSOS2/vs7-big.bin > /dev/null 2>&1 || cat /boot/BITSOS2/vs7-big.bin > /dev/null", 1000)
    print(f"note: reader started: {t[:200]}")
    time.sleep(1)
    b = phase(c, "with reader", "b", 4)
    worst = lambda cs: max(int(x["ns"]) + int(x["pre_ns"]) for x in cs)
    print(f"commit max (before + held): alone {worst(a) / 1e6:.1f} ms, with the reader {worst(b) / 1e6:.1f} ms")
    err, t = call(c, "metrics", {"name": "bitsvol.commit", "last": 4, "max": 1, "agg": "last"})
    print(t[:1500])
    need(not err and "bitsvol.commit.count" in t, "the commit counters are in MCP metrics", t)
    sh(c, f"rm -rf {DIR}")


main()
