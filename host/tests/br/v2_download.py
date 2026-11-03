#!/usr/bin/env python3
"""BR-7 scenario client (40-download.sh): a download and the save dialog in Brave on V2, driven over MCP.

Usage: v2_download.py <mcp host port> <token> <qmp socket> <run dir> - <http port>
Waits for the data volume, `brave --check` (downloads folder on it), starts Brave on download.html (it clicks its
own download link), waits for the whole file in /data/home/1000/Downloads (Brave's question where to save gets its
Save button if Brave asks), opens Save as with Ctrl+S and saves the page with Save into the same folder, closes Brave,
then checks Brave's log (no GTK abort, no pixbuf warning, no FATAL), the kernel's faults and ENOSYS. Times are
printed, never judged (QEMU). Exit 0 = green."""
import base64
import hashlib
import json
import os
import re
import sys
import time

sys.dont_write_bytecode = True  # no __pycache__ in the tree
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import v2_brave as br  # noqa: E402  (sets up v2_check from argv, pins the light theme)
import v2_check as v  # noqa: E402
from v2_check import Job, call, ctl, need  # noqa: E402

DL = "/data/home/1000/Downloads"
FILE = "br7-download.bin"
MAIN = "BR-7 download page"
# GTK's and GLib's fatal messages, gdk-pixbuf's and glycin's "no loader" words (24.10.), Chromium's FATAL.
BAD = re.compile(r"Gtk:ERROR|-ERROR \*\*|Could not load a pixbuf|Unrecognized image file format|"
                 r"No image loaders are configured|mime database could not be found|Couldn't recognize the image file format|:FATAL:")
NOISE = "Failed to connect to the bus"
# glycin's one line when it runs its loader without a sandbox (libexec/bwrap's answer); any other glycin line (a
# loader that cannot start, "Could not spawn ... bwrap", a missing config) fails the check even without a GTK abort.
GLYCIN_OK = "WARNING: Glycin running without sandbox. Bubblewrap (bwrap) doesn't work in the environment."


def self_test():
    """BAD catches the 24.10. abort (Georg's log, plan/notes/desk-minipc-w13.md) and lets the known lines pass."""
    caught = ["Gtk:ERROR:../gtk/gtkiconhelper.c:495:ensure_surface_for_gicon: assertion failed (error == NULL): "
              "Unrecognized image file format (gdk-pixbuf-error-quark, 3)",
              "Bail out! Gtk:ERROR:../gtk/gtkiconhelper.c:495:ensure_surface_for_gicon: assertion failed",
              "(brave:12): GLib-GIO-ERROR **: 10:00:00.000: No GSettings schemas are installed on the system",
              "Could not load a pixbuf from /org/gtk/libgtk/icons/32x32/status/image-missing.png.",
              "This may indicate that pixbuf loaders or the mime database could not be found.",
              "Couldn't recognize the image file format for file \u201c/tmp/x.png\u201d",
              "[1:1:FATAL:content/browser/zygote_host/zygote_host_impl_linux.cc:126] No usable sandbox!"]
    passed = [GLYCIN_OK, "[12:34:ERROR:dbus/bus.cc:408] Failed to connect to the bus: Could not parse server address"]
    miss = [l for l in caught if not BAD.search(l)] + [l for l in passed if BAD.search(l)]
    need(not miss, "self-test: the log check catches GTK's 24.10. abort and passes the known lines", "\n".join(miss))
# BR-3's list and seccomp: libseccomp in glycin (in Brave's process) probes the API before it asks bwrap for a
# sandbox (host strace: SECCOMP_GET_ACTION_AVAIL, ...); with ENOSYS it goes on (A7b brings seccomp).
# INT-W27b: with the real /bin/bwrap in the image (SX-3b CR2) glycin's bwrap asks openat2 and mount_setattr first
# and falls back on ENOSYS (SX-3c CR3); without seccomp glycin then runs its loader unsandboxed, as before.
KNOWN_ENOSYS = {**br.KNOWN_ENOSYS, 317: "seccomp", 437: "openat2 (bwrap)", 442: "mount_setattr (bwrap)"}


def sh(c, cmd, ms=30000, quiet=False):
    err, text, _ = call(c, "run", {"path": "/bin/sh", "args": ["-c", cmd], "env": v.ENV, "timeout_ms": ms})
    if err:
        raise SystemExit(f"RED: run {cmd}\n{text}")
    if not quiet:
        print(f"ok: run {cmd[:80]}", flush=True)
    return text


def late(t0, secs, what, detail=""):
    """RED once `secs` have passed since t0 (a wait loop's limit; silent while there is time)."""
    if time.time() - t0 > secs:
        raise SystemExit(f"RED: {what} within {secs} s\n{detail}")


# Brave's main window is app_id brave-browser; its GTK dialogs (the file chooser) come with GTK's app_id "brave".
BRAVE_APP = re.compile(r"app_id=brave(-browser)? ")


def windows(c):
    return [l for l in ctl(c, "list").splitlines() if BRAVE_APP.search(l)]


def focused(c):
    """The `ctl list` line of the focused window when it is one of Brave's, else None."""
    for l in ctl(c, "list").splitlines():
        if "focused=true" in l and BRAVE_APP.search(l):
            return l
    return None


def listing(c):
    """The downloads folder's files as {name: size}."""
    text = sh(c, f'cd {DL} && for f in * .[!.]*; do [ -f "$f" ] && echo "F $(stat -c %s "$f") $f"; done; echo end', quiet=True)
    return {n: int(s) for s, n in re.findall(r"^F (\d+) (.+)$", text, re.M)}


def dump(c):
    """Brave's log into the run dir and its telling lines (GTK, GLib, pixbuf, glycin, FATAL) printed."""
    try:
        blog = read_log(c)
    except SystemExit as e:
        print(f"Brave's log cannot be read: {e}")
        return
    open(os.path.join(v.RUN, "brave.log"), "w").write(blog)
    keep = [l for l in blog.splitlines() if re.search(r"Gtk|GLib|GdkPixbuf|pixbuf|glycin|FATAL|ERROR|CRITICAL", l)]
    print(f"Brave's log ({len(blog)} bytes, {v.RUN}/brave.log), telling lines:\n  " + "\n  ".join(keep[-40:]), flush=True)


def alive(job, what, c=None):
    if re.search(r"^pid \d+ ended: exit \d+", job.poll(), re.M):
        raise SystemExit(f"RED: {what}: the launcher (Brave) ended\n{job.text[-3000:]}")


SHOTS = []


def shot(what):
    """A screendump into the run dir (dumpN.ppm), for the record and for a RED's look."""
    try:
        n = 500 + len(SHOTS)
        Q[0].screendump(n)
        SHOTS.append(f"dump{n}.ppm: {what}")
        print(f"  screen: {v.RUN}/dump{n}.ppm ({what})", flush=True)
    except (SystemExit, OSError, IndexError) as e:
        print(f"  no screendump: {e}", flush=True)


Q = []
C = []


def answer(c, job, what):
    """Saves in Brave's focused GTK dialog (never keys to the main window or another program): the dialog maximised
    (its Save button is then on the screen, for the record), Alt+S = its "_Save" button. Enter in the name field does
    not save on V2 (an open point in plan/notes/br-BR-7.md). True when sent."""
    f = focused(c)
    if f and MAIN not in f:
        print(f"  Alt+S (Save) to Brave's dialog for {what}: {f}", flush=True)
        ctl(c, "max")
        time.sleep(3)
        shot(f"the dialog for {what}, maximised")
        call(c, "send_keys", {"keys": ["alt+s"]})
        return True
    return False


def read_log(c):
    blog, off = b"", 0
    while True:
        err, text, _ = call(c, "read_file", {"path": br.LOG, "offset": off})
        need(not err, f"read_file {br.LOG}", text)
        j = json.loads(text)
        blog += base64.b64decode(j["data"])
        off += j["length"]
        if j["eof"] or not j["length"]:
            return blog.decode("utf-8", "replace")


def main():
    self_test()
    data = open(os.path.join(v.RUN, "www", FILE), "rb").read()
    size, sha = len(data), hashlib.sha256(data).hexdigest()
    c = v.connect(300)
    C.append(c)
    Q.append(v.Qmp(v.QMP))
    comp = Job(c, "/bin/bitscomp", ["--socket", v.SOCK, "--stats", "30"])
    comp.wait_for(r"backend=fbdev size=1280x800", "bitscomp started on fbdev at 1280x800")

    # The data volume: made at this first boot (64 MiB), mounted in the background.
    t0 = time.time()
    while True:
        text = sh(c, 'echo "STATE=$(head -n 1 /run/bitsos/data-state 2>/dev/null)"', quiet=True)
        m = re.search(r"^STATE=(.*)$", text, re.M)
        st = m.group(1).strip() if m else ""
        if st.startswith("mounted") and " rw" in st:
            print(f"ok: the data volume is mounted ({st!r}, {time.time() - t0:.1f} s)", flush=True)
            break
        late(t0, 300, "the data volume is mounted read-write", text)
        time.sleep(2)

    text = sh(c, "brave --check; echo rc=$?", 60000)
    print(text)
    need("rc=0" in text and re.search(r"libraries: \d+ found, none missing", text),
         "brave --check: tree (MIME database, icons, loaders), libraries", text)
    need(f"downloads: {DL} (data volume)" in text, f"brave --check names {DL}", text)

    # 1. The page clicks its download link: the file arrives whole in the data volume's folder.
    t0 = time.time()
    b = Job(c, "/bin/sh", ["/boot/BITSOS2/bin/brave", f"{br.BASE}/download.html", "--", *br.extra("10.0.2.2")])
    try:
        br.wait_title(c, MAIN, "Brave shows download.html", 240, b)
    except SystemExit:
        br.diagnose(c, b)
        raise
    need(f"brave: downloads: {DL} (data volume)" in b.poll(), f"the launcher sets {DL} for Brave", b.text[-2000:])
    t0, asked, last = time.time(), 0, 0.0
    while True:
        files = listing(c)
        if files.get(FILE) == size:
            break
        alive(b, "the download", c)
        if len(windows(c)) > 1 and asked < 3 and time.time() - last > 5 and answer(c, b, "where to save"):
            asked, last = asked + 1, time.time()
        late(t0, 180, f"{FILE} ({size} bytes) arrives in {DL}", f"{files}\n{ctl(c, 'list')}")
        time.sleep(2)
    print(f"ok: {FILE} in {DL} after {time.time() - t0:.1f} s ({'Brave asked where: saved in its dialog' if asked else 'without a question'})",
          flush=True)
    text = sh(c, f"sha256sum {DL}/{FILE}; stat -c 'uid=%u' {DL}/{FILE}")
    need(sha in text, f"{FILE} has the host's bytes (sha256 {sha[:16]}...)", text)
    need("uid=1000" in text, f"{FILE} belongs to uid 1000", text)

    # 2. Save as (Ctrl+S): GTK's file chooser opens and draws its icons (24.10.: GTK aborted here); Save saves.
    before = len(windows(c))
    f = focused(c)
    need(f and MAIN in f, "Brave's main window has the focus for Ctrl+S", ctl(c, "list"))
    call(c, "send_keys", {"keys": ["escape"]})  # the download bubble, if one is open
    time.sleep(1)
    call(c, "send_keys", {"keys": ["ctrl+s"]})
    t0 = time.time()
    while len(windows(c)) <= before:
        alive(b, "Ctrl+S", c)
        late(t0, 60, "Ctrl+S opens the save dialog", ctl(c, "list"))
        time.sleep(1)
    dialog = [l for l in windows(c) if MAIN not in l]
    print(f"ok: the save dialog is open after {time.time() - t0:.1f} s: {dialog}", flush=True)
    time.sleep(5)  # it draws its places, folders and icons
    alive(b, "the open save dialog", c)
    ps = call(c, "ps", {})[1]
    print("loader programs running: " + (", ".join(sorted(set(re.findall(r"glycin-[a-z-]+", ps)))) or "none (ended)"), flush=True)
    # Not a check (an open point, plan/notes/br-BR-7.md): Enter in the name field, GTK's default response, saved
    # nothing on V2 on 26.10. Logged each run so the gap stays visible; Alt+S below saves either way.
    f = focused(c)
    if f and MAIN not in f:
        call(c, "send_keys", {"keys": ["enter"]})
        time.sleep(5)
        saved = [n for n in listing(c) if n.endswith((".html", ".htm", ".mhtml"))]
        print(f"info: Enter in the save dialog ({f}): {'saved ' + str(saved) if saved else 'saved nothing (open point)'}", flush=True)
    t0, sent = time.time(), 0
    while True:
        html = [n for n in listing(c) if n.endswith(".html") or n.endswith(".htm") or n.endswith(".mhtml")]
        if html and len(windows(c)) <= before:
            break
        alive(b, "saving the page", c)
        if len(windows(c)) > before and sent < 3 and time.time() - t0 > 5 * sent and answer(c, b, "Save as"):
            sent += 1
        late(t0, 90, f"Save saves the page into {DL} and closes the dialog", f"{listing(c)}\n{ctl(c, 'list')}")
        time.sleep(2)
    print(f"ok: the page saved through the dialog into {DL}: {html} ({time.time() - t0:.1f} s)", flush=True)
    need(sent >= 1, "the page was saved through the save dialog (Alt+S sent to it)")

    # 3. Brave ends by its window's close.
    text = br.close(c, b, "closing Brave's window ends Brave and the launcher")
    need(re.search(r"Brave ended \(code 0\)", text), "Brave ended with code 0", text[-2000:])

    # 4. Brave's log, the kernel's faults and ENOSYS.
    blog = read_log(c)
    open(os.path.join(v.RUN, "brave.log"), "w").write(blog)
    lines = [re.sub(r"^\[[^]]*\]", "", l) for l in blog.splitlines() if l.strip()]
    noise = sum(NOISE in l for l in lines)
    print(f"Brave's log: {len(blog)} bytes; {noise} known D-Bus line(s); the other lines, once each:")
    for l in dict.fromkeys(l for l in lines if NOISE not in l):
        print("  " + l[:200])
    bad = [l for l in lines if BAD.search(l)]
    need(not bad, "no GTK/GLib abort, no pixbuf or loader warning, no FATAL in Brave's log", "\n".join(bad))
    gl = [l for l in lines if re.search(r"glycin", l, re.I) and l.strip() != GLYCIN_OK]
    need(not gl, "no glycin line beyond its known WARNING (no sandbox)", "\n".join(gl))
    klog = open(os.path.join(v.RUN, "serial.log"), "rb").read().decode("utf-8", "replace")
    faults = [l for l in klog.splitlines() if re.search(r"proc fault .*program=(brave|chrome_crashpad|glycin)", l)]
    need(not faults, "no CPU fault in Brave, its helpers or the image loaders", "\n".join(faults))
    seen = {}
    for m in re.finditer(r"syscall enosys nr=(\d+) name=\S+ program=(\S+)", klog):
        seen.setdefault(int(m.group(1)), m.group(2))
    # BR-6b: on (SX-2 in); off only by BRAVE_SANDBOX=off. Downloads, Ctrl+S and the dialog run under the sandbox.
    br.sandbox_line(b.text)
    KNOWN_ENOSYS.update(br.KNOWN_ENOSYS)
    print("ENOSYS since boot (first program): " + ", ".join(f"{n} {KNOWN_ENOSYS.get(n, '?')} ({p})" for n, p in sorted(seen.items())))
    new = sorted(n for n in seen if n not in KNOWN_ENOSYS)
    need(not new, f"no ENOSYS beyond the known {sorted(KNOWN_ENOSYS.values())}", ", ".join(f"{n} ({seen[n]})" for n in new))
    ctl(c, "quit")


if __name__ == "__main__":
    try:
        main()
    except SystemExit as e:
        if e.code not in (0, None) and C:
            dump(C[0])
        raise
