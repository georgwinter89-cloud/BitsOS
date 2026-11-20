#!/usr/bin/env python3
"""UPD-4 real-machine check: after Georg installed a version with parts (the page's "Aktualisieren", or
`bitsupdate install` at the console) and the machine restarted into it, the running version runs with its own parts.
Read only over MCP; the update node itself refuses MCP on purpose, so this script never installs anything.

Usage: update_check.py <host:port> <release dir> [--tls DIR|auto] [--token-file F] [--back]
  <release dir>: the version's folder `cargo xtask update-release` wrote (its MANIFEST2 is the reference).
  1. `system_list` lists the release's number (the desk reads which generation runs from its text).
  2. /boot/BITSOS2/SYS/<n>/FILES names exactly the release's parts (bin and every tree, with their hashes).
  3. /proc/self/mountinfo (as MCP's run sees it): each part bound at /boot/BITSOS2/<name> from
     /BITSOS2/SYS/STORE/<name>.<16 hex>.
  4. Every file of \\BITSOS2\\bin at /boot/BITSOS2/bin has the release's size and SHA-256 (sha256sum on the machine).
  5. Each tree's PKGLIST.TXT at /boot/BITSOS2/<tree> is the one in the release's piece (its first line's file has
     its SHA-256 on the machine).
  With --back (after "Zurück zur vorherigen Version" and a restart into a slot without FILES): nothing below
  /boot/BITSOS2 is bound, and /boot/BITSOS2/bin/bitscomp is not the release's.
Exit 0 = green."""
import os
import re
import subprocess
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import mp  # noqa: E402

OPTS = mp.options(sys.argv[1:], positional=2, flags=("--back",))
REL = OPTS.rest[1]
BACK = OPTS.flags["--back"]


def need(cond, what, detail=""):
    if not cond:
        raise SystemExit(f"RED: {what}\n{detail}")
    print(f"ok: {what}", flush=True)


def sh(cmd, ms=120000):
    err, text, _ = mp.call(OPTS, "run", {"path": "/bin/sh", "args": ["-c", cmd], "timeout_ms": ms})
    need(not err, f"run {cmd[:70]}", text)
    # The program's stdout only: the answer starts with "pid N ended" and frames stdout and stderr.
    if "\n--- stdout (" not in text:
        return ""
    rest = text.split("\n--- stdout (", 1)[1]
    return rest.split(") ---", 1)[1].removeprefix("\n").split("\n--- stderr (", 1)[0]


def manifest2():
    with open(os.path.join(REL, "MANIFEST2")) as f:
        lines = f.read().splitlines()
    version = int(next(l for l in lines if l.startswith("version ")).split()[1])
    bins = [l.split()[1:] for l in lines if l.startswith("bin ")]
    trees = [l.split()[1:3] for l in lines if l.startswith("tree ")]
    return version, bins, trees


def bin_hash(bins):
    """parts_rules' content hash of the bin part: SHA-256 over `<sha> <size> <name>\\n`."""
    import hashlib
    return hashlib.sha256("".join(f"{sha} {size} {name}\n" for name, size, sha in bins).encode()).hexdigest()


def main():
    version, bins, trees = manifest2()
    parts = sorted([("bin", bin_hash(bins))] + [(n, h) for n, h in trees])
    err, text, _ = mp.call(OPTS, "system_list", {})
    need(not err, "system_list answers", text)
    print(text[:600])
    mounts = sh("cat /proc/self/mountinfo")
    # FAT keeps a folder's own spelling (an old medium's FIRMWARE): the bind lands there, names compare caseless.
    bound = [(s, "/boot/BITSOS2/" + m.lower()) for s, m in re.findall(r" (/BITSOS2/SYS/STORE/[a-z0-9_-]+\.[0-9a-f]{16}) /boot/BITSOS2/([A-Za-z0-9_-]+) ", mounts)]
    if BACK:
        need(not bound, "the previous version binds nothing", mounts)
        got = sh("sha256sum /boot/BITSOS2/bin/bitscomp")
        gpu = next((b for b in bins if b[0] == "bitscomp"), None)
        need(gpu is not None and gpu[2] not in got, "its bitscomp is not the newer version's", got)
        return
    need(re.search(rf"\b{version}\b", text) is not None, f"generation {version} is listed", text)
    files = sh(f"cat /boot/BITSOS2/SYS/{version}/FILES")
    want = "bitsos-files 1\n" + "".join(f"part {n} {h}\n" for n, h in parts)
    need(files.strip() == want.strip(), f"SYS\\{version}\\FILES names the release's {len(parts)} parts", files)
    for n, h in parts:
        need((f"/BITSOS2/SYS/STORE/{n}.{h[:16]}", f"/boot/BITSOS2/{n}") in bound, f"{n} is bound at /boot/BITSOS2/{n}", mounts)
    sums = sh("cd /boot/BITSOS2/bin && sha256sum " + " ".join(f"'{b[0]}'" for b in bins), 600000)
    for name, size, sha in bins:
        need(re.search(rf"(?m)^{sha}\s+{re.escape(name)}$", sums) is not None, f"bin/{name} is the release's", sums)
    for name, h in trees:
        piece = os.path.join(REL, f"tree.{name}.{h[:16]}.1.tar.zst")
        listing = subprocess.run(["sh", "-c", f"zstd -dc '{piece}' | tar -xOf - PKGLIST.TXT 2>/dev/null"], capture_output=True, text=True).stdout
        line = next((l for l in listing.splitlines() if l.startswith("stick ")), None)
        if line is None:
            print(f"note: tree {name} has no PKGLIST.TXT in its first piece (an extra tree); its bind was checked")
            continue
        path, sha = line.split()[1], line.split()[3]
        got = sh(f"sha256sum '{path}'")
        need(sha in got, f"tree {name}: {path} is the release's", got)
    print("update check: green", flush=True)


main()
