#!/usr/bin/env python3
"""SX-3c: pressure-vessel with the Steam Linux Runtime sniper tree on a running V2, over MCP
(host/tests/sx/20-pressure-vessel.sh).

Streams the tree (user/ports/steamrt/out: SteamLinuxRuntime_sniper/ and gfx/, the software graphics provider) as
an uncompressed tar from 127.0.0.1:<http port>; V2 pulls it with wget from 10.0.2.2 straight into tar (the tree is
about 1.1 GB: no copy of the tarball in V2's RAM) below /tmp/pv. Then, as uid 1000 (MCP's run) with a clean
environment, `run -- true` and `run -- steam-runtime-system-info` (the tree's own entry, default --copy-runtime: the
runtime is hard-linked into var/). Each run's stdout, stderr and exit code land in <out dir>/<name>.{out,err,rc}.

Arguments: <mcp port> <token> <serial log> <http port> <tree dir> <out dir>"""
import http.client
import http.server
import json
import os
import re
import subprocess
import sys
import threading
import time

PORT, TOKEN, LOG, HTTP, TREE, OUT = int(sys.argv[1]), sys.argv[2], sys.argv[3], int(sys.argv[4]), sys.argv[5], sys.argv[6]
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
            c = http.client.HTTPConnection("127.0.0.1", PORT, timeout=1800)
            rpc(c, "initialize", {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "sx3c-pv", "version": "1"}})
            return c
        except (OSError, http.client.HTTPException, ValueError) as e:
            if time.time() > end:
                need(False, f"MCP answers on 127.0.0.1:{PORT} within {wait_s} s", str(e))
            time.sleep(0.5)


def call(c, name, args=None):
    r = rpc(c, "tools/call", {"name": name, "arguments": args or {}})
    return r["isError"], "\n".join(i.get("text", "") for i in r["content"] if i["type"] == "text")


def sh(c, script, timeout_ms=60000):
    """Runs script with /bin/sh (MCP's run: uid 1000); returns (end text, stdout, whole answer)."""
    err, t = call(c, "run", {"path": "/bin/sh", "argv": ["sh", "-c", script], "timeout_ms": timeout_ms})
    m = re.search(r"--- stdout \(\d+ bytes\) ---\n(.*?)--- stderr", t, re.S)
    end = re.match(r"pid \d+ ended: (.*)", t)
    return (end.group(1) if end else t.splitlines()[0] if t else ""), (m.group(1) if m else ""), t


def serve():
    """GET /pv.tar: tar of the tree's two directories, made while it is sent (no 1 GB file on the host)."""
    class H(http.server.BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.0"

        def do_GET(self):
            if self.path.startswith("/file/gfx/usr/lib/") and "/.." not in self.path:
                f = os.path.join(TREE, self.path[len("/file/"):])
                if os.path.isfile(f):
                    data = open(f, "rb").read()
                    self.send_response(200)
                    self.send_header("Content-Length", str(len(data)))
                    self.end_headers()
                    self.wfile.write(data)
                    return
            if self.path != "/pv.tar":
                self.send_error(404)
                return
            self.send_response(200)
            self.end_headers()
            p = subprocess.Popen(["tar", "-C", TREE, "-cf", "-", "SteamLinuxRuntime_sniper", "gfx"], stdout=subprocess.PIPE)
            while True:
                b = p.stdout.read(1 << 20)
                if not b:
                    break
                self.wfile.write(b)
            p.wait()

        def log_message(self, *a):
            pass

    s = http.server.ThreadingHTTPServer(("127.0.0.1", HTTP), H)
    threading.Thread(target=s.serve_forever, daemon=True).start()


# A clean environment as Steam would give it minus the display (QEMU has none): software graphics from the
# provider tree, no host GPU (/dev/dri does not exist in QEMU).
ENV = ("env -i HOME=/tmp/pv/home USER=user LANG=C.UTF-8 PATH=/usr/bin:/bin XDG_RUNTIME_DIR=/tmp/pv/xdg "
       "PRESSURE_VESSEL_GRAPHICS_PROVIDER=/tmp/pv/gfx ")
# What pressure-vessel's own programs take from the host's glibc (ldd of pressure-vessel/bin and libexec; the rest
# comes from its own lib/). V2's image carries only the glibc libraries its programs link (libresolv.so.2 is not
# one of them: change request to UL); until the image has them, the scenario writes the missing ones from the
# provider's glibc into /usr/lib, and only when it is the very build of V2's libc.so.6 (same SHA256).
HOST_GLIBC = ["libc.so.6", "libm.so.6", "libdl.so.2", "libpthread.so.0", "librt.so.1", "libresolv.so.2"]
RUNS = [
    ("true", "true", 900000),
    ("system-info", "steam-runtime-system-info", 1500000),
    # The hostile case: a command that fails must come back as its own exit code, not as pressure-vessel's 0.
    ("exit7", "sh -c 'exit 7'", 900000),
]


def main():
    os.makedirs(OUT, exist_ok=True)
    serve()
    c = connect(240)
    t0 = time.time()
    end, out, t = sh(c, f"set -e; rm -rf /tmp/pv; mkdir -p /tmp/pv/home /tmp/pv/xdg; chmod 700 /tmp/pv/xdg; cd /tmp/pv; "
                        f"wget -q -O - http://10.0.2.2:{HTTP}/pv.tar | tar -xf -; "
                        f"echo files=$(find . | wc -l) kb=$(du -sk . | cut -f1) links=$(find . -type f -links +1 | wc -l)",
                     timeout_ms=1200000)
    need(end == "exit 0", f"tree unpacked below /tmp/pv ({time.time() - t0:.0f} s)", t[:1500])
    print(f"   {out.strip()}", flush=True)
    end, out, t = sh(c, "id; uname -r; cat /proc/sys/user/max_user_namespaces; "
                        "/tmp/pv/SteamLinuxRuntime_sniper/pressure-vessel/libexec/steam-runtime-tools-0/srt-bwrap --version 2>&1")
    print("   " + out.strip().replace("\n", "\n   "), flush=True)
    end, out, t = sh(c, "sha256sum /usr/lib/libc.so.6 /tmp/pv/gfx/usr/lib/libc.so.6 | cut -d' ' -f1 | uniq | wc -l; "
                        f"for l in {' '.join(HOST_GLIBC)}; do [ -e /usr/lib/$l ] || echo missing $l; done")
    missing = re.findall(r"missing (\S+)", out)
    if missing:
        need(out.split()[0] == "1", "V2's libc.so.6 is the provider's glibc build (to fill in " + " ".join(missing) + ")", out)
        for l in missing:
            err, t = call(c, "fetch", {"url": f"http://10.0.2.2:{HTTP}/file/gfx/usr/lib/{l}", "path": f"/usr/lib/{l}", "mode": "0755"})
            need(not err, f"stand-in for the UL change request: {l} written into /usr/lib", t[:300])
    with open(os.path.join(OUT, "host-glibc.txt"), "w") as f:
        f.write(" ".join(missing) + "\n")
    for name, cmd, tmo in RUNS:
        t0 = time.time()
        script = (f"cd /tmp/pv; {ENV}PRESSURE_VESSEL_VERBOSE=1 timeout {tmo // 1000 - 60} "
                  f"/tmp/pv/SteamLinuxRuntime_sniper/run -- {cmd} > /tmp/pv/{name}.out 2> /tmp/pv/{name}.err; echo rc=$?")
        end, out, t = sh(c, script, timeout_ms=tmo)
        rc = (re.search(r"rc=(\d+)", out) or [None, "?"])[1]
        print(f"   {name}: rc={rc} after {time.time() - t0:.0f} s", flush=True)
        for kind in ("out", "err"):
            _, text, _ = sh(c, f"tail -c 900000 /tmp/pv/{name}.{kind}")
            with open(os.path.join(OUT, f"{name}.{kind}"), "w") as f:
                f.write(text)
        with open(os.path.join(OUT, f"{name}.rc"), "w") as f:
            f.write(rc + "\n")
    _, text, _ = sh(c, "cd /tmp/pv/SteamLinuxRuntime_sniper; ls var; echo linked=$(find var -type f -links +1 | wc -l); df -k /tmp")
    with open(os.path.join(OUT, "var.txt"), "w") as f:
        f.write(text)


if __name__ == "__main__":
    main()
