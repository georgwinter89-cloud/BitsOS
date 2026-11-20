#!/usr/bin/env python3
"""GP-GUARD scenario client (host/tests/bt/65-gp-tiers.sh): MCP over QEMU's hostfwd to a BitsOS booted with the
Gaming-PC profile and gp_net=on, with the stick's own token, as the desk will reach the real machine.

Usage: gp_mcp.py <host port> <token> <readonly|stick> [seconds]
  readonly  the profile without gp_stick: MCP answers, hw_info names the profile, a write below /boot/BITSOS2 fails
  stick     with gp_stick=on: the same write works and reads back; MCP is then called once a second for [seconds]
            (the proof a trial generation needs to confirm itself)
Both end with reboot (QEMU runs with -no-reboot, so the reset ends it). Exit status 0 = green."""
import base64
import http.client
import json
import sys
import time

PORT, TOKEN, MODE = int(sys.argv[1]), sys.argv[2], sys.argv[3]
SECONDS = int(sys.argv[4]) if len(sys.argv) > 4 else 0
PATH = "/boot/BITSOS2/GPTEST.TXT"
TEXT = "GP-GUARD: written over MCP on the Gaming-PC profile's boot stick\r\n"
next_id = 0


def post(c, body):
    c.request("POST", "/mcp", json.dumps(body), {"Authorization": f"Bearer {TOKEN}", "Content-Type": "application/json"})
    r = c.getresponse()
    data = r.read()
    return r.status, (json.loads(data) if data else None)


def rpc(c, method, params=None):
    global next_id
    next_id += 1
    st, j = post(c, {"jsonrpc": "2.0", "id": next_id, "method": method, "params": params or {}})
    if st != 200 or "result" not in j:
        raise ValueError(f"{method}: HTTP {st} {j}")
    return j["result"]


def connect(wait_s):
    end = time.time() + wait_s
    while True:
        try:
            c = http.client.HTTPConnection("127.0.0.1", PORT, timeout=30)
            rpc(c, "initialize", {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "gp-guard-scenario", "version": "1"}})
            return c
        except (OSError, http.client.HTTPException, ValueError) as e:
            if time.time() > end:
                raise SystemExit(f"RED: no MCP answer on 127.0.0.1:{PORT}: {e}")
            time.sleep(1)


def call(c, name, args=None):
    r = rpc(c, "tools/call", {"name": name, "arguments": args or {}})
    return r["isError"], "\n".join(i.get("text", "") for i in r["content"] if i["type"] == "text")


def need(cond, what, detail=""):
    if not cond:
        raise SystemExit(f"RED: {what}\n{detail}")
    print(f"  ok: {what}")


c = connect(180)
need(True, "MCP answers over the network with the stick's token")
err, t = call(c, "hw_info")
need(not err and "profile=gaming-pc reason=dmi" in t, "hw_info names the Gaming-PC profile", t[:2000])
err, t = call(c, "net_info")
need(not err and "addr=10.0.2.15/24" in t, "net_info shows the lease of the cable card", t[:2000])
# A wrong token opens nothing (the profile changes nothing about that).
bad = http.client.HTTPConnection("127.0.0.1", PORT, timeout=30)
bad.request("POST", "/mcp", json.dumps({"jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": {}}),
            {"Authorization": "Bearer wrong-token-000", "Content-Type": "application/json"})
need(bad.getresponse().status in (401, 403), "a wrong token is refused")
bad.close()

err, t = call(c, "write_file", {"path": PATH, "text": TEXT})
if MODE == "readonly":
    need(err, "write_file below /boot/BITSOS2 fails: the stick is read-only without gp_stick", t)
    print("  " + t.splitlines()[0][:200])
    err, t = call(c, "read_file", {"path": PATH})
    need(err, "the file is not there", t[:300])
else:
    need(not err, "write_file below /boot/BITSOS2 works with gp_stick=on", t)
    err, t = call(c, "read_file", {"path": PATH})
    back = base64.b64decode(json.loads(t).get("data", "")).decode() if not err else ""
    need(back == TEXT, "the file reads back", t[:300])
    end = time.time() + SECONDS
    n = 0
    while time.time() < end:
        err, _ = call(c, "version")
        n += 1
        time.sleep(1)
    print(f"  ok: MCP answered {n} calls over {SECONDS} s")
# A wrong CONFIG.TXT write must stay refused whatever the stick's state (R8a; the profile adds nothing to open it).
err, t = call(c, "write_file", {"path": "/boot/BITSOS2/CONFIG.TXT", "text": "nvme_bind=on\nnvme_write=on\n"})
need(err, "write_file never replaces CONFIG.TXT on the stick", t[:300])
try:
    call(c, "reboot")
except (OSError, http.client.HTTPException, ValueError):
    pass
print("  ok: reboot sent")
