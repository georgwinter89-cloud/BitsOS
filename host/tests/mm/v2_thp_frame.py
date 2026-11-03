#!/usr/bin/env python3
"""MM-13 scenario client (70-thp-frame.sh): FD-1's frame program on V2 (MCP `fetch`, then `run`), mm.thp.fault_alloc
before and after.

Usage: v2_thp_frame.py <mcp host port> <token> <frame program URL> <run dir>. Exit 0 = green."""
import http.client
import json
import re
import sys
import time

sys.dont_write_bytecode = True
PORT, TOKEN, FRAME, RUN = int(sys.argv[1]), sys.argv[2], sys.argv[3], sys.argv[4]
next_id = 0


def need(cond, what, detail=""):
    if not cond:
        raise SystemExit(f"RED: {what}\n{detail}")
    print(f"ok: {what}", flush=True)


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
            c = http.client.HTTPConnection("127.0.0.1", PORT, timeout=120)
            rpc(c, "initialize", {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "mm13", "version": "1"}})
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
    return r["isError"], "\n".join(i.get("text", "") for i in r["content"] if i["type"] == "text")


def counter(c, name):
    time.sleep(2.5)  # the sampler takes the counters every 2 s
    _, text = call(c, "metrics", {"name": name, "last": 5, "max": 1})
    for l in text.splitlines():
        m = re.match(re.escape(name) + r" \S+ (\S+)", l)
        if m:
            return 0 if m.group(1) == "-" else int(float(m.group(1)))
    return 0


def main():
    c = connect(300)
    before = counter(c, "mm.thp.fault_alloc")
    err, text = call(c, "fetch", {"url": FRAME, "path": "/tmp/frame", "mode": "0755"})
    need(not err, "fetched the frame program", text)
    err, text = call(c, "run", {"path": "/tmp/frame", "args": ["10"], "timeout_ms": 120000})
    open(f"{RUN}/frame.txt", "w").write(text)
    print(text, flush=True)
    need(not err and "exit 0" in text, "the frame program ended normally", text)
    lines = re.findall(r"copy 13\.8 MB into fresh pages ([\d.]+) ms .* into kept pages ([\d.]+) ms", text)
    need(len(lines) >= 1, "it printed its fresh/kept numbers (QEMU: not judged)", text)
    after = counter(c, "mm.thp.fault_alloc")
    print(f"mm.thp.fault_alloc {before} -> {after}", flush=True)
    need(after > before, "the fresh buffers took huge pages")


if __name__ == "__main__":
    main()
