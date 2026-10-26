#!/usr/bin/env python3
"""CL-2 on V2 through MCP: settingsd (in the image, started by init as uid 0 since INT-W3)
registers settings_list/describe/get/set/undo/history with the kernel (MC-6); every key is set, read back and
undone through the tools, R8 switches are refused with CONFIG.TXT byte-identical, a confirm-free journal shows
source mcp, and when settingsd is killed its tools go with it and come back from init's restart (a new service_pid).

Usage: settings_mcp_check.py <mcp host port> <token> <settingsd binary> <run dir>
"""
import base64
import http.client
import json
import re
import sys
import time

PORT, TOKEN, BIN, RUN_DIR = int(sys.argv[1]), sys.argv[2], sys.argv[3], sys.argv[4]
SD = "/bin/settingsd"  # in the image since INT-W3; init (pid 1) starts and restarts it
TOOLS = ["settings_list", "settings_describe", "settings_get", "settings_set", "settings_undo", "settings_history"]
SAFETY = {"settings_set": "change", "settings_undo": "change"}
next_id = 0


def need(cond, what, detail=""):
    if not cond:
        raise SystemExit(f"RED: {what}{': ' + str(detail)[:600] if detail else ''}")
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
            rpc(c, "initialize", {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "cl2", "version": "1"}})
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


def ok(c, name, args=None):
    err, text = call(c, name, args)
    if err:
        raise SystemExit(f"RED: {name} {args}: {text[:600]}")
    return json.loads(text)


def listed(c):
    tools = rpc(c, "tools/list")["result"]["tools"]
    return {t["name"]: t for t in tools if t["name"].startswith("settings_")}


def wait_tools(c, present, wait_s=60):
    end = time.time() + wait_s
    while True:
        t = listed(c)
        if (len(t) == len(TOOLS)) == present:
            return t
        if time.time() > end:
            raise SystemExit(f"RED: settings tools {'missing' if present else 'still listed'}: {sorted(t)}")
        time.sleep(0.5)


def sample(d):
    s, default = d["schema"], d["default"]
    t = s["type"]
    if t == "bool":
        return not default
    if t == "int":
        return s["min"] if default == s["max"] else s["max"]
    if t == "choice":
        return next(x for x in s["choices"] if x != default)
    if t == "color":
        return "#123abc"
    if t == "text":
        return "Test Font"
    if t == "logspec":
        return "warn,net=debug"
    raise SystemExit(f"RED: no sample for {d['key']}")


def read_file(c, path):
    err, text = call(c, "read_file", {"path": path})
    if err:
        return None
    j = json.loads(text)
    return j.get("text") if "text" in j else base64.b64decode(j.get("data", "")).decode("utf-8", "replace")


def guarded(text):
    return [l for l in (text or "").splitlines() if re.match(r"\s*(nvme_write|stick_write|usb_msd_write)\s*=", l, re.I)]


def daemon_pid(c):
    err, text = call(c, "ps", {})
    for line in text.splitlines():
        if f"{SD} serve" in line:
            m = re.search(r"\d+", line)
            if m:
                return int(m.group(0))
    raise SystemExit(f"RED: no settingsd in ps: {text[:1500]}")


def main():
    c = connect(240)
    config_before = read_file(c, "/boot/BITSOS2/CONFIG.TXT")
    need(config_before is not None, "CONFIG.TXT readable")
    tools = wait_tools(c, True)
    need(sorted(tools) == sorted(TOOLS), "the six settings tools are listed", sorted(tools))
    for name, t in tools.items():
        a = t.get("annotations", {})
        want = SAFETY.get(name, "read")
        need(a.get("owner") == "settingsd" and a.get("safety") == want and a.get("readOnlyHint") == (want == "read"),
             f"{name}: owner settingsd, safety {want}", a)

    keys = ok(c, "settings_list")["keys"]
    need(len(keys) >= 25, f"{len(keys)} keys listed")
    cut = ok(c, "settings_list", {"prefix": "theme", "limit": 2})
    need(len(cut["keys"]) == 2 and cut["more"] > 0, "settings_list limit", cut)
    for k in keys:
        key = k["key"]
        d = ok(c, "settings_describe", {"key": key})["key"]
        before = ok(c, "settings_get", {"key": key})["value"]
        v = sample(d)
        a = ok(c, "settings_set", {"key": key, "value": v})
        need(a.get("changed") is True and "applied" in a and "restart" in a, f"{key} set: applied {a.get('applied')}", a)
        got = ok(c, "settings_get", {"key": key})["value"]
        need(got == a["new"], f"{key} reads back {got!r}", a)
        h = ok(c, "settings_history", {"n": 1})["entries"][0]
        need(h.get("key") == key and h.get("source") == "mcp", f"{key} journaled from mcp", h)
        u = ok(c, "settings_undo", {"n": 1})
        need(len(u.get("undone", [])) == 1, f"{key} undone", u)
        need(ok(c, "settings_get", {"key": key})["value"] == before, f"{key} back to {before!r}")

    config_mid = read_file(c, "/boot/BITSOS2/CONFIG.TXT")
    need(guarded(config_mid) == guarded(config_before), "guarded CONFIG.TXT lines unchanged after every key", config_mid)
    hist = call(c, "settings_history", {"n": 200})[1]
    for key in ["nvme_write", "stick_write", "usb_msd_write", "kernel.nvme_write", "token"]:
        err, text = call(c, "settings_set", {"key": key, "value": True, "confirmed": True})
        need(err and "refused_r8" in text, f"settings_set {key} refused (R8)", text)
    need(call(c, "settings_history", {"n": 200})[1] == hist, "R8 refusals journal nothing")
    config_after = read_file(c, "/boot/BITSOS2/CONFIG.TXT")
    need(config_after == config_mid, "CONFIG.TXT byte-identical after the R8 refusals",
         f"before {config_mid!r} after {config_after!r}")

    # The kernel checks the registered schema (type union for `value`, required args) before settingsd sees it.
    for args in [{"key": "theme.radius", "value": [4]}, {"key": "theme.radius", "value": {"n": 4}}, {"value": 4}]:
        j = rpc(c, "tools/call", {"name": "settings_set", "arguments": args})
        need(j.get("error", {}).get("code") == -32602, f"kernel schema check refuses settings_set {args}", j)
    need(call(c, "settings_history", {"n": 200})[1] == hist, "schema refusals journal nothing")

    err, text = call(c, "settings_set", {"key": "theme.accent", "value": "#ff00aa"})
    need(not err, "theme.accent = #ff00aa (left set for the host check)", text)

    pid = daemon_pid(c)
    err, text = call(c, "kill", {"pid": pid, "signal": "SIGKILL"})
    need(not err, f"settingsd (pid {pid}) killed with SIGKILL", text)
    end = time.time() + 60
    while True:
        t = listed(c)
        pids = {x.get("annotations", {}).get("service_pid") for x in t.values()}
        if len(t) == len(TOOLS) and pid not in pids:
            break
        if time.time() > end:
            raise SystemExit(f"RED: the tools do not come back from init's restarted settingsd: {sorted(pids, key=str)}")
        time.sleep(0.5)
    need(True, f"the killed settingsd's tools are gone, init's new one (service_pid {sorted(pids, key=str)}) lists them")
    need(ok(c, "settings_get", {"key": "theme.accent"})["value"] == "#ff00aa", "tools back after a new start, value kept")
    print("settingsd MCP tools on V2 (QEMU): green")


main()
