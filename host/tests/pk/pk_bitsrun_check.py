#!/usr/bin/env python3
"""PK-3 on V2 through MCP (QEMU hostfwd): a test package (host/tests/pk/bitsrun-probe.c, dynamic against the
build host's glibc, as a per-program root tree root/usr + root/etc) installs into a scratch store as uid 1000 and
starts through /bin/bitsrun: it sees only its view, writes outside its data folder (and the wanted Downloads)
fail, its own ld.so is the interpreter (inode of the tree's file), a re-exec through /proc/self/exe works, the
environment is bitsrun's, no mount is left in the caller's view; `--root TREE -- CMD`, a refusal, removal.

Usage: pk_bitsrun_check.py <host port> <token> <tar of pkg/>
Exit status 0 = green."""
import base64
import http.client
import json
import sys
import time

PORT, TOKEN, TAR = int(sys.argv[1]), sys.argv[2], sys.argv[3]
BP, BR, BB = "/bin/bitspkg", "/bin/bitsrun", "/bin/busybox"
W = "/tmp/pkr"
R = W + "/root"
H = W + "/home"
ENV = ["PATH=/bin:/sbin:/usr/bin", f"HOME={H}", "XDG_RUNTIME_DIR=/run/user/1000", "LANG=de_DE.UTF-8", "LD_LIBRARY_PATH=/bad", "LD_PRELOAD=/bad.so"]
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
            rpc(c, "initialize", {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "pk3", "version": "1"}})
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


def run(c, argv, env=None, timeout_ms=60000):
    """(exit status line, stdout, stderr) of a program run over MCP (uid 1000)."""
    a = {"path": argv[0], "args": argv[1:], "timeout_ms": timeout_ms, "kill_after_ms": timeout_ms}
    if env:
        a["env"] = env
    err, t = call(c, "run", a)
    if err or "\n--- stdout (" not in t:
        return None, t, t
    head, rest = t.split("\n--- stdout (", 1)
    out, errpart = rest.split("\n--- stderr (", 1)
    body = lambda part: part.split(") ---", 1)[1].removeprefix("\n")
    return head.splitlines()[0], body(out), body(errpart)


def ok(st):
    return bool(st) and st.endswith("exit 0")


def mounts(c):
    st, out, err = run(c, [BB, "cat", "/proc/self/mountinfo"])
    return sorted(" ".join(l.split()[3:5]) for l in out.splitlines()) if ok(st) else None


def listing(c, d):
    st, out, err = run(c, [BB, "find", d])
    return sorted(out.splitlines()) if ok(st) else None


def main():
    c = connect(240)
    st, out, err = run(c, [BB, "test", "-x", "/bin/bwrap"])
    if not ok(st):
        raise SystemExit(f"RED: no /bin/bwrap in the image (PK-3 CR 8, plan/spec/sandbox.md §4): {st} {out}{err}")
    print("ok   /bin/bwrap is in the image")
    run(c, [BB, "mkdir", "-p", W, f"{H}/Downloads", f"{H}/Documents", f"{H}/Pictures"])
    put(c, W + "/pkg.tar", open(TAR, "rb").read())
    st, out, err = run(c, [BB, "tar", "-xf", W + "/pkg.tar", "-C", W])
    need(ok(st), "the package is unpacked on V2", f"{st}\n{out}{err}")
    st, out, err = run(c, [BP, "--root", R, "install", f"{W}/pkg"])
    need(ok(st), "install probe (uid 1000)", f"{st}\n{out}{err}")
    before = mounts(c)
    # B2: a run that fails leaves nothing behind (store, caller's /tmp, mounts).
    s0 = listing(c, R)
    st, out, err = run(c, [BR, "--store", R, "probe", "fail"], ENV)
    need(bool(st) and st.endswith("exit 3") and "write /tmp/probe-fail = ok" in out, "a failing run ends with the program's status", f"{st}\n{out}{err}")
    st2, o2, e2 = run(c, [BB, "ls", "/tmp/probe-fail"])
    need(s0 is not None and listing(c, R) == s0 and mounts(c) == before and not ok(st2),
         "a failing run leaves nothing (store, /tmp, mounts)", f"{o2}{e2}")
    st, out, err = run(c, [BR, "--store", R, "--plan", "probe"], ENV)
    print(out.rstrip())
    need(ok(st) and "remount-ro /" in out and "ro-bind " + R + "/apps/probe/1/root/usr -> /usr" in out, "bitsrun --plan", f"{st}\n{out}{err}")
    st, out, err = run(c, [BR, "--store", R, "probe", "all"], ENV)
    print(out.rstrip())
    need(ok(st), "the probe runs through bitsrun", f"{st}\n{out}{err}")
    v = {}
    for l in out.splitlines():
        if " = " in l:
            k, x = l.split(" = ", 1)
            v[k] = x
    ls = lambda d: set(v.get(f"ls {d}", "-").split())
    need(v.get("uid") == "1000", "it runs as uid 1000", out)
    root_want = {"bin", "data", "dev", "etc", "lib", "lib64", "proc", "run", "sbin", "tmp", "usr"}
    need(root_want <= ls("/") <= root_want | {"sys"}, "/ shows only the view (no apps, boot, var)", v.get("ls /", ""))
    need(ls("/usr") == {"bin", "lib"} and ls("/usr/bin") == {"probe"}, "/usr is the tree's usr", out)
    need({"passwd", "probe.conf"} <= ls("/etc") <= {"passwd", "probe.conf", "group", "localtime", "resolv.conf", "ssl"},
         "/etc is the tree's etc with the system's files", v.get("ls /etc", ""))
    need(v.get("cat /etc/probe.conf") == "probe-conf-from-tree|", "/etc/probe.conf comes from the tree", out)
    pw = v.get("cat /etc/passwd", "")
    need("treeuser" not in pw and ":1000:" in pw, "/etc/passwd is the system's, bound over the tree's", pw)
    need(ls("/data/appdata") == {"probe"}, "/data/appdata shows only its own data folder", out)
    need(ls(H) == {".local", "Downloads", "Documents"}, "home: data folder, the wanted Downloads and Documents, not Pictures", v.get(f"ls {H}", ""))
    for p in ["/usr/probe-w", "/etc/probe-w", "/probe-w", "/data/probe-w", "/data/appdata/probe-w", f"{H}/Documents/probe-w"]:
        need(v.get(f"write {p}") in ("EROFS", "EACCES"), f"write {p} is refused", v.get(f"write {p}", "-"))
    need(v.get("write /apps/probe-w") in ("ENOENT", "EROFS", "EACCES"), "/apps is not in the view", out)
    for p in ["/data/appdata/probe/w1", "/tmp/probe-w", f"{H}/.local/share/probe/w2", f"{H}/Downloads/probe-w"]:
        need(v.get(f"write {p}") == "ok", f"write {p} works", v.get(f"write {p}", "-"))
    need(v.get("cat /data/appdata/probe/w2") == "x|", "both data folder places are one folder", out)
    need(v.get("parent exe") == "/usr/bin/probe", "/proc/self/exe is the view's path", out)
    need(v.get("env LD_LIBRARY_PATH") == "(unset)" and v.get("env LD_PRELOAD") == "(unset)" and v.get("env PATH") == "/usr/bin"
         and v.get("env BITSOS_PROGRAM") == "probe", "environment: no LD_*, PATH=/usr/bin, BITSOS_PROGRAM", out)
    need(v.get("child exe") == "/usr/bin/probe" and v.get("child") == "ok" and v.get("reexec status") == "0",
         "a re-exec through /proc/self/exe works (zygote pattern)", out)
    need("event=pkg.run.start name=probe version=1" in err and "event=pkg.run.program_id name=probe" in err, "start events", err)
    ld = f"{R}/apps/probe/1/root/usr/lib/ld-linux-x86-64.so.2"
    st2, o2, e2 = run(c, [BB, "stat", "-L", "-c", "%i", ld])
    it = v.get("interp", "-").split()
    need(ok(st2) and len(it) == 2 and "ld-linux-x86-64.so.2" in it[0] and it[1] == o2.strip(),
         "the interpreter is the tree's own ld-linux-x86-64.so.2 (same inode)", f"{v.get('interp')} vs {o2}{e2}")
    st2, o2, e2 = run(c, [BB, "ls", f"{R}/data/appdata/probe", f"{H}/Downloads", f"{H}/Documents"])
    seen, cur = {}, None
    for l in o2.splitlines():
        if l.endswith(":"):
            cur = l[:-1]
            seen[cur] = set()
        elif l.strip() and cur:
            seen[cur] |= set(l.split())
    need(seen.get(f"{R}/data/appdata/probe") == {"w1", "w2"} and seen.get(f"{H}/Downloads") == {"probe-w"} and seen.get(f"{H}/Documents") == set(),
         "the writes landed in the data folder and Downloads, none in Documents", o2 + e2)
    st2, o2, e2 = run(c, [BB, "ls", "/tmp/probe-w"])
    need(not ok(st2), "the view's /tmp is private", o2 + e2)
    need(before is not None and mounts(c) == before, "no mount is left in the caller's view", f"{before}")
    st, out, err = run(c, [BR, "--root", f"{R}/apps/probe/1/root", "--", "probe", "child"], ENV)
    need(ok(st) and "child = ok" in out, "bitsrun --root TREE -- CMD", f"{st}\n{out}{err}")
    st, out, err = run(c, [BR, "--store", R, "nosuch"], ENV)
    need(not ok(st) and "nicht installiert" in err and "event=pkg.run.refused" in err, "an unknown program is refused in German", f"{st}\n{err}")
    for args, want in [(["../x"], "Programmname"), (["a/b"], "Programmname"), (["--version", "../1", "probe"], "Version"),
                       (["--version", "1:2-1", "probe"], "Epoche"), (["--version", "9", "probe"], "nicht installiert")]:
        st, out, err = run(c, [BR, "--store", R] + args, ENV)
        need(not ok(st) and want in err and "event=pkg.run.refused" in err, f"bitsrun {' '.join(args)} is refused in German", f"{st}\n{err}")
    st, out, err = run(c, [BP, "--root", R, "remove", "probe"])
    need(ok(st), "remove probe", f"{st}\n{out}{err}")
    left = listing(c, R)
    need(left is not None and not [l for l in left if "/probe" in l[len(R):]] and mounts(c) == before,
         "B2: after remove nothing of probe is left in the store and no mount", "\n".join(left or []))
    run(c, [BB, "chmod", "-R", "u+w", W])
    run(c, [BB, "rm", "-rf", W])
    if fail:
        raise SystemExit("RED: see FAIL lines above")
    print("PK-3 bitsrun on V2 (QEMU): green")


main()
