#!/usr/bin/env python3
"""PK-0 on V2 through MCP (QEMU hostfwd) and the root console shell (QMP keys): /apps on /data survives a reboot.

Usage: pk_apps_check.py <phase> <host port> <token> <qmp socket> <serial log> <tar with pkg/>
Phases (one boot each; QEMU ends with the MCP reboot of phases 1, -no-reboot):
  data-1   /run/bitsos/apps-state says `data: stick data.img 64 MB`, /apps is a mount; the fixture package installs as
           root (typed into the console shell: MCP's run is uid 1000 and /data/apps is root's), its settings key is
           registered; version 2 installs over it (links and renames on the bound /apps); verify --store clean; reboot
  data-2   apps-state again; `bitspkg list` still shows 2 and 1, `verify --store` clean, `space` names the volume;
           settingsd keeps no package key across a boot, and the serial log shows init's `bitspkg register` giving
           it back (settingsd's `registered owner=bitspkg` after it listened, before register's `done`); then remove:
           no package, no store or trash entry, no data folder of the package left (B2; ctxd's own folder stays)
  tmpfs-1  without a data image: apps-state `tmpfs: no data volume (data_img_mb=0)`; installs; reboot
  tmpfs-2  the same line, the package is gone (expected: /apps was the initramfs tmpfs)
Exit status 0 = green."""
import base64
import http.client
import json
import re
import socket
import sys
import time

PHASE, PORT, TOKEN, QMP_PATH, LOG, TAR = sys.argv[1], int(sys.argv[2]), sys.argv[3], sys.argv[4], sys.argv[5], sys.argv[6]
BP = "/bin/bitspkg"
W = "/tmp/pk0"
NAME = "appsdemo"
KEY = "app.appsdemo.note"
# Data folders of system services, not of packages: ctxd's one folder, made when the service starts
# (user/services/ctxd/DESIGN.md section 4, src/system.rs DATA); removing a package never touches it.
SERVICE_DIRS = {"/data/appdata/ctxd"}
next_id = 0


def need(cond, what, detail=""):
    if not cond:
        raise SystemExit(f"RED: {what}\n{str(detail)[:3000]}")
    print(f"ok   {what}", flush=True)


def rpc(c, method, params=None):
    global next_id
    next_id += 1
    c.request("POST", "/mcp", json.dumps({"jsonrpc": "2.0", "id": next_id, "method": method, "params": params or {}}),
              {"Authorization": f"Bearer {TOKEN}", "Content-Type": "application/json"})
    r = c.getresponse()
    j = json.loads(r.read() or b"null")
    if r.status != 200 or not j or "result" not in j:
        raise ValueError(f"{method}: HTTP {r.status} {j}")
    return j["result"]


def connect(wait_s):
    end = time.time() + wait_s
    while True:
        try:
            c = http.client.HTTPConnection("127.0.0.1", PORT, timeout=120)
            rpc(c, "initialize", {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "pk0", "version": "1"}})
            return c
        except (OSError, http.client.HTTPException, ValueError) as e:
            if time.time() > end:
                raise SystemExit(f"RED: no MCP answer on 127.0.0.1:{PORT}: {e}")
            time.sleep(1)


def call(c, name, args=None):
    r = rpc(c, "tools/call", {"name": name, "arguments": args or {}})
    return r["isError"], "\n".join(i.get("text", "") for i in r["content"] if i["type"] == "text")


def read(c, path):
    err, t = call(c, "read_file", {"path": path})
    if err:
        return None
    try:
        j = json.loads(t)
        return j["text"] if "text" in j else base64.b64decode(j.get("data", "")).decode("utf-8", "replace")
    except (ValueError, KeyError, TypeError):
        return t


def put(c, path, data):
    step = 96 * 1024
    for off in range(0, len(data), step):
        err, t = call(c, "write_file", {"path": path, "data": base64.b64encode(data[off:off + step]).decode(), "offset": off})
        if err:
            raise SystemExit(f"RED: write_file {path} at {off}: {t}")


def run(c, argv, timeout_ms=60000):
    """(exit status line, stdout, stderr) of a program run over MCP (uid 1000)."""
    err, t = call(c, "run", {"path": argv[0], "args": argv[1:], "timeout_ms": timeout_ms, "kill_after_ms": timeout_ms})
    if err or "\n--- stdout (" not in t:
        return None, t, t
    head, rest = t.split("\n--- stdout (", 1)
    out, errpart = rest.split("\n--- stderr (", 1)
    body = lambda part: part.split(") ---", 1)[1].removeprefix("\n")
    return head.splitlines()[0], body(out), body(errpart)


def ok(st):
    return bool(st) and st.endswith("exit 0")


def serial():
    with open(LOG, "rb") as f:
        return f.read().decode("utf-8", "replace")


def wait_serial(pattern, what, timeout=60):
    end = time.time() + timeout
    rx = re.compile(pattern, re.M)
    while time.time() < end:
        m = rx.search(serial())
        if m:
            return m
        time.sleep(0.5)
    raise SystemExit(f"RED: {what}: no /{pattern}/ in the serial log\n" + "\n".join(serial().splitlines()[-30:]))


# QEMU qcodes on V2's German console layout ('-' where the US slash is, '/' = shift+7); no y/z (swapped).
KEYS = {c: [c] for c in "abcdefghijklmnopqrstuvwx0123456789"}
KEYS.update({" ": ["spc"], "-": ["slash"], "\n": ["ret"], "/": ["shift", "7"]})


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
                time.sleep(0.5)
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


def apps_state(c, want, wait_s=180):
    end = time.time() + wait_s
    t = None
    while time.time() < end:
        t = read(c, "/run/bitsos/apps-state")
        if t is not None and not t.startswith("waiting"):
            break
        time.sleep(1)
    need(t == want + "\n", f"/run/bitsos/apps-state says {want!r}", repr(t))


def listed(c):
    st, out, err = run(c, [BP, "list"])
    need(ok(st), "bitspkg list runs", f"{st}\n{out}{err}")
    return out


def verify_clean(c, packages=1, versions=1):
    st, out, err = run(c, [BP, "verify", "--store"], 120000)
    need(ok(st) and out.startswith(f"verify: all good ({packages} packages, {versions} versions"),
         f"bitspkg verify --store is clean ({packages} packages, {versions} versions)", f"{st}\n{out}{err}")


def key_known(c):
    st, out, err = run(c, ["/bin/settingsd", "describe", KEY])
    return ok(st) and KEY in out, f"{st}\n{out}{err}"


QMP = None


def as_root(c, line, done, what):
    """Types `line` into the root console shell, then waits until `done(bitspkg list)` holds."""
    global QMP
    if QMP is None:
        wait_serial(r"init: started the console shell", "the console shell", 120)
        QMP = Qmp(QMP_PATH)
        QMP.type("\n")
    QMP.type(line + "\n")
    end = time.time() + 90
    while time.time() < end and not done(listed(c)):
        time.sleep(2)
    need(done(listed(c)), what, listed(c))


def install_as_root(c, pkg="pkg", version="1", previous="-"):
    if pkg == "pkg":
        run(c, ["/bin/busybox", "mkdir", "-p", W])
        put(c, W + "/pkg.tar", open(TAR, "rb").read())
        st, out, err = run(c, ["/bin/busybox", "tar", "-xf", W + "/pkg.tar", "-C", W])
        need(ok(st), "the fixture packages are unpacked on V2", f"{st}\n{out}{err}")
    want = re.compile(rf"^{NAME} current {version} previous {previous} id \d+$", re.M)
    as_root(c, f"bitspkg install {W}/{pkg}", lambda t: bool(want.search(t)), f"the fixture package {version} installed as root")
    # `list` shows the package once its version is linked; the hooks (settingsd's register) run after that, and
    # install prints its line when they are through (INT-W38: describe ran 7 ms before the register, red once).
    wait_serial(rf"installed: {NAME} {version} \(", f"bitspkg install {version} finished", 90)


def register_at_boot():
    """Boot 2's serial log: settingsd listened, then got the package's key from init's register run."""
    log = serial()
    listening = log.find("settingsd listening socket=")
    got = log.find("settingsd registered owner=bitspkg keys=1", max(listening, 0))
    done = re.search(r"init: /bin/bitspkg register \(pid \d+\) done", log)
    need(listening >= 0 and got > listening and done and got < done.start(),
         "init's bitspkg register gave settingsd the package's key after it listened", "\n".join(l for l in log.splitlines() if "settingsd" in l or "register" in l)[-3000:])


def reboot(c):
    err, t = call(c, "reboot")
    need(not err, "MCP reboot answered", t)


def main():
    c = connect(300)
    if PHASE == "data-1":
        apps_state(c, "data: stick data.img 64 MB")
        st, out, err = run(c, ["/bin/busybox", "stat", "-c", "%d %n", "/", "/data", "/apps"])
        devs = dict(reversed(l.split(" ", 1)) for l in out.splitlines() if " " in l)
        need(ok(st) and devs.get("/apps") == devs.get("/data") and devs.get("/apps") != devs.get("/"),
             "/apps lies on the data volume (the bind of /data/apps), not on the initramfs", f"{st}\n{out}{err}")
        need("init: /apps: bound to /data/apps" in serial(), "init says it bound /apps")
        need(listed(c) == "", "no package yet", listed(c))
        install_as_root(c)
        known, why = key_known(c)
        need(known, f"the install registered {KEY}", why)
        install_as_root(c, "pkg2", "2", "1")
        verify_clean(c, 1, 2)
        reboot(c)
    elif PHASE == "data-2":
        apps_state(c, "data: stick data.img 64 MB")
        need(re.search(rf"^{NAME} current 2 previous 1 id \d+$", listed(c), re.M), "after the reboot bitspkg list still shows both versions", listed(c))
        verify_clean(c, 1, 2)
        st, out, err = run(c, [BP, "space"])
        need(ok(st) and "\nvolume data: stick data.img 64 MB\n" in out, "bitspkg space names the volume", f"{st}\n{out}{err}")
        wait_serial(r"init: /bin/bitspkg register \(pid \d+\) done", "init ran bitspkg register after settingsd came up", 120)
        register_at_boot()
        known, why = key_known(c)
        need(known, f"settingsd knows {KEY} again after the boot", why)
        t = read(c, f"/apps/{NAME}/current/share/note.txt")
        need(t == "kept on /data v2\n", "the package's files read through /apps", repr(t))
        as_root(c, f"bitspkg remove {NAME}", lambda t: t == "", "the package removed as root")
        # list is empty as soon as the package is in the trash; the shell prints remove's line when it is through.
        wait_serial(rf"removed {NAME} \(", "bitspkg remove finished", 90)
        st, out, err = run(c, ["/bin/busybox", "find", "/apps", "-mindepth", "1", "-not", "-name", ".lock", "-not", "-name", ".next-id"])
        need(ok(st) and set(out.split()) == {"/apps/.store", "/apps/.trash", "/apps/.pkgs"},
             "after remove /apps holds only the empty .store, .trash and .pkgs (PK-6d)", f"{st}\n{out}{err}")
        # B2 is about the removed package's data: /data/appdata keeps only its trash and the system services' own
        # folders (SERVICE_DIRS), and nothing named after the package.
        st, out, err = run(c, ["/bin/busybox", "find", "/data/appdata", "-mindepth", "1", "-maxdepth", "1"])
        left = set(out.split()) - {"/data/appdata/.trash"} - SERVICE_DIRS
        need(ok(st) and not left and not any(NAME in p for p in out.split()),
             f"after remove no data folder of {NAME} is left in /data/appdata (B2)", f"{st}\n{out}{err}")
        verify_clean(c, 0, 0)
    elif PHASE == "tmpfs-1":
        apps_state(c, "tmpfs: no data volume (data_img_mb=0)")
        install_as_root(c)
        reboot(c)
    elif PHASE == "tmpfs-2":
        apps_state(c, "tmpfs: no data volume (data_img_mb=0)")
        need(listed(c) == "", "without a data volume the package is gone after the reboot (expected)", listed(c))
    else:
        raise SystemExit(f"RED: unknown phase {PHASE}")


main()
