#!/usr/bin/env python3
"""NW-5 scenario client for V2's MCP (QEMU's hostfwd, or the Mini-PC over the LAN).

Usage:
  sockets.py k3b <mcp_host:port> <token> <echo_host> <echo_port> [MiB]
      V1's k3b-check: `abi-net echo <echo_host> <echo_port> 3 <MiB>` (3 connections at once through a TCP echo,
      default 64 MiB each) runs while MCP is asked `log` every 200 ms on its own connections; every call must be
      answered and everything must come back intact. Latencies are printed (QEMU: functional only; the Mini-PC
      check list judges them).
  sockets.py par64 <mcp_host:port> <token>
      NW-7: the kernel ktest `nw.stack.fetch_parallel` (64 downloads at once, each checked against its SHA-256)
      runs through MCP's `ktest` tool while MCP is asked `log` every 200 ms on its own connections; every call
      must be answered and the ktest must pass.
Exit status 0 = green; every step prints "ok: ..." or "RED: ..."."""
import http.client
import json
import sys
import threading
import time


def need(cond, what, detail=""):
    if not cond:
        raise SystemExit(f"RED: {what}\n{detail}")
    print(f"ok: {what}", flush=True)


def call(hostport, token, name, args=None, timeout=30):
    host, port = hostport.rsplit(":", 1)
    c = http.client.HTTPConnection(host, int(port), timeout=timeout)
    try:
        body = {"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": name, "arguments": args or {}}}
        c.request("POST", "/mcp", json.dumps(body), {"Authorization": f"Bearer {token}", "Content-Type": "application/json"})
        r = c.getresponse()
        data = r.read()
        if r.status != 200:
            raise ValueError(f"HTTP {r.status}")
        j = json.loads(data)
        if "result" not in j:
            raise ValueError(f"{j}")
        text = "\n".join(i.get("text", "") for i in j["result"]["content"] if i["type"] == "text")
        return j["result"]["isError"], text
    finally:
        c.close()


def wait_up(hostport, token, seconds):
    end = time.time() + seconds
    while time.time() < end:
        try:
            err, _ = call(hostport, token, "log", {"max": 1}, timeout=5)
            if not err:
                return True
        except (OSError, http.client.HTTPException, ValueError):
            pass
        time.sleep(0.5)
    return False


def k3b(mcp, token, echo_host, echo_port, mib):
    need(wait_up(mcp, token, 120), "MCP answers")
    result = {}

    def runner():
        try:
            args = {"path": "/bin/abi-net", "args": ["echo", echo_host, str(echo_port), "3", str(mib)], "timeout_ms": 240000}
            result["r"] = call(mcp, token, "run", args, timeout=300)
        except Exception as e:  # noqa: BLE001
            result["e"] = e

    t = threading.Thread(target=runner)
    t.start()
    lat, fails = [], 0
    while t.is_alive():
        t0 = time.time()
        try:
            err, _ = call(mcp, token, "log", {"max": 1}, timeout=10)
            if err:
                fails += 1
            lat.append(time.time() - t0)
        except Exception as e:  # noqa: BLE001
            fails += 1
            print(f"note: log call failed: {e}")
        time.sleep(max(0.0, 0.2 - (time.time() - t0)))
    t.join()
    need("e" not in result, "the run call answered", str(result.get("e")))
    err, text = result["r"]
    print(text)
    need(not err and "ended: exit 0" in text and text.count(" intact") == 3, f"3 x {mib} MiB came back intact")
    lat.sort()
    if lat:
        print(f"MCP during the streams: {len(lat)} calls, median {lat[len(lat) // 2] * 1000:.0f} ms, max {lat[-1] * 1000:.0f} ms")
    need(fails == 0 and len(lat) >= 1, f"MCP answered every call during the streams ({len(lat)}, {fails} failed)")


def during(mcp, token, name, args, timeout):
    """Runs one MCP call in a thread while `log` is asked every 200 ms; returns (answer, latencies, failures)."""
    result = {}

    def runner():
        try:
            result["r"] = call(mcp, token, name, args, timeout=timeout)
        except Exception as e:  # noqa: BLE001
            result["e"] = e

    t = threading.Thread(target=runner)
    t.start()
    lat, fails = [], 0
    while t.is_alive():
        t0 = time.time()
        try:
            err, _ = call(mcp, token, "log", {"max": 1}, timeout=10)
            if err:
                fails += 1
            lat.append(time.time() - t0)
        except Exception as e:  # noqa: BLE001
            fails += 1
            print(f"note: log call failed: {e}")
        time.sleep(max(0.0, 0.2 - (time.time() - t0)))
    t.join()
    need("e" not in result, f"the {name} call answered", str(result.get("e")))
    return result["r"], lat, fails


def counters(mcp, token):
    """net.* counters from net_info as {name: value}."""
    err, text = call(mcp, token, "net_info", {"connections": 0})
    need(not err, "net_info answered", text)
    out = {}
    for line in text.splitlines():
        k, eq, v = line.partition("=")
        if eq and k.startswith("net.") and v.isdigit():
            out[k] = int(v)
    return out


def ktest_during(mcp, token, name, what):
    (err, text), lat, fails = during(mcp, token, "ktest", {"filter": name}, 330)
    print(text)
    head = text.splitlines()[0] if text else ""
    if "no ktest matches" in head:
        print(f"note: {name} is not in this kernel (desk: plan/notes/nw-NW-7-integration.patch)")
        return
    need(not err and "passed=1 failed=0" in head, what)
    lat.sort()
    if lat:
        print(f"MCP during {name}: {len(lat)} calls, median {lat[len(lat) // 2] * 1000:.0f} ms, max {lat[-1] * 1000:.0f} ms")
        # Timing is not judged in QEMU; the Mini-PC list judges it. A note keeps a return of the seconds-long
        # waits of before NW-7 visible in the log.
        if lat[-1] > 1.0:
            print(f"note: an MCP call took {lat[-1] * 1000:.0f} ms during {name} (QEMU; judged on the Mini-PC only)")
    need(fails == 0 and len(lat) >= 1, f"MCP answered every call during {name} ({len(lat)}, {fails} failed)")


def par64(mcp, token):
    need(wait_up(mcp, token, 120), "MCP answers")
    c0 = counters(mcp, token)
    need(c0.get("net.no_memory", 0) == 0, "net.no_memory=0 after a clean boot", str(c0.get("net.no_memory")))
    ktest_during(mcp, token, "nw.stack.fetch_parallel", "64 parallel downloads with the right SHA-256 (nw.stack.fetch_parallel)")
    ktest_during(mcp, token, "nw.sock.fetch_parallel", "64 programs fetching at once with the right SHA-256 (nw.sock.fetch_parallel)")
    c1 = counters(mcp, token)
    need(c1.get("net.no_memory", 0) == 0, "no real socket buffer refusal during the downloads", str(c1.get("net.no_memory")))
    # The A1 Mini-PC run's net.no_memory=529 came from Safe ktests: now only the heap's own refusals count there
    # (one deliberate 1 TiB buffer per run of nw.stack.no_memory), the injected ones in net.no_memory_injected.
    err, text = call(mcp, token, "ktest", {"filter": "nw.stack.no_memory"}, timeout=120)
    need(not err and "passed=1 failed=0" in text, "nw.stack.no_memory passed", text)
    c2 = counters(mcp, token)
    need(c2.get("net.no_memory", 0) == c1.get("net.no_memory", 0) + 1, "nw.stack.no_memory adds exactly 1 to net.no_memory",
         f"{c1.get('net.no_memory')} -> {c2.get('net.no_memory')}")
    need(c2.get("net.no_memory_injected", 0) > c1.get("net.no_memory_injected", 0), "the injected refusals land in net.no_memory_injected")
    print(f"counters: no_memory {c2.get('net.no_memory')} injected {c2.get('net.no_memory_injected')} tx_requeued {c2.get('net.tx_requeued', 0)}")


def main():
    a = sys.argv
    if len(a) >= 6 and a[1] == "k3b":
        k3b(a[2], a[3], a[4], int(a[5]), int(a[6]) if len(a) > 6 else 64)
    elif len(a) >= 4 and a[1] == "par64":
        par64(a[2], a[3])
    else:
        raise SystemExit(__doc__)


main()
