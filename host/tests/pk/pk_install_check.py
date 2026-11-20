#!/usr/bin/env python3
"""PK-6d on V2 through MCP (QEMU hostfwd) and the root console shell (QMP keys): programs from the fixture mirror.
Usage: pk_install_check.py <host port> <token> <qmp socket> <serial log> <http port>. Exit 0 = green.
The mirror is the host's 127.0.0.1:<http port>, the guest's 10.0.2.2:<http port>. Root installs (typed into the
console shell); uid 1000 (MCP) runs jq through bitsrun and foot's shell in foot's view (`bitsrun --exec`; QEMU has no
desktop for foot's window). No font is placed by this test: foot's closure brings none, its view shows the system's
fonts (PK-FONTS)."""
import base64
import http.client
import json
import os
import re
import socket
import sys
import time

sys.dont_write_bytecode = True
PORT, TOKEN, QMP_PATH, LOG, HTTP = int(sys.argv[1]), sys.argv[2], sys.argv[3], sys.argv[4], int(sys.argv[5])
BP, BR, BB = "/bin/bitspkg", "/bin/bitsrun", "/bin/busybox"
W = "/tmp/pkin"
R = W + "/root"
M = f"http://10.0.2.2:{HTTP}"
SYSFONTS = "/usr/share/fonts"
ENV = {"PATH": "/bin:/usr/bin", "HOME": "/data/home/1000", "XDG_RUNTIME_DIR": "/run/user/1000", "LANG": "de_DE.UTF-8"}
next_id = 0


def need(cond, what, detail=""):
    if not cond:
        raise SystemExit(f"RED: {what}\n{str(detail)[:4000]}")
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
            rpc(c, "initialize", {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "pk6a", "version": "1"}})
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
    for off in range(0, max(len(data), 1), step):
        err, t = call(c, "write_file", {"path": path, "data": base64.b64encode(data[off:off + step]).decode(), "offset": off})
        if err:
            raise SystemExit(f"RED: write_file {path}: {t}")


def run(c, argv, timeout_ms=60000, env=None):
    args = {"path": argv[0], "args": argv[1:], "timeout_ms": timeout_ms, "kill_after_ms": timeout_ms}
    if env is not None:
        args["env"] = env
    err, t = call(c, "run", args)
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
        if rx.search(serial()):
            return
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


def sources(extra=""):
    out = extra
    for n in ["fixture", "flip", "badpkg"]:
        out += f'[[source]]\nname = "{n}"\nservers = ["{M}/main/$repo"]\nsig_db = "optional"\nkeyring = "archlinux"\n\n'
    return out


# Root's part, typed into the console shell: every command's output into out/<name>.txt with its exit status.
GO = f"""#!/bin/sh
B={BP}
o() {{ n=$1; shift; "$@" > {W}/out/$n.txt 2>&1; echo "exit $?" >> {W}/out/$n.txt; }}
mkdir -p {W}/out
for r in {R} {W}/root2 {W}/root3; do mkdir -p $r/etc/bitspkg/keyrings; cp {W}/keyrings/* $r/etc/bitspkg/keyrings/; done
o refresh $B --root {R} --settings none refresh --sources {W}/src.toml
o gc $B --root {R} --settings none gc
o before $B snapshot {R}
o jq $B --root {R} --settings none install jq --sources {W}/src.toml
o foot $B --root {R} --settings none install foot --sources {W}/src.toml
o scripts $B --root {R} --settings none info --scripts foot
o verify1 $B --root {R} --settings none verify --store
o refresh2 $B --root {W}/root2 --settings none refresh --sources {W}/src.toml
o flip $B --root {W}/root2 --settings none install flip/jq --sources {W}/src.toml
o badpkg $B --root {W}/root2 --settings none install badpkg/jq --sources {W}/src.toml
o snap2 $B snapshot {W}/root2/apps
o refresh3 $B --root {W}/root3 --settings none refresh --sources {W}/tiny.toml
o tiny $B --root {W}/root3 --settings none install jq --sources {W}/tiny.toml
o snap3 $B snapshot {W}/root3/data
id > {W}/out/id.txt
chmod -R a+rX {W}/out
echo done > {W}/out/done
"""

GO2 = f"""#!/bin/sh
B={BP}
o() {{ n=$1; shift; "$@" > {W}/out2/$n.txt 2>&1; echo "exit $?" >> {W}/out2/$n.txt; }}
mkdir -p {W}/out2
o rmfoot $B --root {R} --settings none remove foot
o rmjq $B --root {R} --settings none remove jq
o verify2 $B --root {R} --settings none verify --store
o after $B snapshot {R}
chmod -R a+rX {W}/out2
echo done > {W}/out2/done
"""


def root_phase(q, c, script, out):
    q.type(f"sh {W}/{script}\n")
    end = time.time() + 900
    while time.time() < end and read(c, f"{W}/{out}/done") is None:
        time.sleep(3)
    need(read(c, f"{W}/{out}/done") is not None, f"the root run {script} finished", "\n".join(serial().splitlines()[-40:]))



def snap_lines(t):
    return [l for l in t.splitlines() if l and not l.startswith("exit ") and not l.startswith("apps/.next-id ") and not l.startswith("apps/.lock ")]


def main():
    c = connect(300)
    st, out, err = run(c, [BB, "mkdir", "-p", W + "/keyrings"])
    need(ok(st), "scratch folder", f"{st}{out}{err}")
    for f in sorted(os.listdir("/usr/share/pacman/keyrings")):
        put(c, f"{W}/keyrings/{f}", open(f"/usr/share/pacman/keyrings/{f}", "rb").read())
    put(c, W + "/src.toml", sources().encode())
    put(c, W + "/tiny.toml", sources("space_max_gb = 0\n").encode())
    put(c, W + "/go", GO.encode())
    put(c, W + "/go2", GO2.encode())

    wait_serial(r"init: started the console shell", "the console shell", 120)
    q = Qmp(QMP_PATH)
    q.type("\n")
    root_phase(q, c, "go", "out")
    o = {n: read(c, f"{W}/out/{n}.txt") or "" for n in ["refresh", "gc", "before", "jq", "foot", "scripts", "verify1", "flip", "badpkg", "snap2", "tiny", "snap3", "id"]}
    need("uid=0" in o["id"], "the run was root's", o["id"])
    need(o["refresh"].rstrip().endswith("exit 0") and "fixture: aktualisiert, 38 Pakete" in o["refresh"], "refresh from the fixture mirror", o["refresh"])
    jq = o["jq"]
    need(jq.rstrip().endswith("exit 0") and "Installiert: jq 1.8.2-1.1+1" in jq, "jq installed from the fixture mirror as root", jq)
    need("Geprüft: sha256 und Signatur von 7 Paketen" in jq and "Hook 11-glibc-ldconfig: emuliert (ldconfig): root/etc/ld.so.cache" in jq,
         "jq: sha256 and signatures checked, ldconfig emulated in the staging view", jq)
    need("Hook 10-glibc-locale-gen: emuliert (localedef)" in jq, "jq: de_DE.UTF-8 and en_US.UTF-8 made by the closure's localedef", jq)
    need("event=pkg.download.done" in jq and "event=pkg.trigger.emulated" in jq, "events pkg.download.done and pkg.trigger.emulated", jq)
    ft = o["foot"]
    need(ft.rstrip().endswith("exit 0") and "Installiert: foot 1.28.0-2.1+1" in ft and "Pakete: 36 (31 neu entpackt, 5 schon in /apps/.pkgs)" in ft,
         "foot installed: 36 packages, the 5 jq brought taken from /apps/.pkgs", ft)
    need("Hook 40-fontconfig-config: emuliert: " in ft and "Hook fontconfig: später" in ft and "Starter: Foot (foot;" in ft,
         "foot: fontconfig's conf.d links made, fc-cache left to the first start, the launcher listed", ft)
    need("Installationsskript von fontconfig (nie ausgeführt" in ft, "foot: fontconfig's .INSTALL kept and reported, never run", ft)
    need("== bash 5.3.20-2" in o["scripts"] and "| post_upgrade() {" in o["scripts"], "info --scripts foot shows bash's .INSTALL", o["scripts"])
    need(o["verify1"].startswith("verify: all good (2 packages, 2 versions"), "verify --store clean with jq and foot", o["verify1"])
    need("falscher Inhalt" in o["flip"] and o["flip"].rstrip().endswith("exit 1"), "a package with a flipped byte is refused (sha256)", o["flip"])
    need("Signatur vom Spiegel abgelehnt" in o["badpkg"] and o["badpkg"].rstrip().endswith("exit 1"), "a package with a wrong signature is refused", o["badpkg"])
    bad = [l for l in snap_lines(o["snap2"]) if not re.match(r"^(\.store|\.trash|\.pkgs) dir |^\.lock file ", l)]
    need(not bad, "the refused installs left nothing in /apps (no program, no .pkgs entry, no store file)", "\n".join(bad))
    need("Programme dürfen höchstens 0 B belegen (pkg.space.max_gb)" in o["tiny"] and "nichts wurde heruntergeladen" in o["tiny"],
         "space_max_gb = 0: refused with the numbers before any download", o["tiny"])
    need(not any("bitspkg/cache/" in l for l in o["snap3"].splitlines()), "the space refusal downloaded nothing", o["snap3"])

    # uid 1000: jq through bitsrun.
    st, out, err = run(c, [BR, "--store", R, "jq", "-n", "[1+1, \"äö\"]"])
    need(ok(st) and "2" in out and "äö" in out, "jq from /apps runs through bitsrun as uid 1000", f"{st}\n{out}{err}")
    st, out, err = run(c, [BR, "--store", R, "--plan", "jq"])
    need(ok(st) and f"ro-bind {R}/apps/jq/1.8.2-1.1+1/root/usr -> /usr" in out, "jq's view binds its own root tree's usr", out)

    # foot: its closure has fontconfig and no font; the view shows the system's fonts (PK-FONTS). Nothing is placed.
    data = f"{R}/data/appdata/foot"
    seen = f"{ENV['HOME']}/.local/share/fonts"
    st, out, err = run(c, [BB, "find", SYSFONTS])
    sysfonts = sorted(l for l in out.splitlines() if l.lower().endswith((".ttf", ".otf", ".ttc")))
    need(ok(st) and sysfonts, f"the image has fonts in {SYSFONTS} (without one this scenario cannot do its work)", f"{st}\n{out}{err}")
    st, out, err = run(c, [BB, "find", f"{R}/apps/foot", "-name", "*.ttf", "-o", "-name", "*.otf", "-o", "-name", "*.ttc", "-o", "-name", "*.pcf.gz"])
    need(ok(st) and not out.strip(), "foot's closure brings no font of its own", f"{st}\n{out}{err}")
    st, out, err = run(c, [BB, "ls", "-A", data])
    need(ok(st) and not out.strip(), "foot's data folder is empty before its first start (no font, no fonts.conf placed)", f"{st}\n{out}{err}")
    st, out, err = run(c, [BR, "--store", R, "--plan", "foot"])
    need(ok(st) and f"ro-bind {SYSFONTS} -> {seen}" in out, "foot's view binds the system's fonts read-only below its home", out)
    # foot's own shell (bash of its closure) in foot's view, through `bitsrun --exec`: the view a started foot has.
    # QEMU has no desktop (Georg 18.11.: it draws only on the GPU), so foot's window itself is the Mini-PC's.
    shell = ("locale > /data/appdata/foot/locale.txt 2>&1; "
             "LC_ALL=C sh -c ': > \"$HOME/.local/share/fonts/pk-probe\"' > /data/appdata/foot/fontwrite.txt 2>&1; "
             "echo \"status $?\" >> /data/appdata/foot/fontwrite.txt; "
             "while read -r l; do case \"$l\" in *\" $HOME/.local/share/fonts \"*) echo \"$l\";; esac; done "
             "< /proc/self/mountinfo > /data/appdata/foot/fontmount.txt 2>&1; "
             "echo foot-ok $LANG > /data/appdata/foot/out.txt")
    st, out, err = run(c, [BR, "--store", R, "--exec", "bash", "foot", "-c", shell], env=ENV)
    need(ok(st), "bash in foot's view runs through bitsrun --exec as uid 1000", f"{st}\n{out}{err}")
    need((read(c, f"{data}/out.txt") or "").strip() == "foot-ok de_DE.UTF-8", "foot's shell (bash of the closure) ran in the view", f"{st}\n{out}{err}")
    loc = read(c, f"{data}/locale.txt") or ""
    need("LANG=de_DE.UTF-8" in loc and "Cannot" not in loc, "de_DE.UTF-8 works in foot's shell (the emulated locale-gen)", loc)
    fw = read(c, f"{data}/fontwrite.txt") or ""
    st, out, err = run(c, [BB, "find", SYSFONTS])
    # V2 answers EACCES here where Linux answers EROFS (crates/vfs ops.rs asks may_create before the mount's
    # read-only flag: open item in plan/notes/desk-offen.md, group G3), so the mount's own line proves the read-only bind.
    need(("Read-only file system" in fw or "Permission denied" in fw) and re.search(r"^status [1-9]", fw, re.M) and "pk-probe" not in out
         and sorted(l for l in out.splitlines() if l.lower().endswith((".ttf", ".otf", ".ttc"))) == sysfonts,
         "the view cannot write the system's fonts, the folder is as before", f"{fw}\n{out}")
    fm = [l.split() for l in (read(c, f"{data}/fontmount.txt") or "").splitlines() if l.strip()]
    need(len(fm) == 1 and len(fm[0]) > 5 and fm[0][3].endswith("/fonts") and fm[0][4] == seen and "ro" in fm[0][5].split(","),
         "in the view the system's font folder is one read-only mount (its mountinfo line)", fm)
    st, out, err = run(c, [BB, "find", data, "-name", "*.ttf", "-o", "-name", "fonts.conf", "-o", "-name", "fonts"])
    need(ok(st) and not out.strip(), "nothing was copied per program: no font and no fonts.conf in foot's data folder", f"{st}\n{out}{err}")

    # Removal: the scratch root is exactly as before the installs (B2).
    root_phase(q, c, "go2", "out2")
    p = {n: read(c, f"{W}/out2/{n}.txt") or "" for n in ["rmfoot", "rmjq", "verify2", "after"]}
    need(p["rmfoot"].rstrip().endswith("exit 0") and p["rmjq"].rstrip().endswith("exit 0"), "foot and jq removed", p["rmfoot"] + p["rmjq"])
    need(p["verify2"].startswith("verify: all good (0 packages, 0 versions"), "verify --store clean after the removal", p["verify2"])
    before, after = snap_lines(o["before"]), snap_lines(p["after"])
    diff = sorted(set(before) ^ set(after))
    need(before and not diff, "after removal the root tree is the tree before the installs (store, .pkgs, data folders, cache)", "\n".join(diff[:40]))


main()
