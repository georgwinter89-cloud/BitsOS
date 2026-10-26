#!/usr/bin/env python3
"""AB-2 scenario client: runs programs on a booted BitsOS V2 through the `run` MCP tool (QEMU hostfwd) and the
same programs on this CachyOS host, and compares what they print and how they end.

Usage: run_client.py <host port> <token> <dir with the host builds>
Exit status 0 = green. Every comparison prints one line."""
import http.client
import json
import os
import subprocess
import sys
import time

PORT, TOKEN, PROGS = int(sys.argv[1]), sys.argv[2], sys.argv[3]
ROOT = os.environ.get("BITSOS_ROOT", ".")
next_id = 0
failed = []


def connect(wait_s=0):
    end = time.time() + wait_s
    while True:
        try:
            c = http.client.HTTPConnection("127.0.0.1", PORT, timeout=60)
            rpc(c, "initialize", {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "ab2-scenario", "version": "1"}})
            return c
        except (OSError, http.client.HTTPException, ValueError) as e:
            if time.time() > end:
                raise SystemExit(f"RED: no MCP answer on 127.0.0.1:{PORT}: {e}")
            time.sleep(1)


def rpc(c, method, params=None):
    global next_id
    next_id += 1
    c.request("POST", "/mcp", json.dumps({"jsonrpc": "2.0", "id": next_id, "method": method, "params": params or {}}),
              {"Authorization": f"Bearer {TOKEN}", "Content-Type": "application/json"})
    r = c.getresponse()
    data = r.read()
    j = json.loads(data) if data else None
    if r.status != 200 or "result" not in j:
        raise ValueError(f"{method}: HTTP {r.status} {j}")
    return j["result"]


def run_v2(c, args):
    r = rpc(c, "tools/call", {"name": "run", "arguments": args})
    text = "\n".join(i.get("text", "") for i in r["content"] if i["type"] == "text")
    if r["isError"]:
        return None, text, "", text
    head, rest = text.split("\n--- stdout (", 1)
    out_part, err_part = rest.split("\n--- stderr (", 1)
    body = lambda part: part.split(") ---", 1)[1].removeprefix("\n")
    return head.splitlines()[0], body(out_part), body(err_part), text


def run_host(path, argv, env, stdin=b""):
    p = subprocess.run(argv, executable=path, env=env, input=stdin, capture_output=True, timeout=30)
    rc = p.returncode
    status = f"exit {rc}" if rc >= 0 else f"signal {-rc}"
    return status, p.stdout.decode(errors="replace"), p.stderr.decode(errors="replace")


def check(what, ok, detail=""):
    print(("ok: " if ok else "RED: ") + what)
    if not ok:
        failed.append(what)
        print(detail)


def compare(c, label, v2_path, host_path, argv, env, stdin="", drop=()):
    head, out, err, text = run_v2(c, {"path": v2_path, "argv": argv, "env": [f"{k}={v}" for k, v in env.items()], "stdin": stdin})
    hs, hout, herr = run_host(host_path, argv, env, stdin.encode())
    if head is None:
        check(f"{label}: run answered", False, text)
        return text
    vs = head.split(" ended: ", 1)[1].split(",")[0].split(" (")[0]
    keep = lambda s: "\n".join(l for l in s.splitlines() if not l.startswith(drop)) if drop else s
    check(f"{label}: same end ({hs})", vs == hs, f"V2: {vs}\nhost: {hs}\n{text}")
    check(f"{label}: same stdout", keep(out).rstrip("\n") == keep(hout).rstrip("\n"), f"--- V2 ---\n{out}\n--- host ---\n{hout}")
    check(f"{label}: same stderr", err.rstrip("\n") == herr.rstrip("\n"), f"--- V2 ---\n{err}\n--- host ---\n{herr}")
    return text


def main():
    c = connect(wait_s=180)
    tools = {t["name"]: t for t in rpc(c, "tools/list")["tools"]}
    t = tools.get("run")
    check("tools/list has run (owner AB, safety change)", t is not None and t["annotations"]["owner"] == "AB" and t["annotations"]["safety"] == "change", t)

    # The Mini-PC check: `run hello` by bare name.
    head, out, err, text = run_v2(c, {"path": "hello"})
    check("run hello: exit 0 and its line", head is not None and head.endswith("ended: exit 0") and out.rstrip("\n") == "hello from musl C on BitsOS V2", text)
    print(text)

    env = {"A": "1", "B": "two words", "EMPTY": ""}
    argv = ["auxv", "one", "two words", ""]
    text = compare(c, "auxv (static-PIE C)", "/bin/ab-auxv", f"{PROGS}/ab-auxv", argv, env, drop=("linux-only ",))
    print("V2 auxv report:\n" + text)
    compare(c, "C++ libc++ (static-PIE)", "/bin/ab-cxx", f"{PROGS}/ab-cxx", ["ab-cxx", "BitsOS"], {})
    compare(c, "stdin and stderr", "/bin/ab-stdin", f"{PROGS}/ab-stdin", ["ab-stdin", "X"], {}, stdin="hello\nworld\n")
    text = compare(c, "fault (SIGSEGV)", "/bin/ab-fault", f"{PROGS}/ab-fault", ["ab-fault"], {})
    check("fault: the V2 message names program, pid, thread, address and reason",
          "Program ab-fault (pid " in text and "ended: ERROR: Page fault at 0x0: not mapped" in text, text)
    rs = os.path.join(ROOT, "target/user/x86_64-unknown-linux-musl/release/hello-rs")
    if os.path.exists(rs):
        compare(c, "Rust std hello (static-PIE)", "/bin/hello-rs", rs, ["hello-rs"], {"HOME": "/"})
    cb = os.path.join(ROOT, "target/xtask/cbin/hello")
    if os.path.exists(cb):
        compare(c, "musl C hello (static ET_EXEC)", "/bin/hello", cb, ["hello"], {})
    if failed:
        raise SystemExit(f"RED: {len(failed)} comparisons failed: {failed}")
    print("ab run: green")


main()
