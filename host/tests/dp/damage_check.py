#!/usr/bin/env python3
"""DP-6 scenario client: after bitscomp ran on V2 (host/tests/dp/40-bitscomp-damage.sh), MCP `metrics` says how
fb0 got its frames.

Usage: damage_check.py <mcp host port> <token> <damage|tracked>
damage: bitscomp reported damage (fbdev.damage.calls and .entered grew, both its windows were used: pans that
copied only the damage), and fewer write faults than one per page of a 1280x800 frame per report. tracked: no
report at all (bitscomp without the change request), the tracked path drew. Exit 0 = green."""
import http.client
import json
import sys
import time

PORT, TOKEN, WANT = int(sys.argv[1]), sys.argv[2], sys.argv[3]
PAGES_PER_FRAME = 1280 * 800 * 4 // 4096


def need(cond, what, detail=""):
    if not cond:
        raise SystemExit(f"RED: {what}\n{detail}")
    print(f"ok: {what}")


def rpc(c, method, params):
    body = {"jsonrpc": "2.0", "id": 1, "method": method, "params": params}
    c.request("POST", "/mcp", json.dumps(body), {"Authorization": f"Bearer {TOKEN}", "Content-Type": "application/json"})
    r = c.getresponse()
    data = r.read()
    if r.status != 200:
        raise SystemExit(f"RED: {method}: HTTP {r.status} {data[:200]}")
    return json.loads(data)


def metrics(c):
    j = rpc(c, "tools/call", {"name": "metrics", "arguments": {"name": "fbdev.,mm.device.", "last": 30, "max": 1, "agg": "last"}})
    if "error" in j or j["result"].get("isError"):
        raise SystemExit(f"RED: metrics: {j}")
    text = "\n".join(i.get("text", "") for i in j["result"]["content"] if i["type"] == "text")
    out = {}
    for line in text.splitlines():
        f = line.split()
        if len(f) >= 3 and f[0].startswith(("fbdev.", "mm.device.")) and f[-1] != "-":
            out[f[0]] = int(float(f[-1]))
    return out, text


def main():
    c = http.client.HTTPConnection("127.0.0.1", PORT, timeout=60)
    rpc(c, "initialize", {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "dp6-scenario", "version": "1"}})
    # The sampler takes a round every 2 s: the counters after bitscomp's end.
    time.sleep(5)
    m, text = metrics(c)
    print("  " + " ".join(f"{k}={v}" for k, v in sorted(m.items())))
    calls = m.get("fbdev.damage.calls", 0)
    if WANT == "tracked":
        need(calls == 0, "no damage report without the change request", text)
        need(m.get("fbdev.write_faults", 0) > 0, "the tracked path took bitscomp's stores", text)
        return
    need(calls > 0, "bitscomp reported its damage", text)
    need(m.get("fbdev.damage.entered", 0) >= 2, "every bitscomp switched the reports on (two runs at least)", text)
    need(m.get("fbdev.damage.pans", 0) > 0, "--fb-pan: pans copied only the reported damage", text)
    need(m.get("fbdev.write_faults", 0) < PAGES_PER_FRAME * calls, "fewer write faults than one per page per report", text)


main()
