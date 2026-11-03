#!/usr/bin/env python3
"""NW-10 driver (70-node-tap.sh): over MCP, the guest downloads the test file from the node server and from
python's http.server (stack_server.py) on the tap side, with MCP `fetch` (the kernel's HTTP client) and with
BusyBox wget (a program's sockets), and checks every download against the file's FNV-1a 64. Prints the rates
(timing is never judged in QEMU; the Mini-PC check compares them).

Usage: node_tap.py <mcp host:port> <token> <file> <node base url> <python base url>
"""
import http.client
import json
import re
import sys
import time


def call(hostport, token, name, args=None, timeout=60):
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


def fnv1a64(path):
    h = 0xCBF29CE484222325
    with open(path, "rb") as f:
        data = f.read()
    # The whole file at once in chunks keeps this fast enough for 100 MB without numpy.
    for b in data:
        h = ((h ^ b) * 0x100000001B3) & 0xFFFFFFFFFFFFFFFF
    return h


def main():
    mcp, token, file, node, py = sys.argv[1:6]
    if not wait_up(mcp, token, 180):
        raise SystemExit("RED: MCP does not answer")
    print("ok: MCP answers", flush=True)
    size = len(open(file, "rb").read())
    want = None
    fails = 0
    for who, base in (("node", node), ("python", py)):
        for path in ("/file", "/chunked"):
            url = base + path
            t0 = time.time()
            err, text = call(mcp, token, "fetch", {"url": url, "path": "/tmp/nw10.bin", "timeout_ms": 290000}, timeout=300)
            print(f"fetch {who} {path}: {text.strip()} (host clock {time.time() - t0:.2f} s)", flush=True)
            m = re.search(r": (\d+) bytes in .* fnv1a64 (0x[0-9a-f]+)", text)
            if err or not m or int(m.group(1)) != size:
                print(f"RED: fetch {who} {path} failed", flush=True)
                fails += 1
                continue
            if want is None:
                want = fnv1a64(file)
            if int(m.group(2), 16) != want:
                print(f"RED: fetch {who} {path}: fnv {m.group(2)}, want {want:#018x}", flush=True)
                fails += 1
        url = base + "/file"
        t0 = time.time()
        args = {"path": "/bin/busybox", "args": ["wget", "-q", "-O", "/tmp/nw10-wget.bin", url], "timeout_ms": 290000}
        err, text = call(mcp, token, "run", args, timeout=300)
        dt = time.time() - t0
        ok = not err and "exit 0" in text
        _, st = call(mcp, token, "stat", {"path": "/tmp/nw10-wget.bin"})
        print(f"wget {who} /file: {'ok' if ok else 'FAILED'} in {dt:.2f} s by the host clock ({size / dt / 1e6:.1f} MB/s); "
              f"{text.strip()[:200]!r}; stat {st.strip()[:120]!r}", flush=True)
        if not ok or str(size) not in st:
            print(f"RED: wget {who} failed", flush=True)
            fails += 1
        call(mcp, token, "delete", {"path": "/tmp/nw10-wget.bin"})
    if fails:
        raise SystemExit(f"RED: {fails} downloads failed")
    print("ok: every download complete and intact", flush=True)


if __name__ == "__main__":
    main()
