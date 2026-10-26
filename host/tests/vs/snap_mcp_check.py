#!/usr/bin/env python3
"""VS-2 through MCP (QEMU): bitssnap's tools `snapshots` (read), `snapshot_take` (change), `snapshot_delete` and
`restore` (delete_overwrite) as Claude Code on the Gaming-PC calls them. bitssnap serve is started by init as
root (VS-2 change request 2; MCP's `run` is uid 1000 and could not serve them). A file of /data is snapshotted, changed, restored
through the tool, the "before-restore" snapshot appears, a path outside /data is refused; the snapshots made
here are deleted again.

Usage: snap_mcp_check.py <mcp host port> <token>
"""
import base64
import http.client
import json
import re
import sys
import time

PORT, TOKEN = int(sys.argv[1]), sys.argv[2]
next_id = 0
SAFETY = {"snapshots": "read", "snapshot_take": "change", "snapshot_delete": "delete_overwrite", "restore": "delete_overwrite"}
DIR = "/data/home/1000/vs-mcp"  # MCP's run is uid 1000


def need(cond, what, detail=""):
    if not cond:
        raise SystemExit(f"RED: {what}{': ' + str(detail)[:800] if detail else ''}")
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
            rpc(c, "initialize", {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "vs2", "version": "1"}})
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


def listed(c):
    return {t["name"]: t for t in rpc(c, "tools/list")["result"]["tools"] if t["name"] in SAFETY}


def wait_tools(c, wait_s):
    end = time.time() + wait_s
    while time.time() < end:
        t = listed(c)
        if len(t) == len(SAFETY):
            return t
        time.sleep(1)
    return listed(c)


def read_file(c, path):
    err, text = call(c, "read_file", {"path": path})
    if err:
        return None
    j = json.loads(text)
    return j.get("text") if "text" in j else base64.b64decode(j.get("data", "")).decode("utf-8", "replace")


def write_data(c, path, text):
    # The MCP file tools never write a disk (/data is on ext4): BusyBox's shell through `run` does.
    out = ok(c, "run", {"path": "/bin/busybox", "args": ["sh", "-c", 'mkdir -p "$(dirname "$1")" && printf %s "$2" > "$1"', "sh", path, text]})
    need(read_file(c, path) == text, f"{path} written through run", out)


def main():
    c = connect(300)
    # /data first (the boot makes data.img on this fresh stick).
    end = time.time() + 300
    while True:
        st = read_file(c, "/run/bitsos/data-state") or ""
        if "mounted pool rw" in st:
            break
        if time.time() > end:
            raise SystemExit(f"RED: /data never became a writable pool: {st[:300]}")
        time.sleep(2)
    print("ok: /data is a writable pool")
    tools = wait_tools(c, 60)
    need(len(tools) == len(SAFETY), "the four snapshot tools are listed", sorted(tools))
    for name, want in SAFETY.items():
        a = tools[name].get("annotations", {})
        need(a.get("owner") == "bitssnap" and a.get("safety") == want, f"{name}: owner bitssnap, safety {want}", a)

    write_data(c, f"{DIR}/note.txt", "eins\n")
    err, text = call(c, "run", {"path": "/bin/bitssnap", "args": ["take"]})
    need("errno 13" in text, "bitssnap through run (uid 1000) is refused", text)
    t = json.loads(ok(c, "snapshot_take", {"label": "vsmcp"}))
    need(t["id"] > 1 and "pause_us" in t, "snapshot_take", t)
    sid = t["id"]
    err, text = call(c, "snapshot_take", {"label": "bad label"})
    need(err, "a bad label is refused", text)
    write_data(c, f"{DIR}/note.txt", "zwei\n")

    snaps = json.loads(ok(c, "snapshots"))
    mine = [s for s in snaps["snapshots"] if s["id"] == sid]
    need(len(mine) == 1 and mine[0]["kind"] == "manual" and mine[0]["label"] == "vsmcp" and "bytes" in mine[0]
         and re.match(r"\d{4}-\d\d-\d\d \d\d:\d\d$", mine[0]["time"]), "snapshots lists it with time, kind and size", mine)
    need("free_percent" in snaps and "max_pause_us" in snaps, "free space and pauses in the answer", snaps.keys())

    r = json.loads(ok(c, "restore", {"snapshot": sid, "path": f"{DIR}/note.txt"}))
    need(r["restored"] == f"{DIR}/note.txt" and r["replaced"] and r["files"] == 1, "restore answer", r)
    text = read_file(c, f"{DIR}/note.txt")
    need(text == "eins\n", "the file is back as in the snapshot", text)
    before = r["before"]
    snaps = json.loads(ok(c, "snapshots", {"sizes": False}))
    b = [s for s in snaps["snapshots"] if s["id"] == before]
    need(len(b) == 1 and b[0]["kind"] == "before-restore" and "bytes" not in b[0], "the state before is a snapshot", b)

    err, text = call(c, "restore", {"snapshot": sid, "path": "/etc/passwd"})
    need(err and "below /data" in text, "a path outside /data is refused", text)
    err, text = call(c, "restore", {"snapshot": sid})
    need(err, "restore without a path is refused", text)

    for i in (sid, before):
        need(json.loads(ok(c, "snapshot_delete", {"snapshot": i})) == {"deleted": i}, f"snapshot_delete {i}")
    err, text = call(c, "snapshot_delete", {"snapshot": 1})
    need(err, "the volume itself is no snapshot to delete", text)
    ok(c, "run", {"path": "/bin/busybox", "args": ["rm", "-rf", DIR]})
    snaps = json.loads(ok(c, "snapshots", {"sizes": False}))
    need(not [s for s in snaps["snapshots"] if s["id"] in (sid, before)], "the test's snapshots are gone")
    print("snapshots MCP tools: green")


main()
