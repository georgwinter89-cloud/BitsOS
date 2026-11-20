#!/usr/bin/env python3
"""PK-6e on V2 through MCP (QEMU hostfwd) and the root console shell (QMP keys): `bitspkg update` against the fixture
mirror's states a (main), b (upd/b: oniguruma rebuilt and signed by TEST key B, bitstest-keyring 1-1 bringing B,
bitstest-mirrorlist 1-1) and c (upd/c: bitstest-keyring 2-1 signed by the unknown TEST key C).
Usage: pk_update_check.py <host port> <token> <qmp socket> <serial log> <http port> <test keyring dir>. Exit 0 = green.
The mirror is the host's 127.0.0.1:<http port>, the guest's 10.0.2.2:<http port>. Root runs bitspkg (typed into
the console shell); MCP (uid 1000) only puts the files in and reads the results."""
import base64
import http.client
import json
import os
import re
import socket
import sys
import time

sys.dont_write_bytecode = True
PORT, TOKEN, QMP_PATH, LOG, HTTP, TESTKEYS = int(sys.argv[1]), sys.argv[2], sys.argv[3], sys.argv[4], int(sys.argv[5]), sys.argv[6]
BP, BR, BB = "/bin/bitspkg", "/bin/bitsrun", "/bin/busybox"
W = "/tmp/pkup"
R = W + "/root"
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
            rpc(c, "initialize", {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "pk6e", "version": "1"}})
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
    step = 96 * 1024
    for off in range(0, max(len(data), 1), step):
        err, t = call(c, "write_file", {"path": path, "data": base64.b64encode(data[off:off + step]).decode(), "offset": off})
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



def src(servers):
    return f'[[source]]\nname = "fixture"\nservers = [{", ".join(chr(34) + s + chr(34) for s in servers)}]\nsig_db = "optional"\nkeyring = "archlinux"\n'


OLD, NEW = "1.8.2-1.1+1", "1.8.2-1.1+2"

# Root's part, typed into the console shell: every command's output into out/<name>.txt with its exit status.
GO = f"""#!/bin/sh
B="{BP} --root {R} --settings none"
o() {{ n=$1; shift; "$@" > {W}/out/$n.txt 2>&1; echo "exit $?" >> {W}/out/$n.txt; }}
mkdir -p {W}/out {R}/etc/bitspkg/keyrings
cp {W}/keyrings/* {R}/etc/bitspkg/keyrings/
o refresh $B refresh --sources {W}/a.toml
o jq $B install jq --sources {W}/a.toml
o pkgs0 {BB} ls {R}/apps/.pkgs/oniguruma
o apps0 {BP} snapshot {R}/apps
o check $B update --check --sources {W}/b.toml
o apps1 {BP} snapshot {R}/apps
o nokeys {BB} ls {R}/data/bitspkg
o upd $B update --sources {W}/b.toml
o mlist {BB} cat {R}/data/bitspkg/mirrorlist/bitstest-mirrorlist
o mref $B refresh --sources {W}/m.toml
o again $B update --sources {W}/b.toml
o rb1 $B rollback jq
o rb2 $B rollback jq
o pkgs1 {BB} ls {R}/apps/.pkgs/oniguruma
o rmold $B remove jq --version {OLD}
o pkgs2 {BB} ls {R}/apps/.pkgs/oniguruma
o verify $B verify --store
o keys1 {BP} snapshot {R}/data/bitspkg/keyrings
o bad $B update --sources {W}/c.toml
o keys2 {BP} snapshot {R}/data/bitspkg/keyrings
o older $B update --sources {W}/a.toml
o list $B list
o rmjq $B remove jq
o after {BP} snapshot {R}/apps
id > {W}/out/id.txt
chmod -R a+rX {W}/out
echo done > {W}/out/done
"""

NAMES = ["refresh", "jq", "pkgs0", "apps0", "check", "apps1", "nokeys", "upd", "mlist", "mref", "again", "rb1", "rb2", "pkgs1", "rmold",
         "pkgs2", "verify", "keys1", "bad", "keys2", "older", "list", "rmjq", "after", "id"]


def ex(t, code):
    return t.rstrip().endswith(f"exit {code}")


def main():
    c = connect(300)
    st, out, err = run(c, [BB, "mkdir", "-p", W + "/keyrings"])
    need(ok(st), "scratch folder", f"{st}{out}{err}")
    for d in ["/usr/share/pacman/keyrings", TESTKEYS]:
        for f in sorted(os.listdir(d)):
            put(c, f"{W}/keyrings/{f}", open(os.path.join(d, f), "rb").read())
    put(c, W + "/a.toml", src([f"{M}/main/$repo"]).encode())
    put(c, W + "/b.toml", src(["include:bitstest-mirrorlist", f"{M}/upd/b/$repo"]).encode())
    put(c, W + "/c.toml", src([f"{M}/upd/c/$repo"]).encode())
    put(c, W + "/m.toml", src(["include:bitstest-mirrorlist"]).encode())
    put(c, W + "/go", GO.encode())

    wait_serial(r"init: started the console shell", "the console shell", 120)
    q = Qmp(QMP_PATH)
    q.type("\n")
    q.type(f"sh {W}/go\n")
    end = time.time() + 1200
    while time.time() < end and read(c, f"{W}/out/done") is None:
        time.sleep(3)
    need(read(c, f"{W}/out/done") is not None, "the root run finished", "\n".join(serial().splitlines()[-40:]))
    o = {n: read(c, f"{W}/out/{n}.txt") or "" for n in NAMES}
    need("uid=0" in o["id"], "the run was root's", o["id"])
    need(ex(o["refresh"], 0) and ex(o["jq"], 0) and f"Installiert: jq {OLD}" in o["jq"], "jq installed from state a", o["refresh"] + o["jq"])
    ch = o["check"]
    need(ex(ch, 0) and f"jq {OLD}: 1 Teil neuer (oniguruma 6.9.10-3.1 → 6.9.10-3.1.1)" in ch, "update --check: jq has 1 Teil neuer", ch)
    need("Aktualisierung: Schlüsselbund bitstest-keyring: Abbild → 1-1 (fixture)" in ch and "Aktualisierung: Spiegelliste bitstest-mirrorlist: Abbild → 1-1" in ch
         and "3 Aktualisierungen gefunden (installiert wird nur auf Wunsch" in ch, "update --check: keyring and mirror list packages, 3 found, nothing installed", ch)
    need(o["apps0"] == o["apps1"] and "keyrings" not in o["nokeys"] and "mirrorlist" not in o["nokeys"], "update --check changed nothing in /apps and no keyring",
         o["nokeys"])
    u = o["upd"]
    need(ex(u, 0) and "Neuer Schlüsselbund geprüft:" in u and "Spiegelliste bitstest-mirrorlist 1-1:" in u, "update: keyring and mirror list packages verified and active", u)
    need(f"Installiert: jq {NEW}" in u and "Pakete: 7 (1 neu entpackt, 6 schon in /apps/.pkgs)" in u and "Download: 1 Pakete" in u,
         "update: jq's new version, only oniguruma fetched, the other 6 reused from .pkgs", u)
    need("data/bitspkg/keyrings)" in u.split("Download: 1 Pakete", 1)[1], "jq's oniguruma (signed by test key B) was checked with the new keyring", u)
    need(f"Server = {M}/upd/b/$repo" in o["mlist"] and ex(o["mref"], 0), "the installed mirror list serves a refresh", o["mlist"] + o["mref"])
    need(ex(o["again"], 0) and f"jq {NEW}: aktuell" in o["again"] and "Download" not in o["again"], "a second update finds nothing", o["again"])
    need(ex(o["rb1"], 0) and f"back from {NEW} to {OLD}" in o["rb1"] and "1 Teil älter (oniguruma 6.9.10-3.1.1 → 6.9.10-3.1)" in o["rb1"],
         "rollback: previous with its closure difference (1 Teil älter)", o["rb1"])
    need(ex(o["rb2"], 0) and "1 Teil neuer (oniguruma 6.9.10-3.1 → 6.9.10-3.1.1)" in o["rb2"], "rollback again: 1 Teil neuer", o["rb2"])
    p0, p1, p2 = (set(o[n].split()[:-2]) for n in ["pkgs0", "pkgs1", "pkgs2"])
    need(len(p0) == 1 and len(p1) == 2 and len(p2) == 1 and p2.isdisjoint(p0), "gc: the old oniguruma's .pkgs entry goes once no version uses it",
         f"{p0} {p1} {p2}\n{o['rmold']}")
    need(ex(o["rmold"], 0) and "event=pkg.pkgs.gone pkg=oniguruma" in o["rmold"], "remove --version: event pkg.pkgs.gone", o["rmold"])
    need(o["verify"].startswith("verify: all good (1 packages, 1 versions"), "verify --store clean", o["verify"])
    b = o["bad"]
    need(ex(b, 1) and "nicht im Schlüsselbund" in b and "bitstest-keyring nicht übernommen (der bisherige Schlüsselbund bleibt aktiv)" in b,
         "a newer keyring package signed by an unknown key is refused", b)
    need(o["keys1"] == o["keys2"] and "bitstest.gpg" in o["keys1"], "the refused keyring package left the active keyring as it was", o["keys1"] + o["keys2"])
    ol = o["older"]
    need(ex(ol, 1) and "Datenbank abgelehnt (oben); die bisherige gilt weiter" in ol, "an older database stays refused on the update path", ol)
    need(f"jq current {NEW}" in o["list"], "jq unchanged after the refusals", o["list"])
    need(ex(o["rmjq"], 0) and ".pkgs entries" in o["rmjq"], "jq removed", o["rmjq"])
    left = [l for l in o["after"].splitlines() if l.startswith(".pkgs/")]
    need(not left, "no .pkgs entry stays after the removal", "\n".join(left[:20]))


main()
