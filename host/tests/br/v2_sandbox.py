#!/usr/bin/env python3
"""BR-6 scenario client (50-sandbox.sh): Brave on V2 in its own sandbox, driven over MCP.

Usage: v2_sandbox.py <mcp host port> <token> <qmp socket> <run dir> <reference dir> <http port>
The launcher starts Brave on render.html (from the host, as BR-3) and must say `sandbox: on`; a new tab gets
chrome://sandbox/ (typed: Chromium opens no chrome:// page from its command line), whose rows (green/yellow/red) and
verdict line are compared with the host's picture of the same page (reference dir, CachyOS's sandbox); every
renderer has seccomp mode 2 in /proc/<pid>/status. chrome://crash/ in a third tab: one renderer ends by SIGSEGV,
Brave and its first tab live on, BR-1's checks page loads in a new tab. chrome://gpucrash: Brave lives on and a page
loads again. Brave's window closes cleanly. Faults and FATAL lines only from the processes crashed on purpose, no
seccomp kill, no ENOSYS beyond the known. Times are printed, never judged (QEMU). Exit 0 = green."""
import base64
import json
import os
import re
import subprocess
import sys
import time
from collections import Counter

sys.dont_write_bytecode = True
HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import dekeys  # noqa: E402
import v2_brave as vb  # noqa: E402  (takes argv 5 and 6, as this script)
from v2_check import Job, Qmp, call, ctl, differing, need  # noqa: E402

v = vb.v
# chrome://sandbox's rows as Brave 1.8x shows them (the host's picture, 26.10.); only their order is used here.
LABELS = ["Layer 1 Sandbox", "PID namespaces", "Network namespaces", "Seccomp-BPF sandbox",
          "Seccomp-BPF sandbox supports TSYNC", "Ptrace Protection with Yama LSM (Broker)",
          "Ptrace Protection with Yama LSM (Non-broker)"]
TOP, RIGHT = 240, 900  # below Brave's bars and its info bar; the table is narrower than this
PS = r"""for p in /proc/[0-9]*; do c=$(tr '\0' ' ' <$p/cmdline 2>/dev/null); case "$c" in
*/brave*) t=$(echo "$c" | sed -n 's/.*--type=\([a-z-]*\).*/\1/p'); echo "${p#/proc/} ${t:-browser}";; esac; done"""


def kind(r, g, b):
    if g > r + 40 and g > b + 40:
        return "green"
    if r > 200 and g > 200 and b < 150:
        return "yellow"
    if r > g + 60 and r > b + 60:
        return "red"
    return None


def rows(px, w, h):
    """The table's rows top to bottom as [first y, last y, colour]: lines whose saturated pixels are mostly one of
    the page's three status colours (text and the borders between rows break them)."""
    out = []
    for y in range(TOP, h):
        n = Counter(k for k in (kind(*px[(y * w + x) * 3:(y * w + x) * 3 + 3]) for x in range(0, RIGHT, 2)) if k)
        if not n or n.most_common(1)[0][1] < 60:
            continue
        k = n.most_common(1)[0][0]
        if out and out[-1][2] == k and out[-1][1] >= y - 1:
            out[-1][1] = y
        else:
            out.append([y, y, k])
    return [r for r in out if r[1] - r[0] >= 12]


def verdict(px, w, rs):
    """The line under the table ("You are adequately sandboxed." or not), as bytes of a fixed-size box."""
    y0 = rs[-1][1] + 4 if rs else TOP
    return b"".join(px[(y * w) * 3:(y * w + RIGHT) * 3] for y in range(y0, y0 + 36))


def save_png(px, w, h, path):
    ppm = path + ".ppm"
    with open(ppm, "wb") as f:
        f.write(b"P6\n%d %d\n255\n" % (w, h) + px)
    subprocess.run(["magick", ppm, path], check=False, capture_output=True)


def procs(c):
    _, text, _ = call(c, "run", {"path": "/bin/sh", "args": ["-c", PS], "env": v.ENV, "timeout_ms": 30000})
    out = {}
    for l in text.splitlines():
        m = re.match(r"^(\d+) ([a-z-]+)$", l.strip())
        if m:
            out[int(m.group(1))] = m.group(2)
    return out


def status(c, pid):
    _, text, _ = call(c, "run", {"path": "/bin/sh", "args": ["-c", f"grep -E '^(Uid|NSpid|NoNewPrivs|Seccomp|Seccomp_filters):' /proc/{pid}/status"],
                                 "env": v.ENV, "timeout_ms": 20000})
    return dict(re.findall(r"^(\w+):\s*(.*)$", text, re.M))


# Every NSpid entry seen of a Brave process, by its global pid (the first entry). Chromium's log prefix names the
# pid outside the namespace (host, sandbox on: "[2757790:1:...] Intentionally crashing" from a renderer whose
# tid there is 1), but the FATAL filter takes all levels, so it holds whichever pid V2 hands a renderer.
ALIASES = {}


def nspids(c, pids):
    """Reads NSpid of `pids` in one call; records every level in ALIASES; returns {pid: [levels]}."""
    if not pids:
        return {}
    _, text, _ = call(c, "run", {"path": "/bin/sh", "args": ["-c", "for p in " + " ".join(map(str, pids)) +
                                 "; do echo \"$p $(grep '^NSpid:' /proc/$p/status 2>/dev/null)\"; done"],
                                 "env": v.ENV, "timeout_ms": 20000})
    out = {}
    for m in re.finditer(r"^(\d+) NSpid:\s*([\d\s]+?)\s*$", text, re.M):
        lv = m.group(2).split()
        out[int(m.group(1))] = lv
        ALIASES.setdefault(int(m.group(1)), set()).update(lv)
    return out


def keys(c, words):
    """Keys into Brave's window (bitscomp's `ctl key`, the codes the host's reference got), only while it has the
    focus."""
    need(vb.brave_focused(c), "Brave's window has the focus before keys go to it", ctl(c, "list"))
    ctl(c, "key", *words)


def open_tab(c, url):
    keys(c, dekeys.ctrl("t"))
    time.sleep(2)
    keys(c, dekeys.codes(url + "\n"))


def klog():
    return open(os.path.join(v.RUN, "serial.log"), "rb").read().decode("utf-8", "replace")


def ends_since(off):
    """`proc exit` lines of Brave's processes since serial-log offset `off`: (pid, status)."""
    return [(int(p), s) for p, s in re.findall(r"proc exit pid=(\d+) program=brave code=\d+ status=\"?([^\"\n]*)", klog()[off:])]


CRASH_SIGNALS = ("SIGSEGV", "SIGTRAP", "SIGILL", "SIGABRT", "SIGBUS")


def crash(c, b, url, kind_, browser, late):
    """Opens `url` (a crash page) in a new tab and waits until one of Brave's processes ends by a crash signal (on
    CachyOS the renderer of chrome://crash/ lives 33 s on while crashpad writes its dump, then the tab shows "Oh
    nein!" with SIGSEGV); returns their pids."""
    before = procs(c)
    off = len(klog())
    t0 = time.time()
    open_tab(c, url)
    hit = []
    while time.time() - t0 < 180 and not hit:
        time.sleep(2)
        nspids(c, sorted(p for p, t in procs(c).items() if t == kind_))
        hit = [(p, s) for p, s in ends_since(off) if any(x in s for x in CRASH_SIGNALS)]
        if re.search(r"^pid \d+ ended: exit \d+", b.poll(), re.M):
            raise SystemExit(f"RED: {url}: the launcher (Brave) ended\n{b.text[-2000:]}")
    after = procs(c)
    print(f"{url}: ended by a crash signal after {time.time() - t0:.0f} s: {hit or 'none'}; {kind_} pids before "
          f"{sorted(p for p, t in before.items() if t == kind_)}, after {sorted(p for p, t in after.items() if t == kind_)}", flush=True)
    if not hit:
        late.append(f"{url}: no process of Brave ended by a crash signal within 180 s")
        for p in sorted(p for p, t in after.items() if t == kind_):
            _, text, _ = call(c, "run", {"path": "/bin/sh", "args": ["-c", f"for t in /proc/{p}/task/*; do cut -d' ' -f1-3 $t/stat; done | tr '\\n' ' '"],
                                         "env": v.ENV, "timeout_ms": 20000})
            print(f"  {kind_} {p} threads (tid name state): {text.strip()[:400]}", flush=True)
    need(browser in after and after[browser] == "browser", f"{url}: Brave's browser process (pid {browser}) lives on")
    return [p for p, _ in hit]


def main():
    late = []  # checks that fail the run at its end, so the steps after them still run and show what they can
    c = v.connect(300)
    q = Qmp(v.QMP)
    comp = Job(c, "/bin/bitscomp", ["--socket", v.SOCK, "--stats", "30"])
    comp.wait_for(r"backend=fbdev size=1280x800", "bitscomp started on fbdev at 1280x800")
    _, text, _ = call(c, "run", {"path": "/bin/sh", "args": ["-c", "brave --check; echo rc=$?"], "env": v.ENV, "timeout_ms": 60000})
    print(text)
    need("rc=0" in text, "brave --check is fine", text)
    sb = ([None] + list(re.finditer(r"^brave: sandbox: .*$", text, re.M)))[-1]
    need(sb, "brave --check names the sandbox", text)

    # 1. Brave on render.html (tab 1), started by the launcher; its sandbox line.
    t0 = time.time()
    b = Job(c, "/bin/sh", ["/boot/BITSOS2/bin/brave", f"{vb.BASE}/render.html", "--", *vb.extra("10.0.2.2")])
    try:
        vb.wait_title(c, "BR-3 render ready", "Brave shows render.html", 240, b)
    except SystemExit:
        vb.diagnose(c, b)
        raise
    print(f"Brave: render.html ready {time.time() - t0:.1f} s after the start (QEMU, not judged)", flush=True)
    # The last line: a start Chromium refused ("No usable sandbox!") says a second one, `missing` (BR-6b: no fallback;
    # a kernel without namespaces then ends here or at the wait above, RED; 272 is never a known ENOSYS any more).
    line = ([None] + list(re.finditer(r"^brave: sandbox: .*$", b.poll(), re.M)))[-1]
    print(line.group(0) if line else "brave: no sandbox line", flush=True)
    on = bool(line and line.group(0).startswith("brave: sandbox: on"))
    if not on:
        late.append(f"the launcher started Brave without its sandbox: {line.group(0) if line else 'no sandbox line'}")
    ctl(c, "max")
    p0 = procs(c)
    browser = next((p for p, t in p0.items() if t == "browser"), None)
    need(browser is not None, "Brave's browser process is there", json.dumps(p0))
    print("Brave's processes: " + ", ".join(f"{p} {t}" for p, t in sorted(p0.items())), flush=True)

    # 2. chrome://sandbox in tab 2 against the host's picture.
    open_tab(c, "chrome://sandbox/")
    vb.wait_title(c, "Sandbox Status", "chrome://sandbox is shown", 60, b)
    w, h, px = vb.still(q, 200)
    rw, rh, rpx = v.read_ppm(os.path.join(vb.REF_DIR, "shot.ppm"))
    save_png(px, w, h, os.path.join(v.RUN, "sandbox-v2.png"))
    mine, ref = rows(px, w, h), rows(rpx, rw, rh)
    for i, lab in enumerate(LABELS[:max(len(mine), len(ref))] + [""] * (max(len(mine), len(ref)) - len(LABELS))):
        a = mine[i][2] if i < len(mine) else "-"
        r = ref[i][2] if i < len(ref) else "-"
        print(f"  {lab or 'row ' + str(i + 1):46} V2 {a:6} host {r:6}{'' if a == r else '   <- differs'}", flush=True)
    need(len(ref) == len(LABELS) and all(r[2] != "red" for r in ref),
         f"the host's chrome://sandbox has its {len(LABELS)} rows and none red (the reference is sound)", str(ref))
    same = [r[2] for r in mine] == [r[2] for r in ref]
    vd = differing(verdict(px, w, mine), verdict(rpx, rw, ref), RIGHT)
    print(f"chrome://sandbox: the verdict line differs from the host's in {vd} of {RIGHT * 36} pixels", flush=True)
    if same and vd <= RIGHT * 36 // 100:
        print("ok: chrome://sandbox on V2 shows the host's rows and verdict", flush=True)
    else:
        late.append(f"chrome://sandbox differs from the host's: V2 {[r[2] for r in mine]}, host {[r[2] for r in ref]}, "
                    f"verdict line {vd} pixels off (picture: {v.RUN}/sandbox-v2.png)")

    # 3. The kernel's view of the renderers: seccomp mode 2 (filter), no_new_privs, their namespaces.
    p1 = procs(c)
    rend = sorted(p for p, t in p1.items() if t == "renderer")
    need(rend, "Brave runs renderers", json.dumps(p1))
    for p in rend:
        st = status(c, p)
        print(f"  renderer {p}: " + "; ".join(f"{k} {st.get(k, '-')}" for k in ("Uid", "NSpid", "NoNewPrivs", "Seccomp", "Seccomp_filters")), flush=True)
        if on and st.get("Seccomp") != "2":
            late.append(f"renderer {p} has no seccomp filter (Seccomp: {st.get('Seccomp', '-')})")
        # As on CachyOS: the zygote is pid 1 of the sandbox's PID namespace and forks each renderer into a nested
        # one of its own (the renderer's main thread logs tid 1 there), so NSpid has three levels, the last 1.
        ns = st.get("NSpid", "").split()
        ALIASES.setdefault(p, set()).update(ns)
        if on and (len(ns) != 3 or ns[-1] != "1"):
            late.append(f"renderer {p} is not pid 1 of its own PID namespace inside the zygote's (NSpid: {st.get('NSpid', '-')}; "
                        "the host shows three levels, the last 1)")
    # The page's Broker row reads Yama's ptrace_scope (CachyOS: 1, green there).
    # run's answer wraps the output ("pid N ended ... --- stdout ..."): the value comes on its own marked line.
    _, text, _ = call(c, "run", {"path": "/bin/sh", "args": ["-c", "echo \"scope=$(cat /proc/sys/kernel/yama/ptrace_scope 2>&1)\""], "env": v.ENV, "timeout_ms": 20000})
    m = re.search(r"^scope=(.*)$", text, re.M)
    scope = m.group(1).strip() if m else text.strip()[:120]
    print(f"/proc/sys/kernel/yama/ptrace_scope: {scope}", flush=True)
    if scope != "1":
        late.append(f"/proc/sys/kernel/yama/ptrace_scope is not 1 as on CachyOS: {scope} (AB change request, br-BR-6.md)")

    # 4. chrome://crash in tab 3 ends that renderer only; tab 1 lives on; BR-1's checks load in a new tab.
    # The slash keeps the address bar from completing "chrome://crash" to chrome://crashes (the crash list).
    crashed = crash(c, b, "chrome://crash/", "renderer", browser, late)
    keys(c, dekeys.ctrl("w"))
    time.sleep(2)
    keys(c, dekeys.ctrl("1"))
    vb.wait_title(c, "BR-3 render ready", "tab 1 (render.html) lives on after the renderer crash", 30, b)
    open_tab(c, f"{vb.BASE}/index.html")
    vb.wait_title(c, "BR-1 done", "a new page (BR-1's checks) loads after the crash", 180, b)
    t = vb.title(c)
    need("all-ok" in t, "storage, IndexedDB, Cache Storage, fetch, Worker, WebAssembly and WebAudio work after the crash", t)

    # 5. chrome://gpucrash: the GPU process (software, --disable-gpu) ends; Brave lives on, a page loads again.
    crashed += crash(c, b, "chrome://gpucrash/", "gpu-process", browser, late)
    keys(c, dekeys.ctrl("w"))
    time.sleep(2)
    open_tab(c, f"{vb.BASE}/render.html")
    vb.wait_title(c, "BR-3 render ready", "a page loads after the GPU process crash", 120, b)

    # 6. The window's close ends Brave cleanly.
    text = vb.close(c, b, "closing Brave's window (several tabs) ends Brave and the launcher")
    need(re.search(r"Brave ended \(code 0\)", text), "Brave ended with code 0 after the crashes", text[-2000:])

    # 7. Brave's log, the kernel's lines.
    blog, off = b"", 0
    while True:
        err, text, _ = call(c, "read_file", {"path": vb.LOG, "offset": off})
        need(not err, f"read_file {vb.LOG}", text)
        j = json.loads(text)
        blog += base64.b64decode(j["data"])
        off += j["length"]
        if j["eof"] or not j["length"]:
            break
    blog = blog.decode("utf-8", "replace")
    open(os.path.join(v.RUN, "brave.log"), "w").write(blog)
    fatal = [l for l in blog.splitlines() if ":FATAL:" in l]
    on_purpose = {str(p) for p in crashed} | {x for p in crashed for x in ALIASES.get(p, ())}
    own = [l for l in fatal if not any(l.startswith(f"[{x}:") for x in on_purpose)]
    for l in fatal:
        print("  FATAL " + ("(crashed on purpose) " if l not in own else "") + l[:200], flush=True)
    if own:
        late.append("FATAL lines from processes not crashed on purpose:\n" + "\n".join(own))
    k = klog()
    faults = [l for l in k.splitlines() if re.search(r"proc fault .*program=(brave|chrome_crashpad)", l)]
    bad = [l for l in faults if not any(re.search(rf"pid={x}\b", l) for x in on_purpose)]
    for l in faults:
        print("  kernel: " + l[:240], flush=True)
    if bad:
        late.append("CPU faults in Brave's processes not crashed on purpose:\n" + "\n".join(bad))
    kills = [l for l in k.splitlines() if "seccomp kill" in l]
    if kills:
        late.append("seccomp killed a process (a call Chromium's policy does not expect to be refused):\n" + "\n".join(kills[:10]))
    seen = {}
    for m in re.finditer(r"syscall enosys nr=(\d+) name=\S+ program=(\S+)", k):
        seen.setdefault(int(m.group(1)), m.group(2))
    print("ENOSYS since boot (first program): " + ", ".join(f"{n} {vb.KNOWN_ENOSYS.get(n, '?')} ({p})" for n, p in sorted(seen.items())))
    new = sorted(n for n in seen if n not in vb.KNOWN_ENOSYS)
    if new:
        late.append("ENOSYS beyond the known (SX change requests): " + ", ".join(f"{n} ({seen[n]})" for n in new))
    ctl(c, "quit")
    if late:
        raise SystemExit("RED: " + "\nRED: ".join(late))
    print("ok: Brave runs in its own sandbox on V2", flush=True)


if __name__ == "__main__":
    main()
