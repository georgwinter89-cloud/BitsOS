#!/usr/bin/env python3
"""DB2 through MCP (`run` is uid 1000): the probe's run as bitsaccess answers it (C5 and the MCP tool access_log).

Usage: werhatwas_check.py <mcp host port> <token> <run dir>
"""
import http.client
import json
import os
import re
import sys
import time

PORT, TOKEN, RUN_DIR = int(sys.argv[1]), sys.argv[2], sys.argv[3]
PROBE = "/bin/access-probe"
DEVICES = "/dev/snd/pcmC0D0c,/dev/ntsync"
next_id = 0


def need(cond, what, detail=""):
    if not cond:
        raise SystemExit(f"RED: {what}{': ' + str(detail)[-3000:] if detail else ''}")
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
            c = http.client.HTTPConnection("127.0.0.1", PORT, timeout=120)
            rpc(c, "initialize", {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "db2", "version": "1"}})
            return c
        except (OSError, http.client.HTTPException, ValueError) as e:
            if time.time() > end:
                raise SystemExit(f"RED: no MCP answer on 127.0.0.1:{PORT}: {e}")
            time.sleep(1)


def text_of(j):
    return "\n".join(i.get("text", "") for i in j.get("result", {}).get("content", []) if i.get("type") == "text")


def run(c, path, args, timeout_ms=60000):
    j = rpc(c, "tools/call", {"name": "run", "arguments": {"path": path, "args": args, "timeout_ms": timeout_ms}})
    text = text_of(j)
    m = re.match(r"pid \d+ ended: exit (\d+)\n--- stdout \(\d+ bytes\) ---\n(.*)\n?--- stderr \(\d+ bytes\) ---\n(.*)", text, re.S)
    if j.get("result", {}).get("isError") or not m:
        raise SystemExit(f"RED: run {path} {args}: {text[:1500]}")
    return int(m.group(1)), m.group(2), m.group(3)


def query(c, op, **args):
    code, out, err = run(c, "/bin/bitsaccess", ["query", op] + [f"{k}={v}" for k, v in args.items()])
    if code != 0:
        return None
    j = json.loads(out)
    return j.get("result")


def serial():
    return open(os.path.join(RUN_DIR, "serial.log"), errors="replace").read()


def wait(what, secs, test):
    end = time.time() + secs
    while time.time() < end:
        v = test()
        if v:
            return v
        time.sleep(1)
    raise SystemExit(f"RED: {what} (after {secs} s)")


def settings_set(c, key, value):
    code, out, err = run(c, "/bin/settings", ["set", key, value])
    if code != 0:
        # Why settingsd has no such key: bitsaccess's own view and its log lines.
        _, mine, why = run(c, "/bin/bitsaccess", ["settings"])
        logs = text_of(rpc(c, "tools/call", {"name": "log", "arguments": {}}))
        lines = [l for l in logs.splitlines() if "bitsaccess" in l or "settingsd" in l or "bitsjournal" in l]
        print("--- bitsaccess settings:", mine, why, "--- log:", "\n".join(lines[-40:]), sep="\n")
    need(code == 0, f"settings set {key} {value}", out + err)


def main():
    c = connect(400)
    wait("bitsaccess started", 300, lambda: "started the access log /bin/bitsaccess" in serial())
    wait("bitsaccess keeps the log on /data", 300, lambda: "bitsaccess: /data/system/access:" in serial())
    wait("bitsaccess answers its socket", 120, lambda: query(c, "summary", since_s=60) is not None)
    # It reads the kernel only once settingsd answered (Georg may have switched the log off); its first batch then
    # only learns what the boot touched, so the probe must come after it to be "new".
    wait("bitsaccess reads the kernel (settingsd answered)", 120, lambda: "bitsaccess: recording on" in serial())
    time.sleep(1)
    code, said, err = run(c, PROBE, [PROBE, "/tmp/db2-scn", DEVICES])
    need(code == 0 and "done" in said, "the probe ran", said + err)
    port = int(re.search(r"^port (\d+)$", said, re.M).group(1))
    dev = re.search(r"^device (\S+)$", said, re.M).group(1)
    progs = wait("the probe in the summary", 30, lambda: (query(c, "summary", since_s=600, app="access-probe") or {}).get("programs"))
    open(os.path.join(RUN_DIR, "summary.json"), "w").write(json.dumps(progs, indent=1))
    need(len(progs) == 1 and progs[0]["app"] == PROBE, "one program, the probe", progs)
    p = progs[0]
    # Shipped in /bin, but its first device and listening port are new: shown on its own, not under "BitsOS selbst".
    need(p["new"] is True and p["system"] is False, "a program in /bin with something new is shown on its own", p)
    files = {f["folder"]: f for f in p["files"]}
    need(set(files) == {"/bin", "/tmp", "/tmp/db2-scn"}, "exactly its three folders", sorted(files))
    need(files["/bin"]["read"] == 1 and files["/bin"]["write"] == 0, "/bin read once", files["/bin"])
    need(files["/tmp"]["create"] == 1, "its folder made in /tmp", files["/tmp"])
    d = files["/tmp/db2-scn"]
    need((d["create"], d["delete"], d["rename"], d["read"], d["write"]) == (2, 2, 1, 0, 0), "in its folder: 2 made, 2 removed, 1 renamed", d)
    need(not any(f["private"] for f in p["files"]), "nothing private", p["files"])
    net = {(n["host"], n["port"]): n for n in p["net"]}
    own = net.get(("127.0.0.1", port))
    need(own is not None and (own["connect"], own["listen"], own["tx"], own["rx"]) == (1, 1, 5, 0), f"127.0.0.1:{port}: listened, connected, 5 bytes sent", p["net"])
    acc = [n for k, n in net.items() if k != ("127.0.0.1", port)]
    need(len(acc) == 1 and acc[0]["host"] == "127.0.0.1" and (acc[0]["accept"], acc[0]["rx"], acc[0]["tx"]) == (1, 5, 0), "the accepted side: 5 bytes received", p["net"])
    need(all(n["words"] == "dieser Rechner" for n in p["net"]), "loopback named as this machine", p["net"])
    want_class = "mikrofon" if dev.endswith("c") and "/snd/" in dev else "andere"
    need([(x["class"], x["paths"], x["count"]) for x in p["devices"]] == [(want_class, [dev], 1)], f"one device, {dev}", p["devices"])
    need(p["rights"] == [], "no rights raised", p["rights"])
    ev = query(c, "events", since_s=600, app="access-probe", kind="netclose")
    need(ev and len(ev["events"]) == 2 and sorted((e["tx"], e["rx"]) for e in ev["events"]) == [(0, 5), (5, 0)], "events: both closes with their bytes", ev)
    tools = rpc(c, "tools/list")
    need(any(t["name"] == "access_log" for t in tools["result"]["tools"]), "access_log listed (mcp=kernel: the token opens everything)")
    j = rpc(c, "tools/call", {"name": "access_log", "arguments": {"op": "summary", "since_s": 600, "app": "access-probe"}})
    t = json.loads(text_of(j))
    need(t["result"]["programs"][0]["files"] == p["files"], "access_log answers as the socket", t)
    code, st, _ = run(c, "/bin/cat", ["/proc/bitsos/access"])
    need(st.startswith("on=1 lost="), "/proc/bitsos/access: on", st)
    # bitsaccess registers its keys once settingsd listens (it asks every second until then).
    wait("access.enabled registered by bitsaccess", 30, lambda: run(c, "/bin/settings", ["get", "access.enabled"])[1].strip() == "true")

    settings_set(c, "access.enabled", "false")
    wait("the kernel switched off", 40, lambda: run(c, "/bin/cat", ["/proc/bitsos/access"])[1].startswith("on=0"))
    code, said, err = run(c, PROBE, [PROBE, "/tmp/db2-aus", "-"])
    need(code == 0, "the probe ran while off", said + err)
    time.sleep(3)
    off = query(c, "summary", since_s=600, app="access-probe")
    need(not any(f["folder"] == "/tmp/db2-aus" for p in off["programs"] for f in p["files"]), "nothing recorded while off", off)
    settings_set(c, "access.enabled", "true")
    wait("the kernel switched on again", 40, lambda: run(c, "/bin/cat", ["/proc/bitsos/access"])[1].startswith("on=1"))
    print("werhatwas_check: all checks passed")


main()
