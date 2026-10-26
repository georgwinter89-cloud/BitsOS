#!/usr/bin/env python3
"""PM-2 scenario checks over MCP (10-connections.sh): runs conn_client.rs on BitsOS against conn_lan.py and checks
that the connection view has the right names and byte counts for a program with several connections: while one
connection is held open (tool `connections`, live=true), and after the program ended (the `pm conn.closed` events
in `log`, and `connections`). Without the `connections` tool in the kernel (desk wiring, change request) the
tool checks are skipped with a note, unless REQUIRE_TOOL=1.

Usage: conn_check.py <host:port> <token> <conn_client ELF> <conn_lan log> <hold_s>"""
import base64
import http.client
import json
import os
import re
import sys
import threading
import time

HOSTPORT, TOKEN, ELF, LANLOG, HOLD = sys.argv[1], sys.argv[2], sys.argv[3], sys.argv[4], int(sys.argv[5])
DNS = "192.168.77.1"
# target, address, answer size of the host (conn_lan.py SIZES), the name the program resolved ("-": none).
TARGETS = [("www.google.com", "192.168.77.50", 1000, "www.google.com"),
           ("www.youtube.com", "192.168.77.51", 40000, "www.youtube.com"),
           ("192.168.77.53", "192.168.77.53", 700, "-"),
           ("search.brave.com", "192.168.77.52", 2500, "search.brave.com")]
HELD = "search.brave.com"


def expect(target, size):
    """(bytes in, bytes out) of the program's side: the answer, the request line."""
    return len(f"LEN {size}\n") + size, len(f"GET {target}\n")


def need(cond, what, detail=""):
    if not cond:
        raise SystemExit(f"RED: {what}\n{detail}")
    print(f"ok: {what}", flush=True)


def call(name, args=None, timeout=30):
    host, port = HOSTPORT.rsplit(":", 1)
    c = http.client.HTTPConnection(host, int(port), timeout=timeout)
    try:
        body = {"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": name, "arguments": args or {}}}
        c.request("POST", "/mcp", json.dumps(body), {"Authorization": f"Bearer {TOKEN}", "Content-Type": "application/json"})
        r = c.getresponse()
        data = r.read()
        if r.status != 200:
            raise ValueError(f"HTTP {r.status}")
        j = json.loads(data)
        if "result" not in j:
            return True, json.dumps(j.get("error"))
        text = "\n".join(i.get("text", "") for i in j["result"]["content"] if i["type"] == "text")
        return j["result"]["isError"], text
    finally:
        c.close()


def wait_for(what, f, seconds):
    end = time.time() + seconds
    while time.time() < end:
        try:
            if f():
                return
        except (OSError, http.client.HTTPException, ValueError):
            pass
        time.sleep(0.5)
    raise SystemExit(f"RED: {what} (after {seconds} s)")


def lan():
    try:
        return open(LANLOG).read()
    except OSError:
        return ""


wait_for("MCP answers", lambda: not call("log", {"max": 1}, timeout=5)[0], 120)
wait_for("card 2 has its lease 192.168.77.21", lambda: "192.168.77.21" in call("net_info")[1], 90)
err, _ = call("connections", {"max": 1})
tool = not err
if not tool:
    if os.environ.get("REQUIRE_TOOL") == "1":
        raise SystemExit("RED: the connections tool is not registered")
    print("note: no connections tool in this kernel (desk wiring, CR); its checks are skipped", flush=True)

# The program is larger than one MCP request may be (256 KiB): written in parts, then run by path.
PROG = "/tmp/conn_client"
raw = open(ELF, "rb").read()
for i in range(0, len(raw), 96 * 1024):
    part = {"path": PROG, "data": base64.b64encode(raw[i:i + 96 * 1024]).decode(), "mode": "0755", "parents": True}
    part.update({"append": True} if i else {"offset": 0})
    err, text = call("write_file", part)
    need(not err, f"conn_client written ({min(i + 96 * 1024, len(raw))} of {len(raw)} bytes)", text)
args = [DNS, str(HOLD)] + [t + ("+hold" if t == HELD else "") for t, _, _, _ in TARGETS]
result = {}


def run():
    result["r"] = call("run", {"path": PROG, "args": args, "timeout_ms": (HOLD + 60) * 1000}, timeout=HOLD + 90)


th = threading.Thread(target=run)
th.start()
wait_for("the fake LAN answered every connection but the held one",
         lambda: all(f"tcp {ip}:80" in lan() for t, ip, _, _ in TARGETS if t != HELD), 60)
wait_for("the held connection's DNS answer", lambda: f"dns {HELD} " in lan(), 30)
time.sleep(2)
held_t, held_ip, held_size, _ = next(x for x in TARGETS if x[0] == HELD)
if tool:
    _, live = call("connections", {"live": True, "filter": "conn_client"})
    print(live)
    hin, hout = expect(held_t, held_size)
    need(re.search(rf"^live  tcp {held_ip}:80 name={HELD} in={hin} out={hout} ", live, re.M), "the held connection is live with its name and bytes", live)
    need(not re.search(r"^live  tcp 192\.168\.77\.5[013]:80", live, re.M), "the finished ones are not live", live)
th.join()
err, out = result.get("r", (True, "no answer"))
print(out)
need(not err and "done" in out, "conn_client ran", out)
for t, ip, size, name in TARGETS:
    need(f"conn {t} {ip} " in out, f"conn_client reached {t}", out)

# The journal's record: one pm conn.closed event per connection, with the name and the bytes.
wait_for("the held connection ended", lambda: f"peer={held_ip}:80" in call("log", {"subsystem": "pm", "name": "conn.closed", "level": "info", "max": 500})[1], 30)
_, events = call("log", {"subsystem": "pm", "name": "conn.closed", "level": "info", "max": 500})
mine = [l for l in events.splitlines() if " proto=tcp peer=192.168.77." in l or " proto=udp peer=192.168.77.1:53 " in l]
print("\n".join(mine))
for t, ip, size, name in TARGETS:
    bin_, bout = expect(t, size)
    need(re.search(rf"proto=tcp peer={re.escape(ip)}:80 name={re.escape(name)} .*bytes_in={bin_} bytes_out={bout} ", "\n".join(mine) + " "),
         f"event for {t}: name {name}, in {bin_}, out {bout}", "\n".join(mine))
    lan_line = re.search(rf"tcp {re.escape(ip)}:80 port \d+ in=(\d+) out=(\d+)", lan())
    need(lan_line and (int(lan_line.group(2)), int(lan_line.group(1))) == (bin_, bout), f"the fake LAN counted the same for {t}", lan())
dns = re.findall(r"dns (\S+) from \d+ query=(\d+) answer=(\d+)", lan())
for qname, q, a in dns:
    need(re.search(rf"proto=udp peer=192\.168\.77\.1:53 name=- .*bytes_in={a} bytes_out={q} ", "\n".join(mine) + " "),
         f"DNS query for {qname}: one UDP entry with in {a}, out {q}", "\n".join(mine))

if tool:
    _, view = call("connections", {"filter": "conn_client"})
    print(view)
    need(re.search(r"^== conn_client pid=\d+ program=1: 7 connections", view, re.M), "one program, 7 connections (4 TCP, 3 DNS)", view)
    for t, ip, size, name in TARGETS:
        bin_, bout = expect(t, size)
        need(re.search(rf"^ended tcp {re.escape(ip)}:80 name={re.escape(name)} in={bin_} out={bout} first=\S+ last=\S+", view, re.M),
             f"view: {t} ended with its name and bytes", view)
    _, google = call("connections", {"filter": "google"})
    need("name=www.google.com" in google and "www.youtube.com" not in google and "search.brave.com" not in google, "filter by name", google)
print("pm connections: green (names from the program's own DNS answers, bytes each way, live and ended, events)")
