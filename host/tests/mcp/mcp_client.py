#!/usr/bin/env python3
"""MC-2 scenario client: talks MCP (Streamable HTTP, JSON-RPC 2.0) to a booted BitsOS V2 kernel through QEMU's
hostfwd and checks tools/list, the generic tools and, last, that reboot answers first and then resets.

Usage: mcp_client.py <host port> <token> <check|reboot>
Exit status 0 = green. Every step prints one line."""
import http.client
import json
import re
import sys
import time

PORT, TOKEN, MODE = int(sys.argv[1]), sys.argv[2], sys.argv[3]
GENERIC = {"cpus": "read", "hw_info": "read", "ktest": "change", "msr_read": "read", "net_info": "read",
           "output": "read", "read_phys": "read", "reboot": "change", "stop": "change", "version": "read"}
next_id = 0


def connect(wait_s=0):
    end = time.time() + wait_s
    while True:
        try:
            c = http.client.HTTPConnection("127.0.0.1", PORT, timeout=30)
            rpc(c, "initialize", {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "mc2-scenario", "version": "1"}})
            return c
        except (OSError, http.client.HTTPException, ValueError) as e:
            if time.time() > end:
                raise SystemExit(f"RED: no MCP answer on 127.0.0.1:{PORT}: {e}")
            time.sleep(1)


def post(c, body):
    c.request("POST", "/mcp", json.dumps(body), {"Authorization": f"Bearer {TOKEN}", "Content-Type": "application/json"})
    r = c.getresponse()
    data = r.read()
    return r.status, r.getheader("Connection", ""), (json.loads(data) if data else None)


def rpc(c, method, params=None):
    global next_id
    next_id += 1
    st, _, j = post(c, {"jsonrpc": "2.0", "id": next_id, "method": method, "params": params or {}})
    if st != 200 or "result" not in j:
        raise ValueError(f"{method}: HTTP {st} {j}")
    return j["result"]


def call(c, name, args=None):
    r = rpc(c, "tools/call", {"name": name, "arguments": args or {}})
    text = "\n".join(i.get("text", "") for i in r["content"] if i["type"] == "text")
    return r["isError"], text


def need(cond, what, detail=""):
    if not cond:
        raise SystemExit(f"RED: {what}\n{detail}")
    print(f"ok: {what}")


def check():
    c = connect(wait_s=120)
    tools = {t["name"]: t for t in rpc(c, "tools/list")["tools"]}
    for name, safety in GENERIC.items():
        t = tools.get(name)
        need(t is not None and t["annotations"]["owner"] == "MC" and t["annotations"]["safety"] == safety, f"tools/list has {name} (MC, {safety})", t)
    need(not [n for n in tools if "poweroff" in n or "shutdown" in n], "no power-off tool (R8f)", sorted(tools))
    print("tools: " + " ".join(sorted(tools)))

    err, t = call(c, "hw_info")
    need(not err and all(f"== {s} ==" in t for s in ["kernel", "cpu", "memory", "firmware", "interrupts", "msi", "pci"]), "hw_info has every section", t)
    need("generation " in t and "next start: KERNEL" in t, "hw_info names the generation and the next start", t)
    need("cpu vendor=" in t and "frames total=" in t and "apic mode=" in t, "hw_info cpu, memory and interrupt lines", t)
    # INT-W3: BT-5's profile line (QEMU: the normal profile, CONFIG has no profile=) and CP-4/6's per-CPU lines.
    need("profile=normal reason=-" in t, "hw_info names the boot profile", t)
    need("smp cpus=" in t and re.search(r"^cpu 0 apic=\d+ .* capacity=\d+", t, re.M), "hw_info lists the CPUs with capacity", t)
    print("\n".join(l for l in t.splitlines() if l.startswith(("kernel", "generation", "next start", "cpu vendor", "frames total"))))

    err, t = call(c, "net_info")
    need(not err and "addr=10.0.2.15/24" in t and "router=10.0.2.2" in t and "default-route" in t, "net_info shows the QEMU lease", t)
    need("conn Accepted Tcp" in t, "net_info lists this connection", t)

    err, t = call(c, "cpus")
    need(not err and "mcp-accept class=KernelRt" in t and "mcp-worker class=Normal" in t, "cpus: server KernelRt, workers Normal", t)

    err, t = call(c, "msr_read", {"msr": "0xC0000080"})
    need(not err and t.startswith("msr 0xc0000080 = "), "msr_read EFER", t)
    err, t = call(c, "msr_read", {"msr": 16})
    need(err and "not on the read list" in t, "msr_read refuses the TSC MSR", t)
    err, t = call(c, "read_phys", {"addr": "0xFEE00000", "len": 16})
    need(err and t.startswith("Refused: "), "read_phys refuses the LAPIC", t)
    err, t = call(c, "read_phys", {"addr": "0x100000", "len": 32})
    need(not err and t.startswith("phys 0x100000 len=32"), "read_phys reads RAM at 1 MiB", t)

    err, t = call(c, "ktest", {"filter": "kreg."})
    need(not err and "failed=0" in t.splitlines()[0] and "passed=0" not in t.splitlines()[0], "ktest kreg. over MCP", t)
    err, t = call(c, "output")
    need(not err and t.startswith("no tool call"), "output with nothing running", t)
    err, t = call(c, "reboot", {"dry_run": True})
    need(not err and "next start: KERNEL" in t and "keyboard controller" in t, "reboot dry run names the next start", t)
    c.close()


def reboot():
    c = connect(wait_s=30)
    st, conn, j = post(c, {"jsonrpc": "2.0", "id": 99, "method": "tools/call", "params": {"name": "reboot", "arguments": {}}})
    r = j.get("result", {})
    text = "\n".join(i.get("text", "") for i in r.get("content", []))
    need(st == 200 and not r.get("isError") and text.startswith("rebooting once this answer is out"), "reboot answered first", j)
    need(conn.lower() == "close", "the connection ends after the reboot answer", conn)
    print(text)


{"check": check, "reboot": reboot}[MODE]()
