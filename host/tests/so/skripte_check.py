#!/usr/bin/env python3
"""EINS-D on V2 through MCP (QEMU hostfwd) and the root console shell (QMP keys): the set's Python as a command.
Usage: skripte_check.py <host port> <token> <qmp socket> <serial log> <http port>. Exit 0 = green.
Root installs python from the fixture mirror into a scratch root (typed into the console shell); uid 1000 (MCP)
runs /bin/python3 (the image's link to bits) with BITS_APPS naming that root."""
import base64
import http.client
import os
import json
import re
import socket
import sys
import time

sys.dont_write_bytecode = True
PORT, TOKEN, QMP_PATH, LOG, HTTP = int(sys.argv[1]), sys.argv[2], sys.argv[3], sys.argv[4], int(sys.argv[5])
W = "/tmp/skr"
R = W + "/root"
M = f"http://10.0.2.2:{HTTP}"
HOME = "/data/home/1000"
ENV = {"PATH": "/bin:/usr/bin", "HOME": HOME, "XDG_RUNTIME_DIR": "/run/user/1000", "BITS_APPS": R, "LANG": "de_DE.UTF-8"}
next_id = 0

# What the interpreter must have on the program's own glibc stage, and bitsos inside the view.
PROBE = r"""
import bz2, ctypes, curses, decimal, hashlib, json, locale, lzma, os, readline, sqlite3, ssl, sys, zlib
import bitsos
ctx = ssl.create_default_context()
assert sqlite3.connect(":memory:").execute("select 6*7").fetchone()[0] == 42
assert zlib.decompress(zlib.compress(b"bits" * 100)) == b"bits" * 100
assert lzma.decompress(lzma.compress(b"x")) == b"x" and bz2.decompress(bz2.compress(b"y")) == b"y"
assert ctypes.CDLL(None).getpid() == os.getpid()
assert str(decimal.Decimal("0.1") + decimal.Decimal("0.2")) == "0.3"
assert hashlib.sha256(b"").hexdigest().startswith("e3b0c442")
print("locale", locale.setlocale(locale.LC_ALL, "de_DE.UTF-8"))
assert ctx.cert_store_stats()["x509_ca"] > 0, ctx.cert_store_stats()
print("ca", ctx.cert_store_stats()["x509_ca"])
open("/tmp/skr/aus-python.txt", "w").write("Grüße aus der Sicht")
open(os.path.join(os.environ["HOME"], "aus-python.txt"), "w").write("im Heim")
werte = bitsos.messwerte.liste()
print("metrics", len(werte), werte[0]["name"] if werte else "-")
print("befehle", [b["program"] for b in bitsos.programm.befehle() if b["installed"]])
print("version", sys.version.split()[0], ssl.OPENSSL_VERSION, "sqlite", sqlite3.sqlite_version)
print("SKRIPTE-OK")
"""


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
            c = http.client.HTTPConnection("127.0.0.1", PORT, timeout=300)
            rpc(c, "initialize", {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "eins-d", "version": "1"}})
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


def run(c, argv, env=None, timeout_ms=120000):
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


SOURCES = f'[[source]]\nname = "fixture"\nservers = ["{M}/$repo"]\nsig_db = "optional"\nkeyring = "archlinux"\n'

# Root's part, typed into the console shell (the word "skr" has no y or z).
GO = f"""#!/bin/sh
B=/bin/bitspkg
o() {{ n=$1; shift; "$@" > {W}/out/$n.txt 2>&1; echo "exit $?" >> {W}/out/$n.txt; }}
mkdir -p {W}/out {R}/etc/bitspkg/keyrings {HOME}
chown 1000:1000 {HOME}
cp {W}/keyrings/* {R}/etc/bitspkg/keyrings/
o refresh $B --root {R} --settings none refresh --sources {W}/src.toml
o python $B --root {R} --settings none install python --sources {W}/src.toml
o rootpy /bin/python3 -c 1
id > {W}/out/id.txt
chmod -R a+rX {W}/out
chmod 1777 {W}
echo done > {W}/out/done
"""


def main():
    c = connect(300)
    st, out, err = run(c, ["/bin/busybox", "mkdir", "-p", W + "/keyrings"])
    need(ok(st), "scratch folder", f"{st}{out}{err}")
    for f in sorted(os.listdir("/usr/share/pacman/keyrings")):
        put(c, f"{W}/keyrings/{f}", open(f"/usr/share/pacman/keyrings/{f}", "rb").read())
    put(c, W + "/src.toml", SOURCES.encode())
    put(c, W + "/go", GO.encode())
    st, out, err = run(c, ["/bin/busybox", "ls", "-l", "/bin/python3", "/bin/rg", "/bin/git"])
    need(ok(st) and out.count("-> bits") == 3, "the image links the set's commands to bits", out + err)
    st, out, err = run(c, ["/bin/python3", "-c", "1"], ENV)
    need(not ok(st) and "nicht installiert" in out + err and "bits programm satz skripte" in out + err, "before the install: python3 says how it comes", f"{st}\n{out}{err}")

    wait_serial(r"init: started the console shell", "the console shell", 120)
    q = Qmp(QMP_PATH)
    q.type("\n")
    q.type(f"sh {W}/go\n")
    end = time.time() + 1500
    while time.time() < end and read(c, f"{W}/out/done") is None:
        time.sleep(5)
    need(read(c, f"{W}/out/done") is not None, "the root run finished", "\n".join(serial().splitlines()[-40:]))
    o = {n: read(c, f"{W}/out/{n}.txt") or "" for n in ["refresh", "python", "rootpy", "id"]}
    need("uid=0" in o["id"], "the run was root's", o["id"])
    need(o["refresh"].rstrip().endswith("exit 0"), "refresh from the fixture mirror", o["refresh"])
    py = o["python"]
    need(py.rstrip().endswith("exit 0") and "Installiert: python " in py, "python installed from the fixture mirror as root", py)
    need("Hook 10-glibc-locale-gen: emuliert (localedef)" in py and "Hook 11-glibc-ldconfig: emuliert (ldconfig)" in py, "the closure's glibc: locales and ld.so.cache made", py)
    for part in ["openssl-3", "sqlite", "libffi", "ncurses", "readline", "xz", "bzip2", "zlib-ng-compat", "mpdecimal"]:
        need(re.search(rf"\b{re.escape(part)}\b", py), f"the closure holds {part}", py)
    need("nicht als root" in o["rootpy"] and o["rootpy"].rstrip().endswith("exit 5"), "root's python3 is refused as bitsrun refuses root", o["rootpy"])

    st, out, err = run(c, ["/bin/bits", "programm", "befehle"], ENV)
    need(ok(st) and re.search(r"python\s+python3 python\s+\d\S+", out), "bits programm befehle: python installed", out + err)
    put(c, W + "/probe.py", PROBE.encode())
    st, out, err = run(c, ["/bin/python3", W + "/probe.py"], ENV, 300000)
    need(ok(st) and "SKRIPTE-OK" in out, "python3 in its command view: ssl, sqlite3, zlib, ctypes, curses, readline, lzma, bz2, decimal, bitsos", f"{st}\n{out}{err}")
    need("locale de_DE.UTF-8" in out, "the closure's de_DE.UTF-8", out)
    m = re.search(r"metrics (\d+) ", out)
    need(m and int(m.group(1)) > 0, "bitsos reads the kernel's measurements through bits inside the view", out)
    need("befehle ['python']" in out, "bitsos.programm.befehle sees the installed program", out)
    need(read(c, W + "/aus-python.txt") == "Grüße aus der Sicht", "the view shares /tmp with the system", read(c, W + "/aus-python.txt"))
    need(read(c, HOME + "/aus-python.txt") == "im Heim", "the view shares the home with the system", read(c, HOME + "/aus-python.txt"))
    need(re.search(r"^ca [1-9]", out, re.M), "the system's CA bundle is trusted (SSL_CERT_FILE)", out)
    print("info " + " ".join(l for l in out.splitlines() if l.startswith("version")), flush=True)
    # A set command started from inside another command's view: a view inside the view (nested namespaces).
    nested = "import subprocess; r = subprocess.run(['python3', '-c', 'print(6*7)'], capture_output=True, text=True); print('innen', r.returncode, r.stdout.strip(), r.stderr.strip()[-300:])"
    st, out, err = run(c, ["/bin/python3", "-c", nested], ENV, 300000)
    need(ok(st) and "innen 0 42" in out, "python3 starts python3 from inside its command view (a view in the view)", f"{st}\n{out}{err}")
    st, out, err = run(c, ["/bin/bits", "befehl", "python3", "--version"], ENV)
    need(ok(st) and out.startswith("Python 3."), "bits befehl python3 --version", out + err)


if __name__ == "__main__":
    main()
