#!/usr/bin/env python3
"""RB-quiesce scenario driver (60-reboot-park.sh, run 5): over MCP, start busy workers on every AP
(ktest rb.quiesce.load_for_mcp, CONFIG rb_quiesce_load_s=) and then call `reboot`.

Usage: mcp_reboot.py <mcp host port> <token>. Exit 0 when both calls were answered."""
import http.client
import json
import sys
import time

PORT, TOKEN = int(sys.argv[1]), sys.argv[2]
next_id = 0


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


def call(c, name, args=None):
    r = rpc(c, "tools/call", {"name": name, "arguments": args or {}})
    return r.get("isError", False), "\n".join(i.get("text", "") for i in r["content"] if i["type"] == "text")


def connect(wait_s):
    end = time.time() + wait_s
    while True:
        try:
            c = http.client.HTTPConnection("127.0.0.1", PORT, timeout=60)
            rpc(c, "initialize", {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "rbq-scenario", "version": "1"}})
            return c
        except (OSError, http.client.HTTPException, ValueError) as e:
            if time.time() > end:
                raise SystemExit(f"RED: MCP does not answer on 127.0.0.1:{PORT} within {wait_s} s: {e}")
            time.sleep(0.5)


c = connect(150)
err, text = call(c, "ktest", {"filter": "rb.quiesce.load_for_mcp"})
print(f"ktest load: error={err}\n{text}")
if err or "passed=1" not in text.replace(" ", "").replace(":", "="):
    # The answer's wording belongs to MC; the workers' event in the serial log is what the shell checks.
    print("note: the ktest answer did not say passed=1 (checked again through the serial log)")
err, text = call(c, "reboot")
print(f"reboot: error={err}\n{text}")
sys.exit(1 if err else 0)
