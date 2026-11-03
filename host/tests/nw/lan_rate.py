#!/usr/bin/env python3
"""NW-10, for the desk on the real LAN (not a gate scenario): BitsOS downloads the same file from node.js
(node_server.mjs, a read stream piped into the response, as host/mcp/push.mjs serves) and from python's
http.server (stack_server.py), with MCP `fetch` (the kernel's client) and with BusyBox wget (a program's
sockets), plus node with 1 MiB file reads and node from memory in one write. While each download runs, the sender's TCP state is sampled with `ss -tin` every 50 ms, so the answer
says not only how fast but what held the sender back: the receiver's window (rwnd_limited: BitsOS), the sender's
own buffer (sndbuf_limited: the server program), or loss (retrans, cwnd).

Usage (on the machine that serves, from the repository root):
  python3 -I host/tests/nw/lan_rate.py --mcp <BitsOS ip>:<mcp port> --token-file <file> --bind <this machine's LAN ip>
      [--tls DIR|auto] [--mib 100] [--node-port 24530] [--py-port 24531] [--dir target/xtask/runs/nw-lan-rate] [--target /tmp/nw10.bin]
The ports must be open towards BitsOS (python uses --py-port +1 and +2 as well).
"""
import argparse
import os
import re
import subprocess
import sys
import threading
import time

HERE = os.path.dirname(os.path.abspath(__file__))


# MC-11: under mcp=service (the Mini-PC since 27.10.) plain HTTP carries only the pairing: --tls DIR|auto calls
# over mutual TLS with the key store host/mcp/pair.mjs made (host/tests/mcp/mcp_tls.py, as mcp-call.mjs does).
sys.path.insert(0, os.path.join(HERE, "..", "mcp"))
import mcp_tls  # noqa: E402

TLS = {"tls": None, "token_file": None}


def call(hostport, token, name, args=None, timeout=330):
    return mcp_tls.call(hostport, token, name, args, timeout, TLS["tls"], TLS["token_file"])


class Sampler(threading.Thread):
    """The last `ss -tin` line of the connection from `port` (the sender's view) while it lasts."""

    def __init__(self, port):
        super().__init__(daemon=True)
        self.port, self.last, self.stop = port, "", threading.Event()

    def run(self):
        while not self.stop.is_set():
            out = subprocess.run(["ss", "-tinH", "state", "established", f"( sport = :{self.port} )"],
                                 capture_output=True, text=True).stdout
            line = " ".join(out.split())
            if "cwnd:" in line:
                self.last = line
            time.sleep(0.05)


KEYS = ("rtt", "cwnd", "retrans", "bytes_retrans", "snd_wnd", "delivery_rate", "busy", "rwnd_limited",
        "sndbuf_limited", "unacked", "lost")


def summary(line):
    got = []
    for k in KEYS:
        m = re.search(rf"(?:^| ){k}:(\S+)", line)
        if m:
            got.append(f"{k}:{m.group(1)}")
    if re.search(r"(?:^| )app_limited(?: |$)", line):
        got.append("app_limited")
    return " ".join(got) or "(no sample: the transfer ended within 50 ms or ss is missing)"


def one(a, token, what, port, url, how):
    s = Sampler(port)
    s.start()
    t0 = time.time()
    if how == "fetch":
        err, text = call(a.mcp, token, "fetch", {"url": url, "path": a.target, "timeout_ms": 300000})
    else:
        err, text = call(a.mcp, token, "run", {"path": "/bin/busybox", "args": ["wget", "-q", "-O", a.target, url], "timeout_ms": 300000})
    dt = time.time() - t0
    s.stop.set()
    s.join()
    rate = re.search(r"\((\d+) MB/s\)", text)
    shown = f"{rate.group(1)} MB/s (BitsOS)" if rate else f"{a.mib * 1048576 / dt / 1e6:.1f} MB/s (this machine's clock)"
    print(f"{what:16s} {how:5s} {'FAILED ' if err else ''}{shown}: {text.strip().splitlines()[0][:150]}", flush=True)
    print(f"{'':16s} sender: {summary(s.last)}", flush=True)
    call(a.mcp, token, "delete", {"path": a.target})


def main():
    p = argparse.ArgumentParser()
    p.add_argument("--mcp", required=True)
    p.add_argument("--token-file", required=True)
    p.add_argument("--bind", required=True)
    p.add_argument("--tls", default=None, help="mutual TLS with this key store (or auto), needed under mcp=service")
    p.add_argument("--mib", type=int, default=100)
    p.add_argument("--node-port", type=int, default=24530)
    p.add_argument("--py-port", type=int, default=24531)
    p.add_argument("--dir", default="target/xtask/runs/nw-lan-rate")
    p.add_argument("--target", default="/tmp/nw10.bin")
    a = p.parse_args()
    token = open(a.token_file).read().strip()
    TLS["tls"], TLS["token_file"] = a.tls, a.token_file
    os.makedirs(a.dir, exist_ok=True)
    f = os.path.join(a.dir, "file.bin")
    if not os.path.exists(f) or os.path.getsize(f) != a.mib << 20:
        with open(f, "wb") as out:
            out.write(os.urandom(a.mib << 20))
    procs = [subprocess.Popen(["node", os.path.join(HERE, "node_server.mjs"), str(a.node_port), f, a.bind]),
             subprocess.Popen([sys.executable, "-I", os.path.join(HERE, "stack_server.py"), str(a.py_port), f, "1", a.dir, a.bind])]
    try:
        time.sleep(1.0)
        for how in ("fetch", "wget"):
            one(a, token, "node", a.node_port, f"http://{a.bind}:{a.node_port}/file", how)
            one(a, token, "python", a.py_port, f"http://{a.bind}:{a.py_port}/file", how)
        one(a, token, "node chunked", a.node_port, f"http://{a.bind}:{a.node_port}/chunked", "fetch")
        one(a, token, "node 1 MiB reads", a.node_port, f"http://{a.bind}:{a.node_port}/file1m", "fetch")
        one(a, token, "node one write", a.node_port, f"http://{a.bind}:{a.node_port}/buffer", "fetch")
    finally:
        for x in procs:
            x.terminate()
            x.wait()


if __name__ == "__main__":
    main()
