#!/usr/bin/env python3
"""EINS-C through MCP (`run` is uid 1000, busybox): the assistant tree's trash on /data (60-papierkorb.sh).

Usage: papierkorb_check.py <mcp host port> <token> <run dir>
"""
import http.client
import json
import os
import re
import sys
import time

PORT, TOKEN, RUN_DIR = int(sys.argv[1]), sys.argv[2], sys.argv[3]
HOME = "/data/home/1000/e1"
TRASH = "/data/.papierkorb"
next_id = 0


def need(cond, what, detail=""):
    if not cond:
        raise SystemExit(f"RED: {what}{': ' + str(detail)[-3000:] if detail else ''}")
    print(f"ok: {what}", flush=True)


def rpc(c, method, params=None):
    global next_id
    next_id += 1
    body = {"jsonrpc": "2.0", "id": next_id, "method": method, "params": params or {}}
    c.request("POST", "/mcp", json.dumps(body), {"Authorization": f"Bearer {TOKEN}", "Content-Type": "application/json"})
    r = c.getresponse()
    data = r.read()
    if r.status != 200:
        raise ValueError(f"{method}: HTTP {r.status} {data[:200]}")
    return json.loads(data)


def connect(wait_s):
    end = time.time() + wait_s
    while True:
        try:
            c = http.client.HTTPConnection("127.0.0.1", PORT, timeout=300)
            rpc(c, "initialize", {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "eins-c", "version": "1"}})
            return c
        except (OSError, http.client.HTTPException, ValueError) as e:
            if time.time() > end:
                raise SystemExit(f"RED: no MCP answer on 127.0.0.1:{PORT}: {e}")
            time.sleep(1)


def text_of(j):
    return "\n".join(i.get("text", "") for i in j.get("result", {}).get("content", []) if i.get("type") == "text")


def sh(c, script, timeout_ms=120000):
    """(exit code, stdout, stderr) of `sh -c script` as uid 1000."""
    j = rpc(c, "tools/call", {"name": "run", "arguments": {"path": "/bin/sh", "args": ["-c", script], "timeout_ms": timeout_ms}})
    text = text_of(j)
    m = re.match(r"pid \d+ ended: exit (\d+)\n--- stdout \(\d+ bytes\) ---\n(.*)\n?--- stderr \(\d+ bytes\) ---\n(.*)", text, re.S)
    if j.get("result", {}).get("isError") or not m:
        raise SystemExit(f"RED: sh -c {script!r}: {text[:1500]}")
    return int(m.group(1)), m.group(2), m.group(3)


def ok(c, script):
    code, out, err = sh(c, script)
    need(code == 0, f"sh: {script[:90]}", out + err)
    return out


def entries(c):
    code, out, _ = sh(c, f"ls {TRASH} 2>/dev/null")
    return sorted(out.split()) if code == 0 else []


def serial():
    return open(os.path.join(RUN_DIR, "serial.log"), errors="replace").read()


SETUP = f"""set -e
rm -rf {HOME}
mkdir -p {HOME}/proj/sub/deep {HOME}/proj/leer
for i in 1 2 3; do echo "datei $i" > {HOME}/proj/f$i.txt; done
echo tief > {HOME}/proj/sub/deep/t.txt
dd if=/dev/urandom of={HOME}/proj/sub/blob bs=1024 count=300 2>/dev/null
ln -s f1.txt {HOME}/proj/link
"""
LIST = "find proj | sort; find proj -type f -exec sha256sum {} + | sort; ls -l proj/link | sed 's/.* -> //'"
ENTER = "echo enter > /proc/self/bitsos_tree"


def main():
    c = connect(400)
    end = time.time() + 300
    while time.time() < end and sh(c, "grep -q ' /data ext4 ' /proc/mounts")[0] != 0:
        time.sleep(2)
    need(sh(c, "grep -q ' /data ext4 ' /proc/mounts")[0] == 0, "/data is mounted (ext4)")
    ok(c, SETUP)
    before = ok(c, f"cd {HOME} && {LIST}")
    need("proj/sub/blob" in before and "proj/leer" in before, "the folder to remove is there", before)
    start = entries(c)
    need(sh(c, "cat /proc/self/bitsos_tree")[1] == "none\n", "a process outside the tree reads none")

    # inside the tree: rm -rf
    said = ok(c, f"{ENTER} && echo chat scn-1 > /proc/self/bitsos_tree && cat /proc/self/bitsos_tree && rm -rf {HOME}/proj && echo removed")
    need(re.search(r"^tree=\d+ root=\d+ since=\d+ chat=scn-1$", said, re.M) and said.endswith("removed\n"), "the shell entered a tree and rm -rf ran in it", said)
    need(sh(c, f"test -e {HOME}/proj")[0] != 0, "the folder is gone from its place")
    new = [e for e in entries(c) if e not in start]
    need(len(new) == 1, "rm -rf made exactly one trash entry", new)
    eid = new[0]
    rec = json.loads(ok(c, f"cat {TRASH}/{eid}/record.json"))
    need(rec["path"] == f"{HOME}/proj" and rec["fs"] == "/data" and rec["command"] == "rm" and rec["chat"] == "scn-1" and rec["what"] in ("unlink", "rmdir"), "the record names the folder, rm and the chat", rec)
    held = ok(c, f"cd {TRASH}/{eid}/files/home/1000/e1 && {LIST}")
    need(held == before, "the entry holds the folder as it was (names, contents, the link, the empty folder)", held)
    mode = ok(c, f"stat -c '%a %u' {TRASH}")
    need(mode.strip() == "700 1000", "the trash is Georg's, 0700", mode)

    # the tree may neither empty the trash nor remove from it; tmpfs is not kept
    code, out, err = sh(c, f"{ENTER} && printf 'empty\\n/data\\nall\\n' > /dev/bitsos/papierkorb")
    need(code != 0 and len(entries(c)) == len(start) + 1, "the tree's empty is refused (EPERM) and the entry stays", out + err)
    code, out, err = sh(c, f"{ENTER} && rm -rf {TRASH}/{eid}")
    need(code != 0 and sh(c, f"test -d {TRASH}/{eid}")[0] == 0, "the tree cannot remove the entry", out + err)
    ok(c, "rm -rf /tmp/e1t && mkdir /tmp/e1t && echo x > /tmp/e1t/f")
    ok(c, f"{ENTER} && rm -rf /tmp/e1t")
    need(sh(c, "test -e /tmp/e1t")[0] != 0 and [e for e in entries(c) if e not in start] == [eid] and sh(c, "test -e /tmp/.papierkorb || test -e /.papierkorb")[0] != 0, "on tmpfs the tree removes for real")
    code, out, err = sh(c, f"{ENTER} && echo x >> /boot/BITSOS2/CONFIG.TXT")
    need(code != 0, "the tree cannot write CONFIG.TXT", out + err)

    # back through the control node, outside the tree
    said = ok(c, f"exec 3<>/dev/bitsos/papierkorb && printf 'restore\\n/data\\n%s\\n' {eid} >&3 && cat <&3")
    need(said == "moved=1\n", "restore answered moved=1", said)
    after = ok(c, f"cd {HOME} && {LIST}")
    need(after == before, "everything came back byte for byte", after)
    need(entries(c) == start, "the restored entry is gone from the trash")

    # outside the tree rm -rf deletes for real
    ok(c, f"rm -rf {HOME}/proj")
    need(sh(c, f"test -e {HOME}/proj")[0] != 0 and entries(c) == start, "outside the tree rm -rf deleted for real and kept nothing")

    # Georg empties an entry
    ok(c, f"mkdir -p {HOME}/w && echo w > {HOME}/w/f")
    ok(c, f"{ENTER} && rm -rf {HOME}/w")
    new = [e for e in entries(c) if e not in start]
    need(len(new) == 1, "a second entry", new)
    said = ok(c, f"exec 3<>/dev/bitsos/papierkorb && printf 'empty\\n/data\\n%s\\n' {new[0]} >&3 && cat <&3")
    need(said == "emptied=1\n" and entries(c) == start, "Georg's empty removed the entry", said)

    # INT-EINS1: `bits papierkorb` reads the same entries and restores through the node; the tree never empties.
    ok(c, f"mkdir -p {HOME}/b/innen && echo bits > {HOME}/b/innen/f")
    ok(c, f"{ENTER} && rm -rf {HOME}/b")
    new = [e for e in entries(c) if e not in start]
    lst = json.loads(ok(c, "bits --json papierkorb liste"))
    row = [r for r in lst.get("entries", []) if new and r["id"] == new[0]]
    need(len(new) == 1 and row and row[0]["mount"] == "/data" and row[0]["original"] == f"{HOME}/b" and row[0]["kind"] == "folder" and row[0]["bytes"] == 5, "bits lists the kernel's entry", lst)
    code, out, err = sh(c, f"{ENTER} && bits --json papierkorb leeren {new[0]}")
    need(code == 5 and len(entries(c)) == len(start) + 1, "bits from the tree does not empty", out + err)
    said = json.loads(ok(c, f"bits --json papierkorb zurueck {new[0]}"))
    need(said.get("ok") and said.get("restored") == f"{HOME}/b" and ok(c, f"cat {HOME}/b/innen/f") == "bits\n" and entries(c) == start, "bits put it back through the node", said)
    ok(c, f"rm -rf {HOME}")

    # The serial port lags behind the calls: wait for the journal's lines.
    want = ("vfs trash.entry", "policy tree.entered", "vfs trash.empty_refused", "vfs tree.guarded", "vfs trash.restored", "vfs trash.emptied")
    end = time.time() + 120
    while time.time() < end and not all(w in serial() for w in want):
        time.sleep(2)
    log = serial()
    need(all(w in log for w in want), "the journal names the entries, the tree, the refusals, the restore and the emptying", [w for w in want if w not in log])
    j = rpc(c, "tools/call", {"name": "ktest", "arguments": {"filter": "fs.trash.scenario", "timeout_ms": 120000}})
    t = text_of(j)
    need(not j.get("result", {}).get("isError") and re.search(r"passed\W+1", t) and re.search(r"failed\W+0", t), "ktest fs.trash.scenario passes on this boot's /data", t[:800])


if __name__ == "__main__":
    main()
