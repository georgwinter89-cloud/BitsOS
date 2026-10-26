#!/usr/bin/env python3
"""PK-2/PK-4 on V2 through MCP (QEMU hostfwd): `bitspkg selftest` (the round trip and PK-2's removal without
leftovers: private settingsd, a program that tries to write elsewhere) runs as uid 1000 over MCP; then two packages
built on the host from CachyOS's dynamic jq (layouts libpath and view, `bitspkg import`) are sent as one tar,
installed into a scratch root, run from the package through their wrappers, and removed: `bitspkg snapshot` of
the scratch root is the one before the install. PK-5 in between: `space`, and the B1 link (`at`, `at --apply`)
against a copy of the store standing in for a snapshot.

Usage: pk_qemu_check.py <host port> <token> <tar of pkg-libpath/ and pkg-view/>
Exit status 0 = green."""
import base64
import http.client
import json
import sys
import time

PORT, TOKEN, TAR = int(sys.argv[1]), sys.argv[2], sys.argv[3]
BP = "/bin/bitspkg"
W = "/tmp/pk"
R = W + "/root"
next_id = 0
fail = False


def rpc(c, method, params=None):
    global next_id
    next_id += 1
    c.request("POST", "/mcp", json.dumps({"jsonrpc": "2.0", "id": next_id, "method": method, "params": params or {}}),
              {"Authorization": f"Bearer {TOKEN}", "Content-Type": "application/json"})
    r = c.getresponse()
    j = json.loads(r.read() or b"null")
    if r.status != 200 or not j or "result" not in j:
        raise ValueError(f"{method}: HTTP {r.status} {j}")
    return j["result"]


def connect(wait_s):
    end = time.time() + wait_s
    while True:
        try:
            c = http.client.HTTPConnection("127.0.0.1", PORT, timeout=120)
            rpc(c, "initialize", {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "pk", "version": "1"}})
            return c
        except (OSError, http.client.HTTPException, ValueError) as e:
            if time.time() > end:
                raise SystemExit(f"RED: no MCP answer on 127.0.0.1:{PORT}: {e}")
            time.sleep(1)


def call(c, name, args):
    r = rpc(c, "tools/call", {"name": name, "arguments": args})
    return r["isError"], "\n".join(i.get("text", "") for i in r["content"] if i["type"] == "text")


def need(cond, what, detail=""):
    global fail
    print(f"ok   {what}" if cond else f"FAIL {what}\n{detail[:3000]}")
    fail = fail or not cond


def put(c, path, data):
    step = 96 * 1024
    for off in range(0, len(data), step):
        err, t = call(c, "write_file", {"path": path, "data": base64.b64encode(data[off:off + step]).decode(), "offset": off})
        if err:
            raise SystemExit(f"RED: write_file {path} at {off}: {t}")


def run(c, argv, timeout_ms=60000):
    """(exit status line, stdout, stderr) of a program run over MCP (uid 1000)."""
    err, t = call(c, "run", {"path": argv[0], "args": argv[1:], "timeout_ms": timeout_ms, "kill_after_ms": timeout_ms})
    if err or "\n--- stdout (" not in t:
        return None, t, t
    head, rest = t.split("\n--- stdout (", 1)
    out, errpart = rest.split("\n--- stderr (", 1)
    body = lambda part: part.split(") ---", 1)[1].removeprefix("\n")
    return head.splitlines()[0], body(out), body(errpart)


def ok(st):
    return bool(st) and st.endswith("exit 0")


def snap(c):
    st, out, err = run(c, [BP, "snapshot", R])
    if not ok(st):
        raise SystemExit(f"RED: bitspkg snapshot: {st}\n{out}{err}")
    return [l for l in out.splitlines() if not l.startswith("apps/.next-id ")]


def b1(c, n):
    """PK-5 on V2: the space report, and the B1 link against a copy of the volume standing in for a snapshot:
    removed after the copy, the program comes back from it in its version, linked into the store again."""
    p = f"jq-{n}"
    st, out, err = run(c, [BP, "--root", R, "space", p])
    need(ok(st) and f"{p}: package " in out and "  data " in out, f"space {p}", f"{st}\n{out}{err}")
    S = f"{W}/snap-{n}"
    run(c, ["/bin/busybox", "mkdir", "-p", S])
    for a, b in [("apps", "apps"), ("data/appdata", "appdata")]:
        st, out, err = run(c, ["/bin/busybox", "cp", "-a", f"{R}/{a}", f"{S}/{b}"])
        need(ok(st), f"copy {a} as the snapshot", f"{st}\n{out}{err}")
    st, out, err = run(c, [BP, "--root", R, "at", S, p])
    v = next((l.split(" ", 1)[1] for l in out.splitlines() if l.startswith("then.current ")), "-")
    need(ok(st) and v != "-" and "step nothing" in out, f"at: {p} {v} current in the snapshot and now", f"{st}\n{out}{err}")
    st, out, err = run(c, [BP, "--root", R, "remove", "--keep-data", p])
    need(ok(st), f"remove {p} after the snapshot", f"{st}\n{out}{err}")
    st, out, err = run(c, [BP, "--root", R, "at", S, p, "--apply"])
    need(ok(st) and f"step install {S}/apps/{p}/{v}" in out and f"done {p}: installed {v} from the snapshot" in out,
         f"at --apply: {p} {v} back from the snapshot", f"{st}\n{out}{err}")
    st, out, err = run(c, [BP, "--root", R, "verify", "--store"])
    need(ok(st) and "verify: all good" in out, f"verify --store after bringing {p} back", f"{st}\n{out}{err}")
    st, out, err = run(c, ["/bin/sh", f"{R}/apps/{p}/current/bitsos/run", "-n", "-c", "[1,2,3]|add"])
    need(ok(st) and out.strip() == "6", f"{p} runs again after the B1 restore", f"{st}\n{out}{err}")
    # Version folders are 0555: opened first, or rm (no CAP_DAC_OVERRIDE for uid 1000) leaves them.
    run(c, ["/bin/busybox", "chmod", "-R", "u+w", S])
    run(c, ["/bin/busybox", "rm", "-rf", S])


def main():
    c = connect(240)
    st, out, err = run(c, [BP, "selftest", "/tmp/pk2"], 120000)
    print(out.rstrip())
    need(ok(st) and "selftest ok" in out and "ok   after the removal the tree is the tree before the install" in out,
         "bitspkg selftest on V2 (uid 1000): round trip and removal without leftovers", f"{st}\n{out}{err}")
    need("ok   its writes elsewhere are refused" in out, "the test program's writes outside its data folder are refused", out)
    run(c, ["/bin/busybox", "mkdir", "-p", W])
    put(c, W + "/pkgs.tar", open(TAR, "rb").read())
    st, out, err = run(c, ["/bin/busybox", "tar", "-xf", W + "/pkgs.tar", "-C", W])
    need(ok(st), "the packages are unpacked on V2", f"{st}\n{out}{err}")
    st, out, err = run(c, [BP, "--root", R, "gc"])
    need(ok(st), "an empty store", f"{st}\n{out}{err}")
    before = snap(c)
    for n in ["libpath", "view"]:
        st, out, err = run(c, [BP, "--root", R, "install", f"{W}/pkg-{n}"])
        need(ok(st), f"install jq-{n}", f"{st}\n{out}{err}")
        st, out, err = run(c, [BP, "--root", R, "verify", "--store"])
        need(ok(st), f"verify --store after jq-{n}", f"{st}\n{out}{err}")
        # MCP run starts ELF files only; the wrapper is a script (as `program` it is started by bitsrun, PK-3).
        st, out, err = run(c, ["/bin/sh", f"{R}/apps/jq-{n}/current/bitsos/run", "-n", "-c", "[1,2,3]|add"])
        need(ok(st) and out.strip() == "6", f"CachyOS's dynamic jq runs from its package ({n} layout)", f"{st}\n{out}{err}")
        b1(c, n)
        st, out, err = run(c, [BP, "--root", R, "remove", f"jq-{n}"])
        need(ok(st), f"remove jq-{n}", f"{st}\n{out}{err}")
    after = snap(c)
    gone = [l for l in before if l not in after]
    new = [l for l in after if l not in before]
    need(not gone and not new, "after both removals the scratch root is as before", f"gone {gone}\nleft {new}")
    run(c, ["/bin/busybox", "rm", "-rf", W, "/tmp/pk2"])
    if fail:
        raise SystemExit("RED: see FAIL lines above")
    print("PK-2/PK-4/PK-5 on V2 (QEMU): green")


main()
