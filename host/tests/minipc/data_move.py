#!/usr/bin/env python3
"""NV-2p: moves the stick's data.img contents (read-only at /stickdata) to the new /data on the SSD, once.

Usage: data_move.py <host:port> <out dir> [--tls DIR|auto] [--token-file F]   (token: BITSOS_TOKEN or the file)
Only with a valid window: /proc/bitsos/data-window armed, /data read-write, /stickdata read-only, no marker yet. The MCP
tool `data_move` starts `cp -a` as root (BusyBox: hard links, symlinks, modes, owners and times kept), then every entry
is compared in both trees (type, mode, owner, size of files, content hash, link target, hard-link groups, mtime in
seconds; BusyBox's cp keeps no time on a symlink itself, so that one is not compared), and the marker written. A second
run refuses. /stickdata is only read. A file with a time since the copy's start was written by a running service (the journal): named, not a difference.
Exit 0 = green. The two lists stay in <out dir>."""
import base64
import json
import os
import re
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import mp  # noqa: E402

OPTS = mp.options(sys.argv[1:], positional=2)
OUT = OPTS.rest[1]
DIR = "/run/bitsos/data-move"
LONG_S = 3600


def sh(cmd, ms=120_000):
    err, text, _ = mp.call(OPTS, "run", {"path": "/bin/sh", "args": ["-c", cmd], "timeout_ms": ms}, timeout=ms // 1000 + 60)
    return err, text


def need(cond, what, detail=""):
    if not cond:
        raise SystemExit(f"RED: {what}\n{detail}")
    print(f"ok: {what}", flush=True)


def read_all(path):
    out, off = b"", 0
    while True:
        err, text, _ = mp.call(OPTS, "read_file", {"path": path, "offset": off})
        need(not err, f"read {path}", text)
        j = json.loads(text)
        d = base64.b64decode(j.get("data", "")) if "data" in j else j.get("text", "").encode()
        out += d
        off += len(d)
        if not d or off >= j.get("size", off):
            return out


def manifest(name):
    """The lists the root copy left: {path: facts} with the inode beside them."""
    stats, sums = read_all(f"{DIR}/{name}.stat"), read_all(f"{DIR}/{name}.sums")
    os.makedirs(OUT, exist_ok=True)
    with open(os.path.join(OUT, f"{name}.stat"), "wb") as f:
        f.write(stats)
    hashes = {}
    for line in sums.decode("utf-8", "surrogateescape").splitlines():
        h, _, p = line.partition("  ")
        hashes[p] = h
    entries = {}
    for line in stats.decode("utf-8", "surrogateescape").splitlines():
        f = line.split("|", 7)
        if len(f) != 8:
            raise SystemExit(f"RED: a name this list cannot hold: {line!r}")
        mode, uid, gid, size, mtime, nlink, ino, quoted = f
        m = re.match(r"'(.*)' -> '(.*)'$", quoted)
        path, target = (m.group(1), m.group(2)) if m else (quoted.strip("'"), None)
        kind = int(mode, 16) & 0o170000
        facts = {"mode": mode, "uid": uid, "gid": gid, "target": target}
        if kind == 0o100000:
            facts.update(size=size, mtime=mtime, sha=hashes.get(path), nlink=nlink)
        elif kind == 0o040000:
            facts.update(mtime=mtime)
        entries[path] = (facts, ino)
    return entries


def links(entries):
    """Hard-link groups: inode -> the sorted paths, of files with more than one name."""
    g = {}
    for p, (f, ino) in entries.items():
        if int(f.get("nlink", "1")) > 1:
            g.setdefault(ino, []).append(p)
    return sorted(sorted(v) for v in g.values())


def read_text(path):
    err, text, _ = mp.call(OPTS, "read_file", {"path": path, "encoding": "text"})
    return None if err else json.loads(text).get("text", "")


def main():
    t = read_text("/proc/bitsos/data-window") or ""
    need("window=armed" in t, "the window is armed", t)
    end = time.time() + 120
    while time.time() < end:
        err, t = sh("grep -E ' /(data|stickdata) ' /proc/mounts")
        if "/stickdata" in t:
            break
        time.sleep(1)
    need(" /data ext4 rw" in t, "/data is read-write", t)
    need(" /stickdata ext4 ro" in t, "/stickdata is mounted read-only", t)
    # The copy runs as root in the kernel tool's fixed command (MCP's `run` is uid 1000); a marker refuses it.
    err, t, _ = mp.call(OPTS, "data_move", {})
    need(not err, "the root copy is started (the marker is the proof; a second run refuses)", t)
    end = time.time() + LONG_S
    log = ""
    while time.time() < end:
        log = read_text("/run/bitsos/data-move.log") or ""
        if "done status=" in log:
            break
        time.sleep(2)
    need("done status=0" in log, "copied", log)
    src_before = manifest("stick-before")
    src = manifest("stick-after")
    need(src == src_before, "/stickdata did not change")
    dst = manifest("data")
    start = int(re.search(r"start=(\d+)", log).group(1))
    # `cp -a` keeps the old times: what on /data has a time since the start, a running service wrote afterwards (the
    # journal does); a file gone from a folder written since the start was rotated away by one.
    fresh = lambda e: e is not None and int(e[0].get("mtime", "0")) >= start
    bad, live = [], []
    for p, (f, _) in src.items():
        if p == ".":
            continue
        have = dst.get(p)
        if have is None:
            (live if fresh(dst.get(os.path.dirname(p))) else bad).append(f"{p}: missing")
        elif have[0] != f:
            (live if fresh(have) else bad).append(f"{p}: {f} != {have[0]}")
    if live:
        print(f"note: {len(live)} entries changed by running services after the copy: " + ", ".join(x.split(":")[0] for x in live[:20]), flush=True)
    need(not bad, f"all {len(src)} entries are the same on /data", "\n".join(bad[:50]))
    sg, dg = links(src), links(dst)
    need(all(g in dg for g in sg), f"the {len(sg)} hard-link groups are kept", f"{sg}\n{dg}")
    err, t, _ = mp.call(OPTS, "data_move", {"compared": True})
    need(not err, "the marker is written", t)
    err, t = sh("sync; echo synced")
    need("synced" in t, "synced", t)
    print(f"GREEN: {len(src)} entries moved, /stickdata unchanged", flush=True)


if __name__ == "__main__":
    main()
