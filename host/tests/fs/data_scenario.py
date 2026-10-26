#!/usr/bin/env python3
"""FS-8c: /data over MCP on a running V2 (host/tests/fs/50-data.sh, boot 3). The ABI probes of fs-data in
/data against Linux's transcript, the type ext4, then three Reloads while a shell writes into /data (writes in
flight; each new kernel mounts /data again, without a journal replay since the stop pass left it clean), then
the writer is started once more and the script returns so 50-data.sh can cut the power (QEMU kill).

Arguments: <mcp port> <token> <serial log> <http port> <data-linux.txt>"""
import http.client
import json
import re
import sys
import time

PORT, TOKEN, LOG, HTTP, LINUX = int(sys.argv[1]), sys.argv[2], sys.argv[3], sys.argv[4], sys.argv[5]
RELOADS = 3
next_id = 0


def need(cond, what, detail=""):
    if not cond:
        try:
            tail = open(LOG, "rb").read()[-3000:].decode("utf-8", "replace")
        except OSError:
            tail = ""
        raise SystemExit(f"RED: {what}\n{detail}\n--- serial tail ---\n{tail}")
    print(f"ok: {what}", flush=True)


def rpc(c, method, params=None):
    global next_id
    next_id += 1
    c.request("POST", "/mcp", json.dumps({"jsonrpc": "2.0", "id": next_id, "method": method, "params": params or {}}),
              {"Authorization": f"Bearer {TOKEN}", "Content-Type": "application/json"})
    r = c.getresponse()
    data = r.read()
    j = json.loads(data) if data else None
    if r.status != 200 or not j or "result" not in j:
        raise ValueError(f"{method}: HTTP {r.status} {j}")
    return j["result"]


def connect(wait_s):
    end = time.time() + wait_s
    while True:
        try:
            c = http.client.HTTPConnection("127.0.0.1", PORT, timeout=90)
            rpc(c, "initialize", {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "fs8c-data", "version": "1"}})
            return c
        except (OSError, http.client.HTTPException, ValueError) as e:
            if time.time() > end:
                need(False, f"MCP answers on 127.0.0.1:{PORT} within {wait_s} s", str(e))
            time.sleep(0.5)


def call(c, name, args=None):
    r = rpc(c, "tools/call", {"name": name, "arguments": args or {}})
    return r["isError"], "\n".join(i.get("text", "") for i in r["content"] if i["type"] == "text")


def serial():
    with open(LOG, "rb") as f:
        return f.read().decode("utf-8", "replace")


def wait_for(pattern, start, timeout_s, what):
    end = time.time() + timeout_s
    rx = re.compile(pattern)
    while True:
        text = serial()[start:]
        text = text[:text.rfind("\n") + 1]
        m = rx.search(text)
        if m:
            return m, start + m.end()
        if time.time() > end:
            need(False, f"{what} within {timeout_s} s", f"pattern: {pattern}")
        time.sleep(0.2)


def run(c, argv, timeout_ms=60000):
    err, t = call(c, "run", {"path": argv[0], "argv": argv, "timeout_ms": timeout_ms})
    m = re.search(r"--- stdout \(\d+ bytes\) ---\n(.*?)--- stderr", t, re.S)
    end = re.match(r"pid \d+ ended: (.*)", t)
    return err, (end.group(1) if end else t.splitlines()[0] if t else ""), (m.group(1) if m else "")


WRITER = ("i=0; mkdir -p /data/home/1000/inflight; while :; do echo \"line $i\" >> /data/home/1000/inflight/f$((i % 40)); "
          "i=$((i+1)); if [ $((i % 500)) = 0 ]; then sync; fi; done")


def start_writer(c):
    err, t = call(c, "run", {"path": "sh", "argv": ["sh", "-c", WRITER], "timeout_ms": 1500})
    need("Still running" in t and "stderr bytes" not in t, "a shell writes into /data in a loop (writes in flight, no errors)", t[:600])


def main():
    c = connect(240)
    m, pos = wait_for(r"ext4 data\.mounted at=/data pool=(\w+) writable=(\w+) created=(\w+)", 0, 240, "/data mounted at boot")
    need(m.group(1) == "true" and m.group(2) == "true" and m.group(3) == "true", f"/data is the pool volume, writable, made at this first boot ({m.group(0)})")
    with open(LINUX) as f:
        want = [l for l in f.read().splitlines() if not l.startswith("#")]
    err, end, out = run(c, ["/bin/fs-data", "/data/home/1000/abi-fs8c"])
    got = [f"end: {end}"] + out.splitlines()
    if got != want:
        import difflib
        need(False, "fs-data in /data answers as Linux", "\n".join(difflib.unified_diff(want, got, "linux", "v2 /data", lineterm="")))
    need(True, f"fs-data in /data answers as Linux ({len(want)} lines: flock, F_SETLK/F_GETLK, symlink lock, rename over, fsync, fallocate, utimensat, statfs, shared mmap + msync, the F2FS ioctl, 1500 names)")
    err, end, t = run(c, ["cat", "/run/bitsos/data-state"])
    need(t.strip() == "mounted pool rw created", f"/run/bitsos/data-state says the first boot made and mounted it ({t.strip()!r})")
    err, end, out = run(c, ["/bin/fs-data", "/data", "magic"])
    need(out.strip() == "type: 0xef53", f"statfs type of /data is ext4 ({out.strip()})")
    err, end, t = run(c, ["cat", "/proc/self/mounts"])
    line = next((l for l in t.splitlines() if " /data " in l), "")
    need(" ext4 rw" in line and line.startswith("/dev/mapper/data "), f"/proc/mounts: {line}")
    for k in range(1, RELOADS + 1):
        start_writer(c)
        time.sleep(2)
        start = len(serial())
        err, t = call(c, "reload", {"url": f"http://10.0.2.2:{HTTP}/base/"})
        need(not err and "reloading once this answer is out" in t, f"reload {k}: answered", t[:300])
        m, pos = wait_for(r"kexec reload\.synced reload=(\d+) flushed=(\d+) failed=(\d+)", start, 60, f"reload {k}: written back before the stop")
        need(m.group(3) == "0", f"reload {k}: sync_all flushed={m.group(2)} failed=0")
        _, pos = wait_for(r"BitsOS V2 kernel: entry", pos, 60, f"reload {k}: new kernel entered")
        m, pos2 = wait_for(r"ext4 data\.mounted at=/data pool=(\w+) writable=(\w+) created=(\w+)", pos, 120, f"reload {k}: /data mounted again")
        between = serial()[pos:pos2]
        need("ext4 journal.replayed" not in between, f"reload {k}: mounted without a journal replay (the stop pass left it clean)")
        need("ext4 mounted " in between and "rw=true" in between, f"reload {k}: ext4 mounted rw")
        c = connect(90)
        err, t = call(c, "list_dir", {"path": "/data/home/1000/inflight"})
        need(not err and "f1" in t, f"reload {k}: the writer's files are there")
    start_writer(c)
    time.sleep(7)
    print("ok: writer running for 7 s (two background commits): ready for the cut", flush=True)


if __name__ == "__main__":
    main()
