#!/usr/bin/env python3
"""VS-6 through MCP (QEMU): the snapshot space warning. On a fresh 64 MiB pool `snapshots` has no warning; then a
file of /data is rewritten with random bytes, one manual snapshot after each rewrite (each holds its own copy),
until the pool is more than 80 % full. `snapshots` then warns and proposes the oldest manual snapshots with the
space each frees; nothing is deleted (all of them are still there after a second look). The test's snapshots
are deleted again through snapshot_delete (Georg's way) and the warning goes away.

Usage: snap_space_check.py <mcp host port> <token>
"""
import base64
import http.client
import json
import sys
import time

PORT, TOKEN = int(sys.argv[1]), sys.argv[2]
DIR = "/data/home/1000/vs6"  # MCP's run is uid 1000
STEP_MB = 4
next_id = 0


def need(cond, what, detail=""):
    if not cond:
        raise SystemExit(f"RED: {what}{': ' + str(detail)[:1200] if detail else ''}")
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
            c = http.client.HTTPConnection("127.0.0.1", PORT, timeout=180)
            rpc(c, "initialize", {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "vs6", "version": "1"}})
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


def ok(c, name, args=None):
    err, text = call(c, name, args)
    if err:
        raise SystemExit(f"RED: {name} {args}: {text[:800]}")
    return text


def snapshots(c, sizes=True):
    return json.loads(ok(c, "snapshots", {"sizes": sizes}))


def main():
    c = connect(300)
    end = time.time() + 300
    while True:
        err, text = call(c, "read_file", {"path": "/run/bitsos/data-state"})
        if not err:
            j = json.loads(text)
            text = j["text"] if "text" in j else base64.b64decode(j.get("data", "")).decode("utf-8", "replace")
        if not err and "mounted pool rw" in text:
            break
        if time.time() > end:
            raise SystemExit(f"RED: /data never became a writable pool: {text[:300]}")
        time.sleep(2)
    end = time.time() + 60
    while "snapshots" not in {t["name"] for t in rpc(c, "tools/list")["result"]["tools"]}:
        if time.time() > end:
            raise SystemExit("RED: bitssnap's tools never came")
        time.sleep(1)
    print("ok: /data is a writable pool, snapshots is listed")

    s = snapshots(c, False)
    start_used = s["used_percent"]
    need(start_used < 80 and s["warning"] is False and s["warning_text"] is None and s["proposal"] == [],
         f"a fresh pool ({start_used} % full): no warning, no proposal", s)

    mine = []
    for k in range(1, 40):
        out = ok(c, "run", {"path": "/bin/busybox", "args": ["sh", "-c",
             f'mkdir -p {DIR} && dd if=/dev/urandom of={DIR}/fill.bin bs=1048576 count={STEP_MB} 2>/dev/null && sync && echo done']})
        need("done" in out, f"step {k}: {STEP_MB} MiB rewritten", out)
        mine.append(json.loads(ok(c, "snapshot_take", {"label": f"vs6-{k}"}))["id"])
        s = snapshots(c, False)
        print(f"   {s['used_percent']} % full after {k} steps")
        if s["used_percent"] > 80:
            break
    need(s["used_percent"] > 80, "the pool is past 80 % full", s["used_percent"])

    s = snapshots(c)
    need(s["warning"] is True and "% full (warning above 80 %)" in (s["warning_text"] or ""), "the warning", s.get("warning_text"))
    p = s["proposal"]
    hand = sorted((x for x in s["snapshots"] if x["kind"] != "auto" and not x["mounted"]), key=lambda x: (x["created_s"], x["id"]))
    need(p and p[0]["id"] == hand[0]["id"], "the oldest manual snapshot is proposed first", (p, hand[:3]))
    need([x["id"] for x in p] == [x["id"] for x in hand[:len(p)]], "the proposal is the oldest ones in order", p)
    need(all(x["kind"] in ("manual", "before-restore") and "time" in x and "label" in x for x in p), "each entry: id, time, kind, label", p)
    need(all(x.get("bytes", 0) > 0 for x in p) and s["proposal_bytes"] == sum(x["bytes"] for x in p),
         "each entry says the space it frees", p)
    need(str(p[0]["id"]) in s["warning_text"] and "without Georg" in s["warning_text"], "the text names them", s["warning_text"])

    # Nothing deleted: a second look (sizes or not) still has every one, and still warns.
    s2 = snapshots(c, False)
    ids = {x["id"] for x in s2["snapshots"]}
    need(all(i in ids for i in mine), "nothing was deleted", sorted(set(mine) - ids))
    need(s2["warning"] and s2["proposal"] and s2["proposal"][0]["id"] == p[0]["id"], "without sizes the proposal is the same", s2["proposal"])

    for i in mine:
        need(json.loads(ok(c, "snapshot_delete", {"snapshot": i})) == {"deleted": i}, f"snapshot_delete {i} (the test cleans up)")
    ok(c, "run", {"path": "/bin/busybox", "args": ["rm", "-rf", DIR]})
    ok(c, "run", {"path": "/bin/busybox", "args": ["sync"]})
    s = snapshots(c, False)
    # The deleted file's blocks stay mapped in the volume (no discard): one step more than at the start.
    if start_used + 2 * STEP_MB * 100 / 64 < 80:
        need(s["warning"] is False and s["proposal"] == [], f"after the clean-up ({s['used_percent']} % full): no warning", s)
    else:
        print(f"note: {s['used_percent']} % full after the clean-up, the start was too full to require the warning gone")
    print("snapshot space warning: green")


main()
