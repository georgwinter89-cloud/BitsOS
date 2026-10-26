#!/usr/bin/env python3
"""VS-3 through MCP (QEMU): every kind of bitssnap's tool `restore`, each undone, nothing outside its target changed.

1. Folder: a folder restored from a snapshot (tree as then), a sibling folder unchanged; undone from the
   "before-restore" snapshot (tree as before the restore).
2. Program: package `demo` 1 + data, snapshot, demo 2 and 3 installed (1 is gone from the store), data changed;
   the restore brings demo 1 back from the snapshot (current 1, previous 3, `bitspkg verify --store` clean) and its
   data folder; another program and the sibling folder unchanged; undone (current 3, data as before).
   2b. Hostile and wrong arguments of each kind (../ names, missing snapshots, paths outside /data, a time
   before every snapshot): each refused, the snapshots, rollback, PIN and test tree unchanged.
3. Volume: /data back to a snapshot at the next start: scheduled (snapshots shows it), status, cancelled, scheduled
   again, a file written after that; reboot (MCP `reboot`): the folder as in the snapshot, a "before-rollback"
   snapshot; a rollback to it, reboot: as before the first one, the late file included.
4. System: "everything as at" the volume snapshot's time: a dry run (pin 1: generation 1 ran then, 2 runs and
   starts next); with Georg's own PIN 2 a stray undo changes nothing, a real one over it is undone back to PIN 2
   and a second undo changes nothing; a real one cancelled again (no PIN, no rollback); a real one, then `system_pin 1 now`: a Reload
   into generation 1 with /data as then; undone (rollback to the new before-rollback + `system_pin off now`):
   generation 2 and /data as before.
5. A final snapshot (frozen, clean) for the host's e2fsck of every snapshot.

Usage: restore_check.py <mcp host port> <token> <serial log> <qmp socket>
"""
import base64
import http.client
import json
import re
import socket
import sys
import time

PORT, TOKEN, LOG, SOCK = int(sys.argv[1]), sys.argv[2], sys.argv[3], sys.argv[4]
B = "/data/home/1000/vs3"  # MCP's run is uid 1000
R = f"{B}/root"
BANNER = "bitsboot: BitsOS V2 loader"
next_id = 0



def running(listing, n, raw):
    """system_list's running line (RB-11: the reason in words, then bitsboot's raw name)."""
    return re.search(r"running: generation %d \([^\n]*bitsboot: %s[,)]" % (n, raw), listing) is not None

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
            rpc(c, "initialize", {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "vs3", "version": "1"}})
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


def restore(c, **args):
    return json.loads(ok(c, "restore", args))


def run(c, path, args, timeout_ms=120000):
    err, t = call(c, "run", {"path": path, "args": args, "timeout_ms": timeout_ms})
    m = re.search(r"--- stdout \(\d+ bytes\) ---\n(.*?)(--- stderr|\Z)", t, re.S)
    return (not err and "exit 0" in t.splitlines()[0] if t else False), (m.group(1) if m else ""), t


def sh(c, script, what):
    good, out, t = run(c, "/bin/busybox", ["sh", "-c", "set -e; " + script])
    need(good, what, t)
    return out


def pkg(c, *args):
    good, out, t = run(c, "/bin/bitspkg", ["--root", R, "--settings", "none", *args])
    need(good, "bitspkg " + " ".join(args), t)
    return out


def tree(c, d):
    good, out, t = run(c, "/bin/bitspkg", ["snapshot", d])
    need(good and out, f"bitspkg snapshot {d}", t)
    return out


def snaps(c):
    return json.loads(ok(c, "snapshots", {"sizes": False}))


def newest(c, kind):
    l = [s for s in snaps(c)["snapshots"] if s["kind"] == kind]
    need(l, f"a {kind} snapshot is listed")
    return max(l, key=lambda s: s["id"])


def wait_data(c):
    end = time.time() + 300
    while True:
        err, st = call(c, "read_file", {"path": "/run/bitsos/data-state"})
        if not err:
            j = json.loads(st)
            st = j.get("text") or base64.b64decode(j.get("data", "")).decode("utf-8", "replace")
        if not err and "mounted pool rw" in st:
            break
        if time.time() > end:
            need(False, "/data becomes a writable pool", st)
        time.sleep(2)
    end = time.time() + 120
    while time.time() < end:
        names = [t["name"] for t in rpc(c, "tools/list")["result"]["tools"]]
        if "restore" in names:
            return
        time.sleep(1)
    need(False, "bitssnap's tools are registered")


def wait_for(pattern, start, timeout_s, what):
    end = time.time() + timeout_s
    rx = re.compile(pattern)
    while True:
        text = serial()[start:]
        m = rx.search(text[:text.rfind("\n") + 1])
        if m:
            return m, start + m.end()
        if time.time() > end:
            need(False, f"{what} within {timeout_s} s", f"pattern: {pattern}")
        time.sleep(0.2)


def qmp_reset():
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    s.settimeout(10)
    s.connect(SOCK)
    f = s.makefile("rw")
    f.readline()
    for cmd in ("qmp_capabilities", "system_reset"):
        f.write(json.dumps({"execute": cmd}) + "\n")
        f.flush()
        while "return" not in json.loads(f.readline()):
            pass
    s.close()


def reboot(c, what):
    pos = len(serial())
    ok(c, "reboot")
    c.close()
    for attempt in range(3):
        end = time.time() + 60
        while time.time() < end and BANNER not in serial()[pos:]:
            time.sleep(0.5)
        if BANNER in serial()[pos:]:
            break
        print(f"note: {what}: the firmware never reached bitsboot ({attempt + 1}); QMP reset", flush=True)
        pos = len(serial())
        qmp_reset()
    m, pos2 = wait_for(r"bitsvol volume\.rollback_applied pool=data id=\d+ to=(\d+) kept_as=(\d+)", pos, 180, f"{what}: the rollback is carried out at the mount")
    c = connect(300)
    wait_data(c)
    return c, int(m.group(1)), int(m.group(2))


def system_list(c):
    return ok(c, "system_list")


def main():
    c = connect(300)
    wait_data(c)
    print("ok: /data is a writable pool, bitssnap's tools are there")
    t = {x["name"]: x for x in rpc(c, "tools/list")["result"]["tools"]}["restore"]
    kinds = t["inputSchema"]["properties"]["kind"]["enum"]
    need(kinds == ["path", "program", "volume", "system"] and t.get("annotations", {}).get("safety") == "delete_overwrite",
         "restore: kinds path/program/volume/system, safety delete_overwrite", t)
    l = system_list(c)
    need("pin: none" in l and running(l, 2, "NEWEST_OK"), "generation 2 runs, no pin", l)

    # 1. A folder.
    sh(c, f"mkdir -p {B}/f/sub {B}/keep; echo one > {B}/f/a; echo A > {B}/f/sub/x; echo k1 > {B}/keep/k", "the test folders")
    s0 = json.loads(ok(c, "snapshot_take", {"label": "vs3-folder"}))["id"]
    f0 = tree(c, f"{B}/f")
    sh(c, f"echo two > {B}/f/a; rm {B}/f/sub/x; echo new > {B}/f/n; echo k2 > {B}/keep/k", "the folder changed")
    f1, keep = tree(c, f"{B}/f"), tree(c, f"{B}/keep")
    r = restore(c, snapshot=s0, path=f"{B}/f")
    need(r["restored"] == f"{B}/f" and r["replaced"], "folder restore answer", r)
    need(tree(c, f"{B}/f") == f0, "the folder is as in the snapshot", tree(c, f"{B}/f"))
    need(tree(c, f"{B}/keep") == keep, "the folder beside it is unchanged")
    r = restore(c, kind="path", snapshot=r["before"], path=f"{B}/f")
    need(tree(c, f"{B}/f") == f1 and tree(c, f"{B}/keep") == keep, "undone from before-restore: as before, the sibling unchanged")

    # 2. A program.
    for v in ("1", "2", "3"):
        sh(c, f"d={B}/work/demo{v}; mkdir -p $d/bin; printf 'format = 1\\nname = \"demo\"\\nversion = \"{v}\"\\nprogram = \"bin/run\"\\n' > $d/package.toml;"
              f" printf '#!/bin/sh\\necho {v}\\n' > $d/bin/run; chmod 755 $d/bin/run; echo shared > $d/bin/lib", f"demo {v} tree")
        pkg(c, "build", f"{B}/work/demo{v}", "--out", f"{B}/work/pkg-demo{v}")
    sh(c, f"d={B}/work/other; mkdir -p $d/bin; printf 'format = 1\\nname = \"other\"\\nversion = \"1\"\\nprogram = \"bin/run\"\\n' > $d/package.toml;"
          f" printf '#!/bin/sh\\necho other\\n' > $d/bin/run; chmod 755 $d/bin/run", "other's tree")
    pkg(c, "build", f"{B}/work/other", "--out", f"{B}/work/pkg-other")
    pkg(c, "install", f"{B}/work/pkg-demo1")
    pkg(c, "install", f"{B}/work/pkg-other")
    sh(c, f"echo v1 > {R}/data/appdata/demo/state; echo o1 > {R}/data/appdata/other/o", "data folders written")
    s1 = json.loads(ok(c, "snapshot_take", {"label": "vs3-program"}))["id"]
    data1 = tree(c, f"{R}/data/appdata/demo")
    pkg(c, "install", f"{B}/work/pkg-demo2")
    pkg(c, "install", f"{B}/work/pkg-demo3")
    sh(c, f"echo v3 > {R}/data/appdata/demo/state; test ! -e {R}/apps/demo/1", "demo 3 current, version 1 gone from the store")
    data3 = tree(c, f"{R}/data/appdata/demo")
    others = [tree(c, d) for d in (f"{R}/apps/other", f"{R}/data/appdata/other", f"{B}/keep", f"{B}/f")]
    r = restore(c, kind="program", snapshot=s1, program="demo", root=R)
    need(r["version"] == "1" and r["was"] == "3" and "installed again" in r["package"] and r["data"]["restored"] == f"{R}/data/appdata/demo",
         "program restore answer: demo 1 back from the snapshot, its data folder", r)
    out = sh(c, f"readlink {R}/apps/demo/current; readlink {R}/apps/demo/previous; cat {R}/apps/demo/current/bin/run; cat {R}/data/appdata/demo/state", "demo's state")
    need(out.split() == ["1", "3", "#!/bin/sh", "echo", "1", "v1"], "current 1, previous 3, the program and its data as then", out)
    need(tree(c, f"{R}/data/appdata/demo") == data1, "the data folder is as in the snapshot")
    pkg(c, "verify", "--store")
    need([tree(c, d) for d in (f"{R}/apps/other", f"{R}/data/appdata/other", f"{B}/keep", f"{B}/f")] == others, "the other program and the folders are unchanged")
    u = restore(c, kind="program", snapshot=r["before"], program="demo", root=R)
    need(u["version"] == "3" and "switched to 3" in u["package"], "program undo answer", u)
    out = sh(c, f"readlink {R}/apps/demo/current; cat {R}/data/appdata/demo/state", "demo after the undo")
    need(out.split() == ["3", "v3"] and tree(c, f"{R}/data/appdata/demo") == data3, "undone: demo 3 and its data as before", out)
    pkg(c, "verify", "--store")
    need([tree(c, d) for d in (f"{R}/apps/other", f"{R}/data/appdata/other", f"{B}/keep", f"{B}/f")] == others, "still unchanged after the undo")
    err, text = call(c, "restore", {"kind": "program", "snapshot": s0, "program": "demo", "root": R})
    need(err and "nothing changed" in text, "a snapshot without demo: refused, nothing changed", text)

    # 2b. Hostile and wrong arguments of every kind: an error each, nothing changed anywhere.
    ids, whole = [s["id"] for s in snaps(c)["snapshots"]], tree(c, B)
    for args in ({"kind": "program", "snapshot": s1, "program": "../etc", "root": R}, {"kind": "program", "snapshot": s1, "program": "", "root": R},
                 {"kind": "program", "snapshot": s1, "program": "demo", "root": "rel"}, {"kind": "program", "snapshot": 999999, "program": "demo", "root": R},
                 {"kind": "path", "snapshot": 999999, "path": f"{B}/f"}, {"kind": "path", "snapshot": s0, "path": "/etc"},
                 {"kind": "path", "snapshot": s0, "path": f"{B}/../../etc"}, {"kind": "volume", "snapshot": 999999},
                 {"kind": "volume", "snapshot": s0, "cancel": True}, {"kind": "system", "at": 1}, {"kind": "system", "at": 1, "dry_run": True},
                 {"kind": "system", "at": "yesterday"}, {"kind": "nothing", "snapshot": s0}):
        err, text = call(c, "restore", args)
        need(err, f"restore {json.dumps(args)} is refused", text)
    sn = snaps(c)
    need([s["id"] for s in sn["snapshots"]] == ids and sn["rollback_pending"] is None and tree(c, B) == whole and "pin: none" in system_list(c),
         "the refusals changed nothing: same snapshots, no rollback, no PIN, the test tree as it was")

    # 3. The whole volume.
    sh(c, f"mkdir -p {B}/vol; echo A > {B}/vol/m", "the volume marker")
    s2 = json.loads(ok(c, "snapshot_take", {"label": "vs3-volume"}))["id"]
    va = tree(c, B)
    sh(c, f"echo B > {B}/vol/m; echo b2 > {B}/vol/extra", "the marker changed")
    r = restore(c, kind="volume", snapshot=s2)
    need(r["scheduled"] == s2 and r["replaced"] is None, "volume rollback scheduled", r)
    sn = snaps(c)
    need(sn["rollback_pending"] == s2 and [s for s in sn["snapshots"] if s["id"] == s2][0]["rollback"], "snapshots shows the pending rollback", sn["rollback_pending"])
    need(restore(c, kind="volume")["pending"] == s2, "status: pending")
    need(restore(c, kind="volume", cancel=True)["cancelled"] == s2 and snaps(c)["rollback_pending"] is None, "cancelled")
    restore(c, kind="volume", snapshot=s2)
    err, text = call(c, "snapshot_delete", {"snapshot": s2})
    need(err, "the snapshot a rollback names cannot be deleted", text)
    sh(c, f"echo late > {B}/vol/late", "a file written after the scheduling")
    vb = tree(c, B)
    c, to, k1 = reboot(c, "reboot 1")
    need(to == s2, "the rollback went to the scheduled snapshot", (to, s2))
    need(tree(c, B) == va, "after the reboot /data is as in the snapshot (the test folder compared)", tree(c, B))
    k = newest(c, "before-rollback")
    sn = snaps(c)
    need(k["id"] == k1 and sn["rollback_pending"] is None and sn["rollback_not_done"] is None, "the state before is a before-rollback snapshot, nothing pending", k)
    restore(c, kind="volume", snapshot=k1)
    c, to, _ = reboot(c, "reboot 2")
    need(to == k1 and tree(c, B) == vb, "undone at the next reboot: as before, the late file included")

    # 4. Everything as at the volume snapshot's time.
    at = [s for s in snaps(c)["snapshots"] if s["id"] == s2][0]
    d = restore(c, kind="system", at=at["created_s"], dry_run=True)
    need(d["snapshot"] == s2 and d["pin"] == 1 and d["running"] == 2 and d["next"] == 2 and "Dry run" in d["text"], "dry run: snapshot and generation 1", d)
    d2 = restore(c, kind="system", at=at["time"], dry_run=True)
    need(d2["pin"] == 1, "dry run with the local time string", d2)
    need("pin: none" in system_list(c) and snaps(c)["rollback_pending"] is None, "a dry run changed nothing")
    # A PIN of Georg's: a stray undo leaves it, a real undo puts it back.
    ok(c, "system_pin", {"n": 2, "timeout_ms": 120000})
    need("pin: generation 2" in system_list(c), "Georg's own PIN 2 is set")
    u = restore(c, kind="system", cancel=True)
    need(not u["cancelled"] and "pin: generation 2" in system_list(c) and snaps(c)["rollback_pending"] is None, "a stray undo changes nothing, the PIN stays", u)
    r = restore(c, kind="system", at=at["created_s"])
    need(r["snapshot"] == s2 and r["pin"] == 1, "system restore planned over Georg's PIN", r)
    need("pin: generation 1" in system_list(c) and snaps(c)["rollback_pending"] == s2, "PIN 1 and the rollback are set")
    u = restore(c, kind="system", cancel=True)
    need(u["cancelled"] and "pin: generation 2" in system_list(c) and snaps(c)["rollback_pending"] is None, "undone: Georg's PIN 2 back, no rollback", u)
    u = restore(c, kind="system", cancel=True)
    need(not u["cancelled"] and "pin: generation 2" in system_list(c), "a second undo changes nothing", u)
    ok(c, "system_pin", {"n": "off", "timeout_ms": 120000})
    need("pin: none" in system_list(c), "Georg's PIN removed by himself")
    r = restore(c, kind="system", at=at["created_s"])
    need(r["snapshot"] == s2 and r["pin"] == 1, "system restore planned", r)
    u = restore(c, kind="system", cancel=True)
    need(u["cancelled"] and "pin: none" in system_list(c) and snaps(c)["rollback_pending"] is None, "cancelled: no PIN, no rollback", u)
    r = restore(c, kind="system", at=at["created_s"])
    need(r["pin"] == 1, "system restore set again", r)
    pos = len(serial())
    err, t = call(c, "system_pin", {"n": 1, "now": True, "timeout_ms": 170000})
    need(not err and "Reloading into generation 1" in t, "system_pin 1 now: a Reload into generation 1", t)
    c.close()
    wait_for(r"kexec reload\.generation_switched reload=\d+ generation=1 reason=PINNED", pos, 180, "the Reload is generation 1 (PINNED)")
    wait_for(rf"bitsvol volume\.rollback_applied pool=data id=\d+ to={s2} kept_as=\d+", pos, 180, "the new kernel rolls /data back at its mount")
    c = connect(300)
    wait_data(c)
    l = system_list(c)
    need(running(l, 1, "PINNED"), "generation 1 runs", l)
    need(tree(c, B) == va, "/data is as at that time")
    k2 = newest(c, "before-rollback")["id"]
    restore(c, kind="volume", snapshot=k2)
    pos = len(serial())
    err, t = call(c, "system_pin", {"n": "off", "now": True, "timeout_ms": 170000})
    need(not err and "Reloading into generation 2" in t, "system_pin off now: back to generation 2", t)
    c.close()
    wait_for(r"kexec reload\.generation_switched reload=\d+ generation=2 reason=NEWEST_OK", pos, 180, "the Reload is generation 2")
    wait_for(rf"bitsvol volume\.rollback_applied pool=data id=\d+ to={k2} kept_as=\d+", pos, 180, "and /data goes back to before")
    c = connect(300)
    wait_data(c)
    l = system_list(c)
    need(running(l, 2, "NEWEST_OK") and "pin: none" in l, "undone: generation 2, no pin", l)
    need(tree(c, B) == vb, "/data as before the system restore")

    # 5. A clean last snapshot for the host check.
    ok(c, "snapshot_take", {"label": "vs3-final"})
    print("restore kinds through MCP: green")


main()
