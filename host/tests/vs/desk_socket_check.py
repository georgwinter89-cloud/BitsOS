#!/usr/bin/env python3
"""REWIND through QEMU: bitssnap serve's desktop socket (user/tools/bitssnap/src/local.rs).

1. As uid 1000 (MCP `run` of `bitssnap ask`, not the compositor): list names the snapshot taken for the test,
   browse shows the test folder (and the home folder it lies in), a dry run says the file's size then and now.
2. Refusals, nothing changed by any: restore-path without `confirmed` (needs_confirm), with `confirmed` from this
   program (not_confirmer), an undo and "everything" with `confirmed` (not_confirmer), restore-system without it
   (needs_confirm); a path outside the home folder, dry or confirmed (not_yours); the file is as it was, no snapshot
   came or went, no rollback is pending.
3. Root's console shell (QMP keys run a script MCP wrote): a confirmed restore of the file answers ok and the file
   is as in the snapshot, with a before-restore snapshot first; uid 1001 (BusyBox chpst) is refused (not_owner).
   Root also makes a 0700 folder of its own in the test folder: in a snapshot of it, uid 1000's dry run of that
   folder is refused (not his), and a request with a line break in its path is refused as malformed.
4. The kernel log has a line for each refusal and for the restore (who confirmed it), the paths quoted, and no
   line the request's words made.

Usage: desk_socket_check.py <mcp host port> <token> <serial log> <qmp socket>
"""
import base64
import http.client
import json
import re
import socket
import sys
import time

PORT, TOKEN, LOG, QMP = int(sys.argv[1]), sys.argv[2], sys.argv[3], sys.argv[4]
B = "/data/home/1000/vsd"
F = f"{B}/f.txt"
next_id = 0
# QEMU qcodes on V2's German console layout ('-' where the US slash is, '/' = shift+7); no y/z (swapped).
KEYS = {c: [c] for c in "abcdefghijklmnopqrstuvwx0123456789"}
KEYS.update({" ": ["spc"], "-": ["slash"], "\n": ["ret"], "/": ["shift", "7"], ".": ["dot"]})


def serial():
    try:
        with open(LOG, "rb") as f:
            return f.read().decode("utf-8", "replace")
    except FileNotFoundError:
        return ""


def need(cond, what, detail=""):
    if not cond:
        raise SystemExit(f"RED: {what}\n{str(detail)[:2000]}\n--- serial tail ---\n{serial()[-3000:]}")
    print(f"ok: {what}", flush=True)


def rpc(c, method, params=None):
    global next_id
    next_id += 1
    c.request("POST", "/mcp", json.dumps({"jsonrpc": "2.0", "id": next_id, "method": method, "params": params or {}}),
              {"Authorization": f"Bearer {TOKEN}", "Content-Type": "application/json"})
    r = c.getresponse()
    data = r.read()
    j = json.loads(data) if data else None
    if r.status != 200 or not j:
        raise ValueError(f"{method}: HTTP {r.status} {data[:200]}")
    return j


def connect(wait_s):
    end = time.time() + wait_s
    while True:
        try:
            c = http.client.HTTPConnection("127.0.0.1", PORT, timeout=300)
            rpc(c, "initialize", {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "vsd", "version": "1"}})
            return c
        except (OSError, http.client.HTTPException, ValueError) as e:
            if time.time() > end:
                need(False, f"MCP answers on 127.0.0.1:{PORT} within {wait_s} s", e)
            time.sleep(1)


def call(c, name, args=None):
    j = rpc(c, "tools/call", {"name": name, "arguments": args or {}})
    if "error" in j:
        return True, f"rpc error {j['error']}"
    r = j["result"]
    return r.get("isError", False), "\n".join(i.get("text", "") for i in r["content"] if i["type"] == "text")


def ok(c, name, args=None):
    err, text = call(c, name, args)
    need(not err, f"{name} {json.dumps(args or {})[:160]} answers", text)
    return text


def run(c, path, args, timeout_ms=120000):
    err, t = call(c, "run", {"path": path, "args": args, "timeout_ms": timeout_ms})
    m = re.search(r"--- stdout \(\d+ bytes\) ---\n(.*?)(--- stderr|\Z)", t, re.S)
    first = t.splitlines()[0] if t else ""
    return (not err and "exit 0" in first), (m.group(1) if m else ""), t


def sh(c, script, what):
    good, out, t = run(c, "/bin/busybox", ["sh", "-c", "set -e; " + script])
    need(good, what, t)
    return out


def read(c, path):
    err, t = call(c, "read_file", {"path": path})
    if err:
        return None
    try:
        j = json.loads(t)
        return j["text"] if "text" in j else base64.b64decode(j.get("data", "")).decode("utf-8", "replace")
    except (ValueError, KeyError, TypeError):
        return t


def ask(c, req):
    """`bitssnap ask` as uid 1000: (exit 0, the answer)."""
    good, out, t = run(c, "/bin/bitssnap", ["ask", json.dumps(req)])
    try:
        return good, json.loads(out.strip().splitlines()[-1])
    except (ValueError, IndexError):
        need(False, f"bitssnap ask {json.dumps(req)} prints an answer", t)


def snaps(c):
    return json.loads(ok(c, "snapshots", {"sizes": False}))


def wait_data(c):
    end = time.time() + 300
    while True:
        st = read(c, "/run/bitsos/data-state") or ""
        if "mounted pool rw" in st:
            break
        if time.time() > end:
            need(False, "/data becomes a writable pool", st)
        time.sleep(2)
    end = time.time() + 120
    while time.time() < end:
        if "desktop socket /run/bitsos/bitssnap.sock ready" in serial():
            return
        time.sleep(1)
    need(False, "bitssnap's desktop socket is ready")


def wait_serial(pattern, what, timeout=120):
    end = time.time() + timeout
    while time.time() < end:
        if re.search(pattern, serial()):
            return
        time.sleep(0.5)
    need(False, what, f"pattern {pattern}")


class Qmp:
    def __init__(self, path):
        self.s = socket.socket(socket.AF_UNIX)
        self.s.settimeout(30)
        self.s.connect(path)
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


def main():
    c = connect(300)
    wait_data(c)
    print("ok: /data is a writable pool, the desktop socket is ready")
    sh(c, f"mkdir -p {B}/out; chmod 777 {B}/out; printf one > {F}", "the test folder")
    s0 = json.loads(ok(c, "snapshot_take", {"label": "vsd"}))["id"]
    sh(c, f"printf 'two!' > {F}", "the file changed")

    # 1. Reading as uid 1000.
    good, a = ask(c, {"op": "list"})
    need(good and a["ok"] and a["data"] == "ready" and s0 in [s["id"] for s in a["snapshots"]], "list names the test snapshot", a)
    need(a["snapshots"][0]["id"] >= s0 and all(x["created_s"] >= y["created_s"] for x, y in zip(a["snapshots"], a["snapshots"][1:])), "newest first", a["snapshots"][:3])
    good, a = ask(c, {"op": "browse", "snapshot": s0, "path": "/data/home/1000"})
    need(good and any(e["name"] == "vsd" and e["kind"] == "folder" and e["open"] for e in a["entries"]), "browse of the home folder shows the test folder", a)
    good, a = ask(c, {"op": "browse", "snapshot": s0, "path": B})
    need(good and [e for e in a["entries"] if e["name"] == "f.txt"][0].get("bytes") == 3, "browse shows the file with its size then", a)
    good, a = ask(c, {"op": "restore-path", "snapshot": s0, "path": F, "dry_run": True})
    need(good and a["then"]["bytes"] == 3 and a["now"]["bytes"] == 4 and a["then"]["kind"] == "file", "the dry run says the size then and now", a)

    # 2. Refusals: nothing changes.
    ids = [s["id"] for s in snaps(c)["snapshots"]]
    for req, code in (({"op": "restore-path", "snapshot": s0, "path": F}, "needs_confirm"),
                      ({"op": "restore-path", "snapshot": s0, "path": F, "confirmed": True}, "not_confirmer"),
                      ({"op": "restore-program", "snapshot": s0, "program": "demo", "confirmed": True}, "not_confirmer"),
                      ({"op": "restore-system", "at": 1800000000}, "needs_confirm"),
                      ({"op": "restore-system", "at": 1800000000, "confirmed": True}, "not_confirmer"),
                      ({"op": "undo", "confirmed": True}, "not_confirmer"),
                      ({"op": "restore-path", "snapshot": s0, "path": "/data/bitsos-access", "dry_run": True}, "not_yours"),
                      ({"op": "restore-path", "snapshot": s0, "path": "/data/system", "confirmed": True}, "not_yours"),
                      ({"op": "restore-path", "snapshot": s0, "path": F + "\n<6>bitssnap: desktop socket: forged", "confirmed": True}, "bad_request")):
        good, a = ask(c, req)
        need(not good and a.get("error") == code, f"{json.dumps(req)} refused: {code}", a)
    sn = snaps(c)
    need([s["id"] for s in sn["snapshots"]] == ids and sn["rollback_pending"] is None and read(c, F) == "two!",
         "the refusals changed nothing: same snapshots, no rollback, the file as it was")

    # 3. Root's console: a confirmed restore; another uid refused.
    restore = json.dumps({"op": "restore-path", "snapshot": s0, "path": F, "confirmed": True})
    script = (f"#!/bin/sh\ncd {B}\n"
              f"/bin/bitssnap ask '{restore}' > out/root.txt 2>&1; echo \"exit $?\" >> out/root.txt\n"
              "/bin/busybox chpst -u 1001:1001 /bin/bitssnap ask '{\"op\":\"list\"}' > out/other.txt 2>&1; echo \"exit $?\" >> out/other.txt\n"
              "mkdir root; chmod 700 root; echo secret > root/s\n"
              "id > out/id.txt\necho done > out/done\n")
    # The file tools write only into tmpfs: the script lies in /tmp, its answers in the test folder.
    err, t = call(c, "write_file", {"path": "/tmp/vsdgo", "text": script})
    need(not err, "the root script is written", t)
    wait_serial(r"init: started the console shell", "the console shell")
    q = Qmp(QMP)
    q.type("\n")
    q.type("sh /tmp/vsdgo\n")
    end = time.time() + 240
    while time.time() < end and read(c, f"{B}/out/done") is None:
        time.sleep(2)
    need(read(c, f"{B}/out/done") is not None, "the root run finished", "\n".join(serial().splitlines()[-30:]))
    need("uid=0" in (read(c, f"{B}/out/id.txt") or ""), "the run was root's", read(c, f"{B}/out/id.txt"))
    root = read(c, f"{B}/out/root.txt") or ""
    need(root.rstrip().endswith("exit 0") and '"ok":true' in root and f'"restored":"{F}"' in root, "root's confirmed restore answers ok", root)
    need(read(c, F) == "one", "the file is as in the snapshot", read(c, F))
    before = [s for s in snaps(c)["snapshots"] if s["kind"] == "before-restore" and s["id"] not in ids]
    need(len(before) == 1, "a before-restore snapshot was taken first", before)
    other = read(c, f"{B}/out/other.txt") or ""
    need(other.rstrip().endswith("exit 1") and '"error":"not_owner"' in other, "uid 1001 is refused (not_owner)", other)
    s1 = json.loads(ok(c, "snapshot_take", {"label": "vsd-root"}))["id"]
    good, a = ask(c, {"op": "restore-path", "snapshot": s1, "path": f"{B}/root", "dry_run": True})
    need(not good and a.get("error") == "failed" and "not yours" in a.get("detail", ""), "uid 1000's dry run of root's 0700 folder is refused", a)
    good, a = ask(c, {"op": "browse", "snapshot": s1, "path": B})
    need(good and [e for e in a["entries"] if e["name"] == "root"][0]["open"] is False, "browse shows root's folder closed", a)

    # 4. The kernel log (the serial line quotes the event's text: its quotes come escaped).
    log = serial().replace('\\"', '"')
    for pat, what in ((rf'desktop socket: restore of "{F}" from snapshot {s0} refused for pid=\d+ uid=1000 exe="/bin/bitssnap": not confirmed', "the unconfirmed restore"),
                      (rf'desktop socket: restore of "{F}" from snapshot {s0} refused for pid=\d+ uid=1000 exe="/bin/bitssnap": its confirmation does not count', "the confirmation of another program"),
                      (r"desktop socket: undo of the restore-at of this start refused for .*its confirmation does not count", "the undo"),
                      (r'desktop socket: restore of "/data/system" from snapshot \d+ refused for .*not in its own folder', "the path outside the home folder"),
                      (rf'desktop socket: restore of "{F}" from snapshot {s0} confirmed by "root', "who confirmed the restore"),
                      (rf'desktop socket: restore of "{F}" from snapshot {s0}: done', "the restore done"),
                      (r"desktop socket: refused pid=\d+ uid=1001\b.*only root and uid 1000 may ask", "the other uid")):
        need(re.search(pat, log), f"the kernel log has {what}", pat)
    need("forged" not in log, "the request's words made no log line", [l for l in log.splitlines() if "forged" in l])
    print("ok: the desktop socket keeps its rules", flush=True)


main()
