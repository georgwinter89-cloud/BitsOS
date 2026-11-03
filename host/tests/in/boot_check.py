#!/usr/bin/env python3
"""DS-4 scenario client (50-boot-screen.sh): the boot screen seen through QEMU's screendump (QMP) and the step
list through MCP (`read_file /proc/bitsos/boot`).

Usage: boot_check.py fail|key|up|off|emergency <host port> <token> <qmp socket> <run dir> <repo root> <serial log>
fail (desktop_program=/bin/sleep 6): the boot screen appears early; a screendump equals the Rust model drawn from /proc/bitsos/boot (cargo test
      ... bootscreen_matches_model) mid-start and again with the failing desktop step; after three failed starts
      the text console shows the message.
key:  a key (QMP send-key) while the boot screen is up brings the text console.
up:   /bin/bitscomp takes the screen; the step list says the desktop runs.
off:  no boot screen; /proc/bitsos/boot still lists the steps, the desktop step skipped.
emergency: ktest in.boot.emergency runs while the boot screen is up (the script reads its result).
Exit status 0 = green."""
import http.client
import json
import os
import socket
import subprocess
import sys
import time

MODE, PORT, TOKEN, QMP_PATH, RUN_DIR, ROOT, SERIAL = sys.argv[1], int(sys.argv[2]), sys.argv[3], sys.argv[4], sys.argv[5], sys.argv[6], sys.argv[7]
next_id = 0
# Where the ring's core dot is (crates/console/src/bootdraw.rs: RING_CENTRE 1280,507 on 2560x1440).
CORE = (1280, 507)
CONSOLE_BG = 0x101418


def need(cond, what, detail=""):
    if not cond:
        raise SystemExit(f"RED ({MODE}): {what}\n{detail}")
    print(f"  ok: {what}")


def post(c, body):
    c.request("POST", "/mcp", json.dumps(body), {"Authorization": f"Bearer {TOKEN}", "Content-Type": "application/json"})
    r = c.getresponse()
    data = r.read()
    return r.status, (json.loads(data) if data else None)


def rpc(c, method, params=None):
    global next_id
    next_id += 1
    st, j = post(c, {"jsonrpc": "2.0", "id": next_id, "method": method, "params": params or {}})
    if st != 200 or j is None or "result" not in j:
        raise ValueError(f"{method}: HTTP {st} {j}")
    return j["result"]


def connect(wait_s):
    end = time.time() + wait_s
    while True:
        try:
            c = http.client.HTTPConnection("127.0.0.1", PORT, timeout=90)
            rpc(c, "initialize", {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "ds4", "version": "1"}})
            return c
        except (OSError, http.client.HTTPException, ValueError) as e:
            if time.time() > end:
                raise SystemExit(f"RED: no MCP answer on 127.0.0.1:{PORT}: {e}")
            time.sleep(0.5)


def tool(c, name, args=None):
    r = rpc(c, "tools/call", {"name": name, "arguments": args or {}})
    return "\n".join(i.get("text", "") for i in r["content"] if i["type"] == "text"), r.get("isError", False)


def boot_proc(c):
    t, err = tool(c, "read_file", {"path": "/proc/bitsos/boot", "encoding": "text"})
    if err:
        raise SystemExit(f"RED: read_file /proc/bitsos/boot: {t}")
    return json.loads(t)["text"]


def proc_fields(text):
    f = {}
    for line in text.splitlines():
        k, _, v = line.partition(" ")
        if k == "step":
            sid, _, rest = v.partition(" ")
            f["step " + sid] = rest
        else:
            f[k] = v
    return f


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
                time.sleep(0.2)
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


def screendump(q, name):
    path = os.path.join(RUN_DIR, name)
    if os.path.exists(path):
        os.remove(path)
    q.cmd("screendump", {"filename": path})
    return path


def ppm_pixel(path, x, y):
    with open(path, "rb") as f:
        data = f.read()
    parts, i = [], 0
    while len(parts) < 4:
        while data[i:i + 1].isspace():
            i += 1
        j = i
        while not data[j:j + 1].isspace():
            j += 1
        parts.append(data[i:j])
        i = j
    i += 1
    w, h = int(parts[1]), int(parts[2])
    if x >= w or y >= h:
        return None
    o = i + (y * w + x) * 3
    return data[o] << 16 | data[o + 1] << 8 | data[o + 2]


def is_core(p):
    """The ring's accent core dot (green, bright)."""
    return p is not None and (p >> 8) & 0xFF > 0xB0 and (p >> 16) & 0xFF < 0xA0


def serial():
    with open(SERIAL, "rb") as f:
        return f.read().decode("utf-8", "replace")


def wait_boot_screen(q, timeout=120):
    """Screendumps until the boot screen's core dot shows; returns (dump, serial at that moment)."""
    end = time.time() + timeout
    n = 0
    while time.time() < end:
        n += 1
        d = screendump(q, "early.ppm")
        if is_core(ppm_pixel(d, *CORE)):
            return d, serial()
        time.sleep(0.1)
    raise SystemExit(f"RED ({MODE}): the boot screen never showed ({n} screendumps)")


def matches_model(c, q, name):
    """A screendump between two equal reads of the step list that the screen has drawn; the Rust model drawn from
    that list must be the same picture. Retries while the list moves."""
    for _ in range(40):
        a = boot_proc(c)
        d = screendump(q, name + ".ppm")
        b = boot_proc(c)
        fa = proc_fields(a)
        if a == b and fa.get("drawn") == fa.get("seq") and fa.get("screen") == "shown":
            proc = os.path.join(RUN_DIR, name + ".proc")
            with open(proc, "w", encoding="utf-8") as f:
                f.write(a)
            env = dict(os.environ, BITSOS_SCREENDUMP=d, BITSOS_BOOT_PROC=proc)
            r = subprocess.run(["cargo", "test", "-q", "-p", "console", "--lib", "bootscreen_matches_model", "--", "--ignored", "--nocapture"],
                               cwd=ROOT, env=env, capture_output=True, text=True)
            need(r.returncode == 0, f"screendump ({name}) equals the Rust model of /proc/bitsos/boot pixel for pixel", r.stdout[-3000:] + r.stderr[-3000:])
            return a
        time.sleep(0.15)
    raise SystemExit(f"RED ({MODE}): the step list never stood still for a screendump ({name})")


def wait_proc(c, cond, what, timeout=90, poll=0.2):
    end = time.time() + timeout
    t = ""
    while time.time() < end:
        t = boot_proc(c)
        if cond(proc_fields(t)):
            return t
        time.sleep(poll)
    raise SystemExit(f"RED ({MODE}): {what}\n{t}")


def fail(q):
    d, early = wait_boot_screen(q)
    print(f"  boot screen first seen with {len(early.splitlines())} serial lines; kernel ready by then: {'I kernel ready' in early}")
    c = connect(240)
    matches_model(c, q, "mid")
    wait_proc(c, lambda f: f.get("step desktop", "").startswith("running") and "beendet" in f.get("step desktop", ""),
              "the desktop step shows a failed start", poll=0.05)
    t = matches_model(c, q, "failing")
    need("beendet" in proc_fields(t)["step desktop"], "the failing step says why", t)
    t = wait_proc(c, lambda f: f.get("screen") == "text", "three failed starts end the boot screen", 60)
    need(proc_fields(t)["step desktop"].startswith("failed"), "the desktop step failed", t)
    time.sleep(1)
    text, _ = tool(c, "screen", {"format": "text"})
    need("3-mal nicht gestartet" in text, "the text console says the desktop did not start", text)
    p = ppm_pixel(screendump(q, "after.ppm"), *CORE)
    need(not is_core(p), f"the boot picture is gone (core pixel {p:06x})")


def key(q):
    wait_boot_screen(q)
    # The USB keyboard must be there first (the boot screen shows before USB is enumerated).
    end = time.time() + 60
    while 'name="USB HID' not in serial():
        if time.time() > end:
            raise SystemExit("RED (key): the USB keyboard never registered")
        time.sleep(0.1)
    p = ppm_pixel(screendump(q, "key-before.ppm"), *CORE)
    need(is_core(p), "the boot screen is still up before the key")
    q.cmd("send-key", {"keys": [{"type": "qcode", "data": "spc"}], "hold-time": 40})
    end = time.time() + 20
    p = None
    while time.time() < end:
        p = ppm_pixel(screendump(q, "key.ppm"), *CORE)
        if not is_core(p):
            break
        time.sleep(0.2)
    need(not is_core(p), f"a key brings the text console (core pixel {p:06x})")
    c = connect(240)
    t = boot_proc(c)
    need(proc_fields(t).get("screen") == "text", "the step list says the text console has the screen", t)


def up(q):
    wait_boot_screen(q)
    c = connect(240)
    t = wait_proc(c, lambda f: f.get("step desktop", "").startswith("done"), "bitscomp started and took the screen", 120)
    need(proc_fields(t).get("screen") == "handed", "the boot screen handed the screen over", t)
    time.sleep(2)
    desktop_shown(q, "desktop.ppm")


def off(q):
    c = connect(240)
    t = wait_proc(c, lambda f: f.get("step services", "").startswith("done"), "init started (desktop=off boot)")
    f = proc_fields(t)
    need(f.get("screen") == "off" and f.get("step desktop", "").startswith("skipped"), "no boot screen, the desktop step skipped", t)
    for sid in ("kernel", "memory", "cpus"):
        need(f.get("step " + sid, "").startswith("done"), f"step {sid} done", t)
    p = ppm_pixel(screendump(q, "off.ppm"), *CORE)
    need(not is_core(p), f"the text console as today (core pixel {p:06x})")


def missing(q):
    wait_boot_screen(q)
    c = connect(240)
    t = wait_proc(c, lambda f: f.get("screen") == "text", "three spawn errors end the boot screen", 120)
    need(proc_fields(t)["step desktop"].startswith("failed"), "the desktop step failed", t)
    time.sleep(1)
    text, _ = tool(c, "screen", {"format": "text"})
    need("3-mal nicht gestartet" in text, "the text console says the desktop did not start", text)


def emergency(q):
    wait_boot_screen(q)
    end = time.time() + 180
    while "KTEST done" not in serial():
        if time.time() > end:
            raise SystemExit("RED (emergency): the ktests did not finish")
        time.sleep(0.2)


def watch_metrics(c):
    """IN-9: (klog has a watch, step lines it handed over, of them unreadable) from MCP `metrics`."""
    t, err = tool(c, "metrics", {"name": "klog.watched,console.bootsteps.", "last": 30, "max": 1, "agg": "last"})
    need(not err, "MCP metrics answers", t)
    m = {}
    for line in t.splitlines():
        f = line.split()
        if len(f) >= 3 and f[0].startswith(("klog.watched", "console.bootsteps.")) and f[-1] != "-":
            m[f[0]] = int(float(f[-1]))
    return "klog.watched" in t, m.get("console.bootsteps.watched", 0), m.get("console.bootsteps.watch_unparsed", 0)


def desktop_shown(q, name):
    """The picture is bitscomp's, neither the boot screen (core dot) nor the text console (its background at the
    core and in the top-left corner; bitscomp's start page is lighter in both)."""
    d = screendump(q, name)
    p, corner = ppm_pixel(d, *CORE), ppm_pixel(d, 10, 10)
    print(f"  desktop picture core pixel {p:06x}, corner {corner:06x}")
    need(not is_core(p), f"the boot screen is gone (core pixel {p:06x})")
    need(p != CONSOLE_BG and corner != CONSOLE_BG, f"the desktop, not the text console (core {p:06x}, corner {corner:06x})")


def errlvl(q):
    """IN-9: loglevel=error (Georg's normal) or warn, and desktop=on: every step settles and bitscomp takes the
    screen, fed through klog's watch (the Info lines themselves never reach the ring: checked on serial by the
    script). BITSOS_REQUIRE_WATCH=1 (the desk's merge gate with the change requests applied) makes a missing
    watch red instead of a skip."""
    wait_boot_screen(q)
    c = connect(240)
    # The sampler writes a round every 2 s: wait for one with the counters.
    time.sleep(5)
    has_watch, watched, unparsed = watch_metrics(c)
    if not has_watch:
        if os.environ.get("BITSOS_REQUIRE_WATCH") == "1":
            raise SystemExit(f"RED ({MODE}): BITSOS_REQUIRE_WATCH=1 but klog has no watch (change request IN-9 CR-1 not applied)")
        print(f"  SKIP ({MODE}): klog has no watch yet (change request IN-9 CR-1); the step list cannot see Info lines at error")
        return
    t = wait_proc(c, lambda f: f.get("step desktop", "").startswith("done"), "bitscomp started and took the screen at loglevel=error", 120)
    f = proc_fields(t)
    need(f.get("screen") == "handed", "the boot screen handed the screen over", t)
    for sid in ("kernel", "memory", "cpus", "services", "desktop"):
        need(f.get("step " + sid, "").startswith("done"), f"step {sid} done", t)
    waiting = [k for k, v in f.items() if k.startswith("step ") and v.startswith("waiting")]
    need(not waiting, "no step still waiting", t)
    need(int(f.get("seq", "0")) > 5, "the list moved", t)
    has_watch, watched, unparsed = watch_metrics(c)
    print(f"  watched step lines {watched}, unreadable {unparsed}")
    need(watched >= 8 and unparsed == 0, "the step lines came through the watch, all readable")
    time.sleep(2)
    desktop_shown(q, f"{MODE}.ppm")


def main():
    q = Qmp(QMP_PATH)
    {"fail": fail, "key": key, "up": up, "off": off, "missing": missing, "emergency": emergency, "errlvl": errlvl, "warnlvl": errlvl}[MODE](q)
    print(f"in boot screen ({MODE}): green")


main()
