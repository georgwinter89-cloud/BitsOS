#!/usr/bin/env python3
"""SN-2 Reload scenario driver (host/tests/sn/20-hda-reload.sh): MCP over the forwarded port, the serial log
for what the kernels say. Args: port token serial-log guest-http-port."""
import http.client
import json
import re
import sys
import time

PORT, TOKEN, LOG, HTTP = int(sys.argv[1]), sys.argv[2], sys.argv[3], int(sys.argv[4])
next_id = 0


def serial():
    with open(LOG, "rb") as f:
        return f.read().decode("utf-8", "replace")


def need(cond, what, detail=""):
    if not cond:
        raise SystemExit(f"FAIL {what}\n{detail}\n--- serial tail ---\n{serial()[-3000:]}")
    print(f"ok   {what}", flush=True)


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
            c = http.client.HTTPConnection("127.0.0.1", PORT, timeout=120)
            rpc(c, "initialize", {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "sn2-reload", "version": "1"}})
            return c
        except (OSError, http.client.HTTPException, ValueError) as e:
            if time.time() > end:
                need(False, f"MCP answers within {wait_s} s", str(e))
            time.sleep(0.5)


def call(c, name, args=None):
    r = rpc(c, "tools/call", {"name": name, "arguments": args or {}})
    return r["isError"], "\n".join(i.get("text", "") for i in r["content"] if i["type"] == "text")


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


def ktest(c, f):
    err, t = call(c, "ktest", {"filter": f, "timeout_ms": 120000})
    need(not err and "passed=1" in t and "failed=0" in t, f"ktest {f}", t)


def main():
    c = connect(240)
    ktest(c, "sn.hda.probe")
    for k in range(1, 4):
        # Reload 2 runs with the mixer (SN-3) playing a client through hda0 instead of a bare stream.
        ktest(c, "sn.mix.leave_running" if k == 2 else "sn.hda.leave_running")
        start = len(serial())
        err, t = call(c, "reload", {"url": f"http://10.0.2.2:{HTTP}/base/", "timeout_ms": 60000})
        need(not err, f"reload {k}: accepted", t)
        m, pos = wait_for(r"kexec reload\.jump reload=(\d+) devices=(\S*) (.*)", start, 60, f"reload {k}: old kernel quiesced and jumped")
        need("hda0" in m.group(2).split(","), f"reload {k}: hda0 quiesced ({m.group(2)})")
        need(" bus_master_swept=0 " in m.group(3) + " ", f"reload {k}: no function left with bus master", m.group(0))
        _, pos = wait_for(r"BitsOS V2 kernel: entry", pos, 60, f"reload {k}: new kernel entered")
        entry = pos
        _, pos = wait_for(r"snd hda\.codec ctrl=hda0 cad=0 vendor=0x1af4002\d .*missing=0 timeouts=0", pos, 60, f"reload {k}: codec read again")
        c = connect(90)
        need("hda.stale_dma" not in serial()[entry:], f"reload {k}: no running DMA found by the new kernel")
        ktest(c, "sn.hda.play")
        if k == 2:
            ktest(c, "sn.mix.resample")
    print("sn hda reload: green (3 reloads with a running stream or mixer, quiesced, re-probed, playing)")


main()
