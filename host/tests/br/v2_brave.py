#!/usr/bin/env python3
"""BR-3 scenario client (10-start.sh): Brave on V2 in bitscomp (fbdev, QEMU), driven over MCP.

Usage: v2_brave.py <mcp host port> <token> <qmp socket> <run dir> <reference dir> <http port>
`brave --check`; `brave URL` (the launcher on the stick: uid 1000, the tree, profile as a tar on the stick) on
host/tests/br/page/render.html from the host's HTTP server (10.0.2.2 on QEMU's user network, reached by the name
br3.test; every other name is NOTFOUND, so nothing goes to the internet); the maximised window is compared with the
host's picture (tools/reference.sh, same tree, flags and compositor); a second `brave URL` hands index.html (BR-1's
subsystem checks: storage, IndexedDB, Cache Storage, fetch, Worker, WebAssembly, WebAudio) to the running Brave;
the window's close ends Brave cleanly and the profile is packed onto the stick; a new start takes it back. Brave's
log, faults and ENOSYS are checked. Times are printed, never judged (QEMU). Exit 0 = green."""
import base64
import json
import os
import re
import sys
import time

sys.dont_write_bytecode = True  # no __pycache__ in the tree
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "wy"))
REF_DIR, HTTP_PORT = sys.argv[5], int(sys.argv[6])
sys.argv = sys.argv[:5] + ["-"]
import v2_check as v  # noqa: E402
from v2_check import Job, Qmp, call, ctl, differing, need  # noqa: E402

# The launcher adds --force-dark-mode when settingsd's theme.mode is dark (c823a21); the host's reference has no
# settingsd and is light, so V2's Brave is pinned to light here (INT-W15: render.html differed 23.85 %).
v.ENV = dict(v.ENV, BRAVE_THEME="light")

HOST = "br3.test"
BASE = f"http://{HOST}:{HTTP_PORT}"
# The arguments after the launcher's own flags; tools/reference.sh gets the same with 127.0.0.1. --test-type keeps
# Chromium's "unsupported flag" bar away (it would show the address); the secure-origin flag gives index.html
# Cache Storage, which only a secure context has (127.0.0.1 is one, 10.0.2.2 is not).
def extra(ip):
    return ["--test-type", f"--host-resolver-rules=MAP {HOST} {ip}, MAP * ~NOTFOUND",
            f"--unsafely-treat-insecure-origin-as-secure={BASE}"]

# ENOSYS Brave is known to meet without its sandbox (plan/notes/br-BR-1.md, br-BR-3.md): rseq (AB-8 contract),
# landlock_create_ruleset (Chromium goes on without it), ptrace (crashpad's handler). The pkey calls answer since
# INT-W10 (BR-3 CR 4).
KNOWN_ENOSYS = {101: "ptrace", 334: "rseq", 444: "landlock_create_ruleset"}
# Differing pixels (any channel off by more than 24) between V2's and the host's picture: the page itself (below
# Brave's bars, PAGE box) must be the same; the whole screen may differ a little, since Brave's own bars draw their
# text in another size on V2 (an open point in plan/notes/br-BR-3.md). Measured 22.10.: page 0, screen 1.53 %.
PAGE = (0, 230, 1280, 800)
MAX_PAGE_DIFF = 0.002
MAX_DIFF = 0.03
BLOCKS = {"red": (0xcc, 0x44, 0x33), "green": (0x33, 0xaa, 0x66), "blue": (0x33, 0x66, 0xcc)}
LOG = "/tmp/wy/brave/brave.log"


def title(c):
    m = re.search(r'app_id=brave-browser title="([^"]*)"', ctl(c, "list"))
    return m.group(1) if m else None


def wait_title(c, want, what, secs, job=None):
    """Waits until Brave's window title has `want`; stops early when the launcher `job` has ended."""
    t0, last = time.time(), None
    while time.time() - t0 < secs:
        last = title(c)
        if last and want in last:
            print(f"ok: {what} (after {time.time() - t0:.1f} s, title {last!r})", flush=True)
            return time.time() - t0
        if job is not None and re.search(r"^pid \d+ ended: exit \d+", job.poll(), re.M):
            raise SystemExit(f"RED: {what}: the launcher ended first\n{job.text[-1500:]}")
        time.sleep(2)
    raise SystemExit(f"RED: {what}: the title is {last!r} after {secs} s\n{ctl(c, 'list')}")


def still(q, n):
    """A screendump once two in a row (a second apart) are the same apart from a few pixels."""
    w, h, prev = q.screendump(n)
    for i in range(30):
        time.sleep(1)
        _, _, px = q.screendump(n + 1 + i)
        if differing(prev, px, w, (0, 0, 32, 32)) < 64:
            return w, h, px
        prev = px
    raise SystemExit("RED: the screen never stood still for a second")


def count(px, rgb):
    t = bytes(rgb)
    return sum(1 for i in range(0, len(px), 3) if px[i:i + 3] == t)


def brave_focused(c):
    """The `ctl list` line of the focused window when it is one of Brave's (its main window or the window of its
    question), else None: keys go only where Brave gets them."""
    for l in ctl(c, "list").splitlines():
        if "focused=true" in l and "app_id=brave-browser" in l:
            return l
    return None


def close(c, job, what):
    """Closes Brave's window (`ctl close`, as its close button) and waits until the launcher job has ended; returns its
    whole output. Brave asks "Alle Tabs schliessen?" for a window with several tabs: Enter takes its default button
    ("Alle schliessen"), sent while one of Brave's windows has the focus, never to another program."""
    ctl(c, "close")
    t0, sent = time.time(), 0
    while not re.search(r"^pid \d+ ended: exit \d+", job.poll(), re.M):
        if time.time() - t0 > 120:
            raise SystemExit(f"RED: {what}\n{job.text[-3000:]}\n{ctl(c, 'list')}")
        if time.time() - t0 > 5 * (sent + 1) and sent < 3:
            focused = brave_focused(c)
            if focused:
                print(f"  Brave is still open; Enter to: {focused}", flush=True)
                call(c, "send_keys", {"keys": ["enter"]})
                sent += 1
        time.sleep(1)
    print(f"ok: {what} (after {time.time() - t0:.1f} s; Enter for Brave's question sent {sent} time(s))", flush=True)
    return job.text


def diagnose(c, job):
    """Why Brave did not come up: the launcher's words, the faults the kernel saw, Brave's FATAL lines."""
    print("launcher: " + " | ".join(re.findall(r"^brave: .*$", job.poll(), re.M)))
    klog = open(os.path.join(v.RUN, "serial.log"), "rb").read().decode("utf-8", "replace")
    for l in klog.splitlines():
        if re.search(r"proc fault .*program=(brave|chrome_crashpad)", l):
            print("kernel: " + l[:300])
    _, text, _ = call(c, "run", {"path": "/bin/sh", "args": ["-c", f"grep -E 'FATAL|launch failed' {LOG} | tail -5"],
                                 "env": v.ENV, "timeout_ms": 20000})
    print("Brave's log: " + text[-1500:])


def main():
    c = v.connect(300)
    q = Qmp(v.QMP)
    comp = Job(c, "/bin/bitscomp", ["--socket", v.SOCK, "--stats", "30"])
    comp.wait_for(r"backend=fbdev size=1280x800", "bitscomp started on fbdev at 1280x800")

    _, text, _ = call(c, "run", {"path": "/bin/sh", "args": ["-c", "brave --check; echo rc=$?"], "env": v.ENV,
                                 "timeout_ms": 60000})
    print(text)
    need("rc=0" in text and re.search(r"libraries: \d+ found, none missing", text) and "desktop: /tmp/wy/wayland-1" in text,
         "brave --check: tree, libraries, fonts, desktop and user are fine", text)

    # 1. The still page against the host's picture.
    t0 = time.time()
    b = Job(c, "/bin/sh", ["/boot/BITSOS2/bin/brave", f"{BASE}/render.html", "--", *extra("10.0.2.2")])
    try:
        wait_title(c, "BR-3 render ready", "Brave shows render.html (fonts loaded, two frames drawn)", 240, b)
    except SystemExit:
        diagnose(c, b)
        raise
    print(f"Brave: render.html ready {time.time() - t0:.1f} s after the start (QEMU, not judged)", flush=True)
    need("profile: new" in b.poll(), "a new profile (tar mode: none on the stick yet)", b.text[-2000:])
    print("with Brave: " + call(c, "ps", {})[1].splitlines()[0], flush=True)
    ctl(c, "max")
    w, h, px = still(q, 100)
    rw, rh, rpx = v.read_ppm(os.path.join(REF_DIR, "shot.ppm"))
    need((w, h) == (rw, rh), f"V2's screen {w}x{h} is the host's size {rw}x{rh}")
    for name, rgb in BLOCKS.items():
        n, r = count(px, rgb), count(rpx, rgb)
        need(r > 20000 and n >= r * 9 // 10, f"the {name} block is on the screen ({n} pixels, host {r})")
    d = differing(px, rpx, w, (0, 0, 32, 32))
    print(f"render.html: {d} of {w * h} pixels differ from the host's picture ({100 * d / (w * h):.2f} %)", flush=True)
    need(d <= MAX_DIFF * w * h, f"the screen on V2 looks as on the host (at most {100 * MAX_DIFF:.0f} % differ)")
    x0, y0, x1, y1 = PAGE
    area = (x1 - x0) * (y1 - y0)
    crop = lambda p: b"".join(p[(y * w + x0) * 3:(y * w + x1) * 3] for y in range(y0, y1))  # noqa: E731
    dp = differing(crop(px), crop(rpx), x1 - x0)
    print(f"render.html: {dp} of {area} page pixels differ ({100 * dp / area:.2f} %)", flush=True)
    need(dp <= MAX_PAGE_DIFF * area, f"the page itself is the host's (at most {100 * MAX_PAGE_DIFF:.1f} % differ)")

    # 2. BR-1's subsystem checks, handed to the running Brave by a second start (Chromium's singleton).
    _, text, _ = call(c, "run", {"path": "/bin/sh", "args": ["/boot/BITSOS2/bin/brave", f"{BASE}/index.html"],
                                 "env": v.ENV, "timeout_ms": 60000})
    need("goes to it" in text, "a second `brave URL` hands the URL to the running Brave", text)
    wait_title(c, "BR-1 done", "index.html ran all its checks", 180)
    t = title(c)
    need("all-ok" in t, "storage, IndexedDB, Cache Storage, fetch, Worker, WebAssembly and WebAudio work", t)

    # 3. The window's close ends Brave cleanly; the profile goes to the stick and comes back on the next start.
    text = close(c, b, "closing Brave's window (two tabs) ends Brave and the launcher")
    need(re.search(r"Brave ended \(code 0\)", text) and "profile saved on the stick" in text,
         "Brave ended with code 0 and the profile was packed onto the stick", text[-2000:])
    print(re.search(r"brave: Brave ended[^\n]*", text).group(0), flush=True)
    end = time.time() + 20
    while title(c) is not None:
        need(time.time() < end, "Brave's window is gone once Brave ended", ctl(c, "list"))
        time.sleep(1)
    t0 = time.time()
    b2 = Job(c, "/bin/sh", ["/boot/BITSOS2/bin/brave", f"{BASE}/render.html", "--", *extra("10.0.2.2")])
    wait_title(c, "BR-3 render ready", "a second start shows render.html again", 240, b2)
    print(f"Brave: the second start was ready {time.time() - t0:.1f} s after it (the tree is in the page cache now)", flush=True)
    need("profile: taken from the stick" in b2.poll(), "the second start takes the profile from the stick", b2.text[-2000:])
    text = close(c, b2, "the second Brave ends by its window's close")
    need("profile saved on the stick" in text, "the profile is saved again", text[-2000:])

    # 4. Brave's own log (MCP read_file: JSON with base64 data, read on until eof), the kernel's events (the serial
    # log: loglevel=warn,syscall=info; it names ENOSYS and ENOTTY only, not other errnos: those are in
    # user/abi-tests/c/br3probe.c).
    blog, off = b"", 0
    while True:
        err, text, _ = call(c, "read_file", {"path": LOG, "offset": off})
        need(not err, f"read_file {LOG}", text)
        j = json.loads(text)
        blog += base64.b64decode(j["data"])
        off += j["length"]
        if j["eof"] or not j["length"]:
            break
    blog = blog.decode("utf-8", "replace")
    open(os.path.join(v.RUN, "brave.log"), "w").write(blog)
    lines = [re.sub(r"^\[[^]]*\]", "", l) for l in blog.splitlines() if l.startswith("[")]
    print(f"Brave's log of the second start: {len(blog)} bytes; ERROR/FATAL lines, once each:")
    for l in dict.fromkeys(lines):
        print("  " + l[:200])
    need(lines, "Brave's log has its lines (V2 always gives the drmGetDevices2 one)", blog[-2000:])
    fatal = [l for l in lines if ":FATAL:" in l]
    need(not fatal, "no FATAL line in Brave's log", "\n".join(fatal))
    klog = open(os.path.join(v.RUN, "serial.log"), "rb").read().decode("utf-8", "replace")
    print("kernel events naming Brave (once each):")
    for l in dict.fromkeys(re.sub(r"^\[[^]]*\]\s*", "", l) for l in klog.splitlines()
                           if re.search(r"program=(brave|chrome_crashpad)", l)):
        print("  " + l[:240])
    faults = [l for l in klog.splitlines() if re.search(r"proc fault .*program=(brave|chrome_crashpad)", l)]
    need(not faults, "no CPU fault in Brave or its helpers", "\n".join(faults))
    # Every program's line counts: the kernel names only the first program that met a number, so a number Brave
    # meets after bitscomp, sh or `brave --check` would carry their name.
    seen = {}
    for m in re.finditer(r"syscall enosys nr=(\d+) name=\S+ program=(\S+)", klog):
        seen.setdefault(int(m.group(1)), m.group(2))
    print("ENOSYS since boot (first program): " + ", ".join(f"{n} {KNOWN_ENOSYS.get(n, '?')} ({p})" for n, p in sorted(seen.items())))
    if not any(p in ("brave", "chrome_crashpad_handler") for p in seen.values()):
        print("note: no ENOSYS line names Brave (all its numbers were met first by others)", flush=True)
    new = sorted(n for n in seen if n not in KNOWN_ENOSYS)
    need(not new, f"no ENOSYS beyond the known {sorted(KNOWN_ENOSYS.values())}",
         ", ".join(f"{n} ({seen[n]})" for n in new))
    ctl(c, "quit")


if __name__ == "__main__":
    main()
