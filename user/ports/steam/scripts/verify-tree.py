#!/usr/bin/env python3
"""GM-2a: proves on the build host what can be proven of the steam tree, without running anything of it.
Usage: verify-tree.py <stage> <libset.txt> <empty work dir> [--report FILE]
  - every package of the pinned manifest is there with the manifest's size and sha2; no link anywhere; the bootstrap,
    the provider and the realpath helper are there
  - unpacks a copy of the client into <work dir>, reads every 64-bit ELF file of it with readelf (never ld.so), and
    follows each NEEDED: host programs (the 64-bit bootstrapper, pressure-vessel) against their RUNPATH and the image's
    library set, the client (which runs in the container) against its RUNPATH, its own folders, the runtime and the
    provider
  - lists the 32-bit ELF files: V2 runs none (AB-10a/b), so each is a known gap, not a missing library
Exit 0 = everything found. Prints one line per miss, then a summary."""
import gzip
import hashlib
import os
import re
import shutil
import stat
import subprocess
import sys

CLIENT_DIRS = ("steamrt64", "ubuntu12_64", "linux64")
PV = "steamrt64/pv-runtime/steam-runtime-steamrt/pressure-vessel/"
# pressure-vessel's helpers that run inside the container: drawing and display checks (steam-runtime-system-info).
IN_CONTAINER = re.compile(r"x86_64-linux-gnu-(check-(gl|va-api|vdpau|vulkan|xdg-portal)|is-x-server-xwayland|wflinfo)$")


def sh(*cmd):
    return subprocess.run(cmd, capture_output=True, text=True, errors="replace", check=False)


def sha256(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for b in iter(lambda: f.read(1 << 20), b""):
            h.update(b)
    return h.hexdigest()


def packages(stage):
    manifests = [f for f in os.listdir(f"{stage}/package") if f.endswith(".manifest")]
    if len(manifests) != 1:
        raise SystemExit(f"RED: package/ holds {manifests}, want one manifest")
    here = os.path.dirname(os.path.abspath(__file__))
    r = sh(sys.executable, "-I", f"{here}/manifest.py", "packages", f"{stage}/package/{manifests[0]}")
    if r.returncode:
        raise SystemExit(f"RED: {r.stderr}")
    return [l.split() for l in r.stdout.splitlines()]


def is_elf(path):
    try:
        with open(path, "rb") as f:
            return f.read(4) == b"\x7fELF"
    except OSError:
        return False


def read_elf(path):
    """-> (bits, needed, search dirs, interp)"""
    with open(path, "rb") as f:
        bits = 64 if f.read(5)[4] == 2 else 32
    d = sh("readelf", "-dW", path).stdout
    needed = re.findall(r"\(NEEDED\)\s+Shared library: \[([^\]]+)\]", d)
    paths = []
    for m in re.findall(r"\((?:RUNPATH|RPATH)\)\s+Library r(?:un)?path: \[([^\]]+)\]", d):
        for p in m.split(":"):
            paths.append(p.replace("${ORIGIN}", "$ORIGIN").replace("$ORIGIN", os.path.dirname(path)))
    return bits, needed, paths


def mtree_links(platform):
    """The runtime's zip holds no links: pressure-vessel makes them from usr-mtree.txt.gz. <dir below files>/<name> -> target."""
    links = {}
    with gzip.open(f"{platform}/usr-mtree.txt.gz", "rt", errors="replace") as f:
        for line in f:
            m = re.match(r"\./(\S+) type=link link=(\S+)", line)
            if m:
                links[m.group(1)] = m.group(2)
    return links


def main():
    argv = sys.argv[1:]
    report = None
    if "--report" in argv:
        i = argv.index("--report")
        report = argv[i + 1]
        del argv[i : i + 2]
    stage, libset, work = argv
    if os.path.exists(work) and os.listdir(work):
        raise SystemExit(f"RED: {work} is not empty")
    os.makedirs(work, exist_ok=True)
    root = os.path.join(work, "client")
    os.makedirs(root)
    pkgs = packages(stage)
    fails = []
    for p, ds, fs in os.walk(stage):
        for n in ds + fs:
            if os.path.islink(os.path.join(p, n)):
                fails.append(f"a link in the tree: {os.path.join(p, n)}")
    for f in ("bootstraplinux_ubuntu12_32.tar.xz", "gfx.tar.gz", "bin/realpath", "VERSION.txt", "LICENSES/steam-launcher.copyright", "dbus/dbus-daemon", "dbus/session.conf"):
        if not os.path.isfile(f"{stage}/{f}"):
            fails.append(f"{f} is not in the tree")
    if not os.stat(f"{stage}/bin/realpath").st_mode & stat.S_IXUSR:
        fails.append("bin/realpath is not executable")
    for name, file, size, sha in pkgs:
        f = f"{stage}/package/{file}"
        if not os.path.isfile(f):
            fails.append(f"package {name}: {file} is not in the tree")
        elif os.path.getsize(f) != int(size) or sha256(f) != sha:
            fails.append(f"package {name}: {file} is not the manifest's file")
        else:
            r = sh("bsdtar", "-xf", f, "-C", root)
            if r.returncode:
                fails.append(f"package {name}: {r.stderr.strip()[:200]}")
    if fails:
        raise SystemExit("RED:\n" + "\n".join(fails))
    if not os.path.isfile(f"{root}/steamrt64/steam"):
        raise SystemExit("RED: no steamrt64/steam in any package: the 64-bit client is gone from this manifest")
    gfxroot = os.path.join(work, "provider")
    os.makedirs(gfxroot)
    r = sh("tar", "-xzf", f"{stage}/gfx.tar.gz", "-C", gfxroot)
    if r.returncode:
        raise SystemExit(f"RED: gfx.tar.gz: {r.stderr[:200]}")
    gfx = f"{gfxroot}/gfx/usr/lib"
    if not os.path.isfile(f"{gfx}/libc.so.6"):
        raise SystemExit("RED: the provider has no usr/lib/libc.so.6")
    image = set(re.findall(r"^(\S+)\s+always\b", open(libset).read(), re.M))
    platform = [d for d in os.listdir(f"{root}/steamrt64/pv-runtime/steam-runtime-steamrt") if d.startswith("steamrt3c_platform_")]
    if len(platform) != 1:
        raise SystemExit(f"RED: the runtime has {platform}, want one platform")
    plat = f"{root}/steamrt64/pv-runtime/steam-runtime-steamrt/{platform[0]}"
    files = f"{plat}/files"
    links = mtree_links(plat)
    runtime = [f"{files}/lib/x86_64-linux-gnu", f"{files}/lib", gfx]
    pv_lib = f"{root}/{PV}lib/x86_64-linux-gnu/steam-runtime-tools-0"
    own = [f"{root}/{d}" for d in CLIENT_DIRS]

    seen, stack, miss, i386, roots = set(), [], [], [], {"host": 0, "client": 0}
    for top in CLIENT_DIRS:
        for p, ds, fs in os.walk(f"{root}/{top}"):
            if "/steamrt3c_platform_" in p:
                ds[:] = []
                continue
            for n in fs:
                f = os.path.join(p, n)
                rel = os.path.relpath(f, root)
                if os.path.islink(f) or not is_elf(f) or rel.startswith(f"{PV}lib/"):  # those are followed from their programs
                    continue
                if read_elf(f)[0] == 32:
                    i386.append(rel)
                    continue
                kind = "host" if rel == "steamrt64/steam" or (rel.startswith(PV) and not IN_CONTAINER.search(rel)) else "client"
                roots[kind] += 1
                stack.append((f, kind))
    followed = 0
    while stack:
        f, kind = stack.pop()
        key = (os.path.realpath(f), kind)
        if key in seen:
            continue
        seen.add(key)
        _, needed, paths = read_elf(f)
        for so in needed:
            followed += 1
            # The host programs' RUNPATH names pv_lib, and their libraries are loaded through it.
            search = paths + [files + d[4:] for d in paths if d.startswith("/usr/")]  # in the container /usr is the runtime's
            search += [os.path.dirname(f)] + own + runtime if kind == "client" else [pv_lib]
            hit = next((os.path.join(d, so) for d in search if os.path.exists(os.path.join(d, so))), None)
            if not hit and kind == "client":
                for d in ("lib/x86_64-linux-gnu", "lib"):
                    if f"{d}/{so}" in links:
                        hit = f"{files}/{d}/{links[f'{d}/{so}']}"
                        break
            if hit:
                if is_elf(hit):
                    stack.append((hit, kind))
                continue
            if kind == "host" and so in image:
                continue
            miss.append(f"{os.path.relpath(f, work)} ({kind}): {so}")
    text = [f"missing: {m}" for m in sorted(set(miss))]
    text += [f"i386, not run on V2: {x}" for x in sorted(i386)]
    text.append(
        f"elf host={roots['host']} client={roots['client']} needed-followed={followed} missing={len(set(miss))} i386={len(i386)} packages={len(pkgs)}"
    )
    out = "\n".join(text)
    print(out)
    if report:
        with open(report, "w") as f:
            f.write(out + "\n")
    shutil.rmtree(work)
    sys.exit(1 if miss else 0)


if __name__ == "__main__":
    main()
