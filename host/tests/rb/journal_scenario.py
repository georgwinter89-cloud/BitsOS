#!/usr/bin/env python3
"""RB-8 scenario driver (70-journal.sh): the journal (bitsjournal, started by init) through MCP, one phase per
QEMU run on the same stick (its data.img is /data):

  1  a program's connection (fetch from the host's HTTP server: `pm conn.closed peer=10.0.2.2:<port> pid>0`)
     is found through `events` with since/until (answered from the journal, not the ring); a second one right
     before a Reload is found after it (the Reload waited for the journal); by time window and by field; metrics
     from the journal; then `reboot` (QEMU ends: -no-reboot).
  2  (the next boot) both connections are still there, in the first boot's part; the boots are told apart.
  3  (a boot 70 days later: -rtc base=...) retention (journal.keep_days=60) deleted the old part.

Usage: journal_scenario.py <phase> <mcp host port> <token> <guest http port>"""
import datetime
import http.client
import json
import os
import re
import sys
import time

PHASE, PORT, TOKEN, HTTP = int(sys.argv[1]), int(sys.argv[2]), sys.argv[3], int(sys.argv[4])
next_id = 0
# MCP's reboot drains the journal once the desk applied change request 2 of plan/notes/rb-RB-8.md; until then the
# boot's last lines are written by the 5 s flush, so the run waits for it before rebooting.
REBOOT_DRAINS = os.environ.get("RB8_REBOOT_DRAIN") == "1"
FETCHES = {"subsystem": "pm", "name": "conn.closed", "fields": f"peer=10.0.2.2:{HTTP} pid>0", "since": "1"}


def need(cond, what, detail=""):
    if not cond:
        raise SystemExit(f"RED: {what}{': ' + str(detail)[:1500] if detail else ''}")
    print(f"ok: {what}")


def rpc(c, method, params=None):
    global next_id
    next_id += 1
    c.request("POST", "/mcp", json.dumps({"jsonrpc": "2.0", "id": next_id, "method": method, "params": params or {}}),
              {"Authorization": f"Bearer {TOKEN}", "Content-Type": "application/json"})
    r = c.getresponse()
    data = r.read()
    j = json.loads(data) if data else None
    if r.status != 200 or not j:
        raise ValueError(f"{method}: HTTP {r.status} {data[:200]}")
    return j


def connect(wait_s):
    end = time.time() + wait_s
    while True:
        try:
            c = http.client.HTTPConnection("127.0.0.1", PORT, timeout=90)
            rpc(c, "initialize", {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "rb8", "version": "1"}})
            return c
        except (OSError, http.client.HTTPException, ValueError) as e:
            if time.time() > end:
                raise SystemExit(f"RED: no MCP answer on 127.0.0.1:{PORT} within {wait_s} s: {e}")
            time.sleep(1)


def call(c, name, args=None):
    j = rpc(c, "tools/call", {"name": name, "arguments": args or {}})
    if "error" in j:
        return True, f"rpc error {j['error']}"
    r = j["result"]
    return r.get("isError", False), "\n".join(i.get("text", "") for i in r["content"] if i["type"] == "text")


def wait_tool(c, name, wait_s):
    end = time.time() + wait_s
    while time.time() < end:
        tools = [t["name"] for t in rpc(c, "tools/list")["result"]["tools"]]
        if name in tools:
            return
        time.sleep(1)
    raise SystemExit(f"RED: tool {name} not registered within {wait_s} s (does init start bitsjournal? CR 1 of plan/notes/rb-RB-8.md)")


def shown(t):
    m = re.search(r" shown=(\d+) ", t)
    return int(m.group(1)) if m else -1


def events(c, args):
    err, t = call(c, "events", args)
    need(not err and t.startswith("journal events "), f"events {args} answered from the journal", t)
    return t


def events_until(c, args, cond, wait_s, what):
    end = time.time() + wait_s
    while True:
        t = events(c, args)
        if cond(t):
            print(f"ok: {what}")
            return t
        if time.time() > end:
            raise SystemExit(f"RED: {what}: {t[:1500]}")
        time.sleep(1)


def fetch(c, path):
    err, t = call(c, "run", {"path": "/bin/fetch", "args": ["-o", "-", f"http://10.0.2.2:{HTTP}/{path}"], "timeout_ms": 30000})
    need(not err and f"journal marker {path}" in t, f"fetch {path} from the host", t)


def marker_times(t):
    return [l[:23] for l in t.splitlines() if " pm conn.closed " in l]


def boots(t):
    return re.findall(r"^-- boot ([0-9a-f]{16})$", t, re.M)


def reboot(c):
    if not REBOOT_DRAINS:
        time.sleep(7)
    err, t = call(c, "reboot")
    need(not err, "reboot answered", t)


def phase1():
    c = connect(240)
    wait_tool(c, "journal", 180)
    fetch(c, "marker-a")
    a = events_until(c, FETCHES, lambda t: shown(t) == 1, 30, "the first connection is in the journal (written within 5 s)")
    t_a = marker_times(a)[0]
    # By time: a window around it finds it, one a minute before does not.
    at = datetime.datetime.strptime(t_a[:19], "%Y-%m-%d %H:%M:%S")
    fmt = lambda d: d.strftime("%Y-%m-%d %H:%M:%S")
    win = events(c, {**FETCHES, "since": fmt(at - datetime.timedelta(seconds=2)), "until": fmt(at + datetime.timedelta(seconds=2))})
    need(shown(win) == 1 and t_a in win, f"found by its local time {t_a}", win)
    before = events(c, {**FETCHES, "since": fmt(at - datetime.timedelta(seconds=120)), "until": fmt(at - datetime.timedelta(seconds=60))})
    need(shown(before) == 0, "nothing in the minute before it", before)
    other = events(c, {**FETCHES, "fields": f"peer=10.0.2.2:{HTTP} pid<0"})
    need(shown(other) == 0, "a field that does not match finds nothing", other)
    # The second one right before a Reload: only the drain brings it into the journal in time.
    fetch(c, "marker-b")
    err, t = call(c, "reload", {"url": f"http://10.0.2.2:{HTTP}/base/", "timeout_ms": 60000})
    need(not err and "reloading once this answer is out" in t, "reload answered", t)
    time.sleep(3)
    c = connect(180)
    wait_tool(c, "journal", 120)
    both = events(c, FETCHES)
    need(shown(both) == 2 and len(set(boots(both))) == 1, "after the Reload both connections are there, from the boot before it", both)
    err, m = call(c, "metrics", {"since": "1", "name": "cpu.all"})
    need(not err and m.startswith("journal metrics ") and re.search(r" rows=[1-9]", m) and "\ncpu.all.busy % " in m, "metrics from the journal (the drain wrote the partial mean)", m)
    reboot(c)


def phase2():
    c = connect(240)
    wait_tool(c, "journal", 180)
    both = events(c, FETCHES)
    need(shown(both) == 2, "after the reboot both connections are still in the journal", both)
    att = events_until(c, {"since": "1", "subsystem": "klog", "name": "journal.attached"}, lambda t: len(set(boots(t))) >= 3, 30,
                       "three boots in the journal (first boot, its Reload, this boot), each with its own id")
    print("\n".join(l for l in att.splitlines() if l.startswith("-- boot")))
    err, st = call(c, "journal", {"kind": "status"})
    need(not err and st.startswith("journal status ") and re.search(r" segments=[1-9]", st) and " keep_days=60 max_mb=256 " in st, "journal status (files, size, settings)", st)
    print(st)
    reboot(c)


def phase3():
    c = connect(240)
    wait_tool(c, "journal", 180)
    events_until(c, FETCHES, lambda t: shown(t) == 0, 60, "70 days later: the old part is gone (journal.keep_days=60)")
    att = events(c, {"since": "1", "subsystem": "klog", "name": "journal.attached"})
    need(len(set(boots(att))) == 1, "only this boot is left", att)


{1: phase1, 2: phase2, 3: phase3}[PHASE]()
print(f"phase {PHASE}: green")
