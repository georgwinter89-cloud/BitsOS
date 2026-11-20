#!/usr/bin/env python3
"""PK-6a on V2 through MCP (QEMU hostfwd) and the root console shell (QMP keys): bitspkg refresh from the fixture
mirror. Usage: pk_refresh_check.py <host port> <token> <qmp socket> <serial log> <http port>. Exit 0 = green.
The mirror is the host's 127.0.0.1:<http port>, the guest's 10.0.2.2:<http port>."""
import base64
import http.client
import json
import re
import socket
import sys
import time

PORT, TOKEN, QMP_PATH, LOG, HTTP = int(sys.argv[1]), sys.argv[2], sys.argv[3], sys.argv[4], int(sys.argv[5])
BP = "/bin/bitspkg"
W = "/tmp/pkref"
M = f"http://10.0.2.2:{HTTP}"
next_id = 0


def need(cond, what, detail=""):
    if not cond:
        raise SystemExit(f"RED: {what}\n{str(detail)[:4000]}")
    print(f"ok   {what}", flush=True)


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
            rpc(c, "initialize", {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "pk6a", "version": "1"}})
            return c
        except (OSError, http.client.HTTPException, ValueError) as e:
            if time.time() > end:
                raise SystemExit(f"RED: no MCP answer on 127.0.0.1:{PORT}: {e}")
            time.sleep(1)


def call(c, name, args=None):
    r = rpc(c, "tools/call", {"name": name, "arguments": args or {}})
    return r["isError"], "\n".join(i.get("text", "") for i in r["content"] if i["type"] == "text")


def read(c, path):
    err, t = call(c, "read_file", {"path": path})
    if err:
        return None
    try:
        j = json.loads(t)
        return j["text"] if "text" in j else base64.b64decode(j.get("data", "")).decode("utf-8", "replace")
    except (ValueError, KeyError, TypeError):
        return t


def put(c, path, data):
    err, t = call(c, "write_file", {"path": path, "data": base64.b64encode(data).decode(), "offset": 0})
    if err:
        raise SystemExit(f"RED: write_file {path}: {t}")


def run(c, argv, timeout_ms=60000):
    err, t = call(c, "run", {"path": argv[0], "args": argv[1:], "timeout_ms": timeout_ms, "kill_after_ms": timeout_ms})
    if err or "\n--- stdout (" not in t:
        return None, t, t
    head, rest = t.split("\n--- stdout (", 1)
    out, errpart = rest.split("\n--- stderr (", 1)
    body = lambda part: part.split(") ---", 1)[1].removeprefix("\n")
    return head.splitlines()[0], body(out), body(errpart)


def ok(st):
    return bool(st) and st.endswith("exit 0")


def serial():
    with open(LOG, "rb") as f:
        return f.read().decode("utf-8", "replace")


def wait_serial(pattern, what, timeout=60):
    end = time.time() + timeout
    rx = re.compile(pattern, re.M)
    while time.time() < end:
        if rx.search(serial()):
            return
        time.sleep(0.5)
    raise SystemExit(f"RED: {what}: no /{pattern}/ in the serial log\n" + "\n".join(serial().splitlines()[-30:]))


# QEMU qcodes on V2's German console layout ('-' where the US slash is, '/' = shift+7); no y/z (swapped).
KEYS = {c: [c] for c in "abcdefghijklmnopqrstuvwx0123456789"}
KEYS.update({" ": ["spc"], "-": ["slash"], "\n": ["ret"], "/": ["shift", "7"]})


class Qmp:
    def __init__(self, path):
        end = time.time() + 60
        while True:
            try:
                self.s = socket.socket(socket.AF_UNIX)
                self.s.settimeout(30)
                self.s.connect(path)
                break
            except OSError:
                if time.time() > end:
                    raise SystemExit("RED: no QMP socket")
                time.sleep(0.5)
        self.f = self.s.makefile("rw")
        self.f.readline()
        self.cmd("qmp_capabilities")

    def cmd(self, name, args=None):
        self.f.write(json.dumps({"execute": name, "arguments": args or {}}) + "\n")
        self.f.flush()
        while True:
            r = json.loads(self.f.readline())
            if "return" in r:
                return r["return"]
            if "error" in r:
                raise SystemExit(f"RED: qmp {name}: {r['error']}")

    def type(self, text):
        for ch in text:
            self.cmd("send-key", {"keys": [{"type": "qcode", "data": n} for n in KEYS[ch]], "hold-time": 40})
            time.sleep(0.08)


def sources(name, servers, sig):
    return f'[[source]]\nname = "{name}"\nservers = [{", ".join(json.dumps(s) for s in servers)}]\nsig_db = "{sig}"\nkeyring = "archlinux"\n\n'


GO = f"""#!/bin/sh
B={BP}
R="--root {W}/root --settings none"
o() {{ n=$1; shift; "$@" > {W}/out/$n.txt 2>&1; echo "exit $?" >> {W}/out/$n.txt; }}
mkdir -p {W}/out
o refresh1 $B $R --sources {W}/src1.toml refresh
o search $B $R --sources {W}/src1.toml search jq
o info $B $R --sources {W}/src1.toml info --repo jq
o refresh2 $B $R --sources {W}/src2.toml refresh
o search2 $B $R --sources {W}/src1.toml search oniguruma
o refresh3 $B $R --sources {W}/src3.toml refresh
o snap $B snapshot {W}/root
id > {W}/out/id.txt
chmod -R a+rX {W}/out
echo done > {W}/out/done
"""


def main():
    c = connect(300)
    st, out, err = run(c, ["/bin/busybox", "mkdir", "-p", W])
    need(ok(st), "scratch folder", f"{st}{out}{err}")
    put(c, W + "/src1.toml", (sources("fixture", [M + "/main/$repo"], "optional") + sources("bad", [M + "/main/$repo"], "required")
                              + sources("nosig", [M + "/main/$repo"], "required")).encode())
    put(c, W + "/src2.toml", sources("fixture", [M + "/old/$repo"], "optional").encode())
    put(c, W + "/src3.toml", sources("fixture", [M + "/old/$repo", M + "/main/$repo"], "optional").encode())
    put(c, W + "/go", GO.encode())

    # uid 1000, the real root: settingsd's keys, D1's six sources, nothing fetched (D5).
    st, out, err = run(c, [BP, "sources"])
    names = re.findall(r"^([a-z0-9-]+): ", out, re.M)
    need(ok(st) and names == ["cachyos-v3", "cachyos-extra-v3", "cachyos-core-v3", "cachyos", "core", "extra"] and out.count("noch nicht geladen") == 6,
         "bitspkg sources: D1's six sources in pacman's order, none loaded yet", f"{st}\n{out}{err}")
    print(f"     sources from: {out.splitlines()[0]}")
    st, out, err = run(c, [BP, "vercmp", "1:1.0", "2.0"])
    need(ok(st) and out.strip() == "1", "bitspkg vercmp 1:1.0 2.0 = 1 (epoch wins)", f"{st}\n{out}{err}")

    wait_serial(r"init: started the console shell", "the console shell", 120)
    q = Qmp(QMP_PATH)
    q.type("\n")
    q.type(f"sh {W}/go\n")
    end = time.time() + 240
    while time.time() < end and read(c, W + "/out/done") is None:
        time.sleep(2)
    need(read(c, W + "/out/done") is not None, "the root run finished", "\n".join(serial().splitlines()[-30:]))
    o = {n: read(c, f"{W}/out/{n}.txt") or "" for n in ["refresh1", "search", "info", "refresh2", "search2", "refresh3", "snap", "id"]}
    need("uid=0" in o["id"], "the run was root's", o["id"])
    r1 = o["refresh1"]
    need(re.search(r"^fixture: aktualisiert, 38 Pakete, von " + re.escape(M) + "/main/fixture$", r1, re.M), "refresh loads fixture (38 packages, unsigned database allowed)", r1)
    need(re.search(r"^bad: abgelehnt: Signatur der Datenbank abgelehnt: ", r1, re.M), "bad is refused with its reason (signature)", r1)
    need(re.search(r"^nosig: abgelehnt: Signatur der Datenbank fehlt", r1, re.M), "nosig is refused: its required signature is missing", r1)
    need(r1.rstrip().endswith("exit 1"), "refresh says by its exit code that a source was refused", r1)
    need("event=pkg.db.bad repo=bad" in r1 and "event=pkg.refresh.done repo=fixture" in r1, "events pkg.db.bad and pkg.refresh.done", r1)
    need(re.search(r"^fixture/jq \S+$", o["search"], re.M) and o["search"].rstrip().endswith("exit 0"), "search jq finds fixture/jq", o["search"])
    need("braucht: glibc oniguruma" in o["info"], "info --repo jq names its dependencies", o["info"])
    r2 = o["refresh2"]
    need(re.search(r"^fixture: abgelehnt: Datenbank älter als die zuletzt angenommene", r2, re.M) and "event=pkg.db.older repo=fixture" in r2,
         "an older database from the second mirror path is refused (pkg.db.older)", r2)
    need(re.search(r"^fixture/oniguruma ", o["search2"], re.M), "the database from before stays in use", o["search2"])
    r3 = o["refresh3"]
    need(re.search(r"^fixture: unverändert, 38 Pakete, von " + re.escape(M) + "/main/fixture$", r3, re.M) and "/old/fixture: Datenbank älter" in r3,
         "with both paths the older is refused and the next mirror's database taken (unchanged)", r3)
    snap = [l for l in o["snap"].splitlines() if l and not l.startswith("exit ")]
    bad = [l for l in snap if not re.match(r"^data(/bitspkg(/sync(/\S+)?)?)? (dir|file) \d+ 0:0( |$)", l)]
    need(snap and not bad and not any("download-" in l or ".part " in l for l in snap),
         "refresh wrote only root's files below data/bitspkg/sync and left no download folder (B2)", "\n".join(bad or snap))
    need("fixture.db file 644 0:0" in o["snap"], "the database was handed over to root (0644, uid 0)", o["snap"])


main()
