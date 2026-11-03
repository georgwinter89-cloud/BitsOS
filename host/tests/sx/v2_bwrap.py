#!/usr/bin/env python3
"""SX-3b: bubblewrap's own test suite on a running V2, over MCP (host/tests/sx/30-bwrap-suite.sh).

Serves the stage tarball (user/ports/bubblewrap/out as bw.tar) on 127.0.0.1:<http port>, V2 fetches it from
10.0.2.2, unpacks it into /tmp/bw. Then each suite runs
as uid 1000 (MCP's run), unprivileged like on CachyOS; the TAP output of each lands in <out dir>/<suite>.tap
(and .err for the shell trace). The comparison with the CachyOS transcripts is 30-bwrap-suite.sh's.

Arguments: <mcp port> <token> <serial log> <http port> <bw.tar> <out dir> [suite ...]"""
import http.client
import http.server
import json
import os
import re
import sys
import threading
import time

PORT, TOKEN, LOG, HTTP, TAR, OUT = int(sys.argv[1]), sys.argv[2], sys.argv[3], int(sys.argv[4]), sys.argv[5], sys.argv[6]
SUITES = sys.argv[7:] or ["test-run", "test-specifying-pidns", "test-specifying-userns", "test-utils"]
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
            c = http.client.HTTPConnection("127.0.0.1", PORT, timeout=1200)
            rpc(c, "initialize", {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "sx3b-bwrap", "version": "1"}})
            return c
        except (OSError, http.client.HTTPException, ValueError) as e:
            if time.time() > end:
                need(False, f"MCP answers on 127.0.0.1:{PORT} within {wait_s} s", str(e))
            time.sleep(0.5)


def call(c, name, args=None):
    r = rpc(c, "tools/call", {"name": name, "arguments": args or {}})
    return r["isError"], "\n".join(i.get("text", "") for i in r["content"] if i["type"] == "text")


def sh(c, script, timeout_ms=60000):
    """Runs script with /bin/sh as root; returns (exit text, stdout)."""
    err, t = call(c, "run", {"path": "/bin/sh", "argv": ["sh", "-c", script], "timeout_ms": timeout_ms})
    m = re.search(r"--- stdout \(\d+ bytes\) ---\n(.*?)--- stderr", t, re.S)
    end = re.match(r"pid \d+ ended: (.*)", t)
    return (end.group(1) if end else t.splitlines()[0] if t else ""), (m.group(1) if m else ""), t


def serve():
    data = open(TAR, "rb").read()

    class H(http.server.BaseHTTPRequestHandler):
        def do_GET(self):
            if self.path != "/bw.tar":
                self.send_error(404)
                return
            self.send_response(200)
            self.send_header("Content-Length", str(len(data)))
            self.end_headers()
            self.wfile.write(data)

        def log_message(self, *a):
            pass

    s = http.server.ThreadingHTTPServer(("127.0.0.1", HTTP), H)
    threading.Thread(target=s.serve_forever, daemon=True).start()
    return len(data)


# MCP's run is uid 1000 already (as the suite wants it). V2's /usr/bin is a link to ../bin (Arch has it the other
# way round), so the suite's "pivot to a new rootfs" check (a root made of /usr and /etc, /bin -> usr/bin) would
# find no findmnt: usrbin/ is a real directory with busybox, its applets, bash and the helpers, and each suite runs
# inside one outer bwrap that puts it on /usr/bin (record-host.sh runs CachyOS inside the same outer bwrap).
SETUP = r"""set -e
rm -rf /tmp/bw; mkdir -p /tmp/bw; cd /tmp/bw; tar -xf /tmp/bw.tar
mkdir usrbin; cp /bin/busybox usrbin/busybox
for a in $(busybox --list); do [ -e usrbin/$a ] || ln -s busybox usrbin/$a; done
for p in bwrap bash getpcaps findmnt df bwrap-lockf bwrap-pdeath-probe; do rm -f usrbin/$p; mv $p usrbin/$p; done
# sh is bash as on Arch (busybox picks its applet by argv[0], so the suite's --argv0 check would find none).
rm usrbin/sh; ln -s bash usrbin/sh
echo merged=$([ /lib -ef /usr/lib ] && echo yes || echo no) uid=$(id -u) applets=$(ls usrbin | wc -l)
"""

OUTER = ("/tmp/bw/usrbin/bwrap --unshare-user --uid 1000 --gid 1000 --dev-bind / / --tmpfs /usr --ro-bind /usr/lib /usr/lib "
         "--symlink lib /usr/lib64 --ro-bind /usr/share /usr/share --ro-bind /tmp/bw/usrbin /usr/bin --symlink bin /usr/sbin -- ")
AS_USER = "env -i HOME=/tmp USER=user PATH=/usr/bin:/bin BWRAP=/usr/bin/bwrap G_TEST_SRCDIR=/tmp/bw " + OUTER


def main():
    os.makedirs(OUT, exist_ok=True)
    n = serve()
    c = connect(240)
    err, t = call(c, "fetch", {"url": f"http://10.0.2.2:{HTTP}/bw.tar", "path": "/tmp/bw.tar", "timeout_ms": 120000})
    need(not err and f"{n} bytes" in t, f"V2 fetched the stage ({n} bytes)", t[:400])
    end, out, t = sh(c, SETUP)
    need(end == "exit 0", "stage unpacked", t[:1500])
    print(f"   {out.strip()}", flush=True)
    for s in SUITES:
        cmd = f"/tmp/bw/tests/{s}" if s == "test-utils" else f"bash /tmp/bw/tests/{s}.sh"
        script = f"cd /tmp && timeout 600 {AS_USER}{cmd} > /tmp/bw/{s}.tap 2> /tmp/bw/{s}.err; echo rc=$?"
        t0 = time.time()
        end, out, t = sh(c, script, timeout_ms=900000)
        rc = (re.search(r"rc=(\d+)", out) or [None, "?"])[1]
        print(f"   {s}: rc={rc} after {time.time() - t0:.0f} s", flush=True)
        for kind in ("tap", "err"):
            _, text, _ = sh(c, f"tail -c 900000 /tmp/bw/{s}.{kind}")
            with open(os.path.join(OUT, f"{s}.{kind}"), "w") as f:
                f.write(text)
        with open(os.path.join(OUT, f"{s}.rc"), "w") as f:
            f.write(rc + "\n")


if __name__ == "__main__":
    main()
