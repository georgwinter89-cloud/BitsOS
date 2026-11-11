#!/usr/bin/env python3
"""TS-1: ktests.py and lo_check.py against a stand-in for the Mini-PC's MCP (plain HTTP on 127.0.0.1): the judgement,
the record file, and above all the machine put back after a red run. A step of `cargo xtask test`; nothing here touches
a machine. Usage: selftest.py <scratch dir>"""
import base64
import contextlib
import io
import json
import os
import subprocess
import sys
import tempfile
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import ktests  # noqa: E402

TOKEN = "selftest-token"
PROBE = ktests.BARE_PROBE
WHOLE = """call 1 ktest: finished after 1.0 s
ktest "": passed=2 failed=0 skipped=2 refused=1 (hardware: skip = belongs to another run)
ok a.b (1 ms)
ok dp.c (1 ms)
skip e.f: belongs to a QEMU run
skip nv.g: belongs to a QEMU run
refused i.j: Audible: makes sound; not over MCP on hardware (R8e)
"""
LO_KTEST = "ok ul.pkg.libreoffice (1 ms)\nktest: passed=3 failed=0 skipped=0 refused=0\n"
LO_CHECK = "libraries: 12 found, none missing\nlibreoffice: home: /data/home/1000 (data volume; documents stay)\nrc=0\n"
PDF = b"%PDF-1.7\n...\n%%EOF\n"
ODT = b"PK\x03\x04" + b"mimetypeapplication/vnd.oasis.opendocument.text"


class Machine:
    """What the scripts can see of the Mini-PC. The switches make it misbehave."""

    def __init__(self, **switches):
        self.desktop = True
        self.level = "warn,net=debug"
        self.config = "token=x\n"
        self.window = None
        self.calls = []
        self.during_ktest = None  # the level and the desktop while the long run was going
        self.get_fails = self.set_back_fails = self.keys_refused = self.quit_ignored = self.stays_open = False
        self.back_on_keys = True
        self.lo_tally = LO_KTEST
        self.__dict__.update(switches)

    def tool(self, name, a):
        self.calls.append((name, a))
        if name == "list_dir":
            return self.list_dir(a["path"])
        if name == "read_file":
            data = {
                "/data/home/1000/lo1-check/brief.pdf": PDF,
                "/data/home/1000/lo1-check/brief.odt": ODT,
                "/proc/105/comm": b"bitscomp\n",
                "/proc/105/status": b"Name:\tbitscomp\nState:\tS (sleeping)\n",
                "/proc/7/comm": b"oldholder\n",
                "/proc/7/status": b"Name:\toldholder\nState:\tS (sleeping)\n",
                "/proc/9/comm": b"undead\n",
                "/proc/9/status": b"Name:\tundead\nState:\tZ (zombie)\n",
            }.get(a["path"], self.config.encode())
            off = a.get("offset", 0)
            part = data[off:]
            return False, json.dumps({"data": base64.b64encode(part).decode(), "length": len(part), "eof": True})
        if name == "settings_get":
            return (True, "settingsd does not answer") if self.get_fails else (False, json.dumps({"key": a["key"], "value": self.level}))
        if name == "settings_set":
            if self.set_back_fails and a["value"] != ktests.TEST_LEVEL:
                return True, "the guard says no"
            self.level = a["value"]
            return False, json.dumps({"new": a["value"]})
        if name == "send_keys":
            if self.keys_refused:
                return True, "refused"
            if self.back_on_keys:
                self.desktop = True
            return False, "sent"
        if name == "run":
            return self.shell(a["args"][1])
        if name == "ktest":
            return self.ktest(a["filter"])
        if name == "screen":
            return False, "the picture"
        raise ValueError(f"no such tool in the stand-in: {name}")

    def list_dir(self, path):
        if path == "/proc":
            pids = ["1", "9"] + (["105"] if self.desktop else []) + (["7"] if self.stays_open else [])
            lines = [f"dr-xr-xr-x     0:0                0 {p}/" for p in pids]
            return False, "\n".join(["/proc: x entries (proc)", "lrwxrwxrwx     0:0                0 self -> 1"] + lines) + "\n"
        target = {"/proc/105/fd": "/dev/fb0", "/proc/7/fd": "/dev/input/event0"}.get(path)
        line = f"lrwx------     0:0                0 3 -> {target}" if target else "lrwx------     0:0                0 3 -> /dev/null"
        return False, f"{path}: 1 entries (proc)\n{line}\n"

    def ktest(self, flt):
        if flt == PROBE:
            line = f"skip {PROBE}: belongs to the Mini-PC run without the desktop: a program holds the screen" if self.desktop else f"ok {PROBE} (1 ms)"
            return False, f"call 1 ktest: finished\n{line}\n"
        if flt.startswith("ul.pkg.libreoffice"):
            return False, self.lo_tally
        self.during_ktest = (self.level, self.desktop)
        return False, WHOLE

    def shell(self, cmd):
        if cmd == "exec bitscomp ctl quit":
            if not self.quit_ignored:
                self.desktop = False
            return False, "ok\n"
        if "ctl address open" in cmd:
            self.window = cmd.split("/")[-1]
            return False, "opened\n"
        if "ctl list" in cmd:
            return False, f"id=1 app_id=libreoffice-{self.window} title=x\n" if self.window else "\n"
        if "ctl close" in cmd:
            self.window = None
            return False, "\n"
        if "--check" in cmd:
            return False, LO_CHECK
        if "--convert-to" in cmd:
            return False, "LibreOffice ended (code 0)\nrc=0\n"
        return False, "\n"

    def used(self, tool):
        return [a for n, a in self.calls if n == tool]


def serve(machine):
    class H(BaseHTTPRequestHandler):
        def log_message(self, *_):
            pass

        def do_POST(self):
            body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            assert self.headers["Authorization"] == f"Bearer {TOKEN}"
            p = body["params"]
            err, text = machine.tool(p["name"], p["arguments"])
            content = [{"type": "text", "text": text}]
            if p["name"] == "screen":
                content.append({"type": "image", "data": base64.b64encode(b"PNG").decode()})
            out = json.dumps({"jsonrpc": "2.0", "id": 1, "result": {"content": content, "isError": err}}).encode()
            self.send_response(200)
            self.send_header("Content-Length", str(len(out)))
            self.end_headers()
            self.wfile.write(out)

    srv = ThreadingHTTPServer(("127.0.0.1", 0), H)
    threading.Thread(target=srv.serve_forever, daemon=True).start()
    return srv


def run_ktests(machine, run, out):
    """ktests.py's main in this process -> (exit text or None for green, what it printed)."""
    srv = serve(machine)
    argv, printed = sys.argv, io.StringIO()
    sys.argv = ["ktests.py", run, f"127.0.0.1:{srv.server_address[1]}", "--out", out]
    code = None
    try:
        with contextlib.redirect_stdout(printed):
            ktests.main()
    except SystemExit as e:
        code = str(e.code)
    finally:
        sys.argv = argv
        srv.shutdown()
    return code, printed.getvalue()


def check(cond, what, detail=""):
    if not cond:
        raise SystemExit(f"RED: {what}\n{detail}")
    print(f"ok: {what}", flush=True)


def judge_and_record():
    text = """ktest "": passed=1 failed=1 skipped=2 refused=1 (hardware: skip = belongs to another run)
ok a.b (1 ms)
FAIL c.d: no device
skip e.f: belongs to a QEMU run
skip g.h: belongs to the Mini-PC run without the desktop: a program holds the screen
refused i.j: Audible: makes sound; not over MCP on hardware (R8e)
"""
    r = ktests.parse(text)
    check([n for n, _ in r["ok"]] == ["a.b"] and [n for n, _ in r["FAIL"]] == ["c.d"], "the answer's lines are read")
    check(ktests.judge("desktop", r) == ["FAIL c.d: no device"], "desktop: a Screen test of the other run is no problem")
    check(any("still held" in b for b in ktests.judge("bare", r)), "bare: the same line is red")
    r = ktests.parse("skip k.l: no device\nrefused m.n: something else\n")
    check(len(ktests.judge("desktop", r)) == 2, "a test skipping itself and an unknown refusal are red")
    ktests.RAN, ran = tempfile.mktemp(), ktests.RAN
    try:
        with contextlib.redirect_stdout(io.StringIO()):
            ktests.record("desktop", ["a.b", "c.d"], "85")
            ktests.record("desktop", ["a.b"], "86")
        with open(ktests.RAN) as f:
            lines = f.read().split("\n")
        check([l.split()[0] for l in lines if l] == ["c.d", "a.b"] and lines[1].endswith(" 86"), "a test recorded again keeps one line", lines)
    finally:
        os.unlink(ktests.RAN)
        ktests.RAN = ran


def machine_put_back(out):
    old = "warn,net=debug"
    m = Machine()
    code, text = run_ktests(m, "desktop", out)
    check(code is None and "GREEN" in text, "desktop run green", text)
    check(m.during_ktest == (ktests.TEST_LEVEL, True) and m.level == old, "desktop: info during the run, the machine's own value afterwards, also without a line in CONFIG.TXT", text)
    check(not [a for a in m.used("run") if "ctl quit" in a["args"][1]] and not m.used("send_keys"), "desktop: the desktop is left alone")
    with open(os.path.join(out, "ktest-desktop.txt")) as f:
        check("ok a.b" in f.read(), "the whole answer is in the file")

    m = Machine()
    code, text = run_ktests(m, "bare", out)
    check(code is None and "GREEN" in text, "bare run green", text)
    check(m.during_ktest == (ktests.TEST_LEVEL, False), "bare: the long run sees no desktop", text)
    check(m.desktop and m.level == old and len(m.used("send_keys")) == 1, "bare: desktop and level are back", text)

    m = Machine(keys_refused=True)
    code, text = run_ktests(m, "bare", out)
    check(code and "RED" in code and "send_keys refused" in text and m.level == old, "bare: send_keys refused is RED, not a warning; the level still goes back", text)
    check("ok a.b" in open(os.path.join(out, "ktest-bare.txt")).read(), "and the results are kept")

    m = Machine(back_on_keys=False)
    code, text = run_ktests(m, "bare", out)
    check(code and "did not come back" in text and m.level == old, "bare: a desktop that does not come back is RED", text)

    m = Machine(quit_ignored=True)
    code, text = run_ktests(m, "bare", out)
    check(code and "still holds the screen" in code and m.during_ktest is None, "bare: a desktop that will not quit is RED, the run does not start", text)
    check(not m.used("send_keys") and m.level == old, "bare: and nothing is typed into it; the level goes back", text)

    m = Machine(stays_open=True)
    code, text = run_ktests(m, "bare", out)
    check(code and "oldholder" in code and m.during_ktest is None, "bare: a process left holding the devices is RED, by name", text)
    check(m.desktop and m.level == old, "bare: the desktop is started again all the same", text)

    m = Machine(set_back_fails=True)
    code, text = run_ktests(m, "bare", out)
    check(code and "the log level" in text and m.desktop, "the level refused at the end: RED, the desktop is back", text)
    check("ok a.b" in open(os.path.join(out, "ktest-bare.txt")).read(), "and the results are kept")

    m = Machine(get_fails=True, config="loglevel=warn\n")
    code, text = run_ktests(m, "desktop", out)
    check(code is None and "WARNING" in text and m.level == "warn", "no settings_get: CONFIG.TXT's level, with a warning", text)
    m = Machine(get_fails=True)
    code, text = run_ktests(m, "desktop", out)
    check(code is None and m.level == ktests.NORMAL_LEVEL, "no settings_get and no line: the normal level", text)

    m = Machine(desktop=False)
    code, text = run_ktests(m, "bare", out)
    check(code is None and not m.used("send_keys") and not m.desktop, "bare in a boot with desktop=off: nothing is stopped, nothing started", text)
    check(not [a for a in m.used("run") if "ctl quit" in a["args"][1]], "and no quit goes out")

    for f in ktests.FORBIDDEN:
        m = Machine(config=f + "\n")
        code, text = run_ktests(m, "bare", out)
        check(code and f in code and not m.used("settings_set") and not m.used("ktest"), f"a CONFIG with {f} stops the script before it changes anything", text)


def lo_check(out):
    def go(m):
        srv = serve(m)
        env = dict(os.environ, BITSOS_TOKEN=TOKEN)
        cmd = [sys.executable, "-I", os.path.join(HERE, "lo_check.py"), f"127.0.0.1:{srv.server_address[1]}", os.path.join(out, "lo"), "--desktop"]
        try:
            return subprocess.run(cmd, env=env, capture_output=True, text=True, timeout=180, check=False)
        finally:
            srv.shutdown()

    p = go(Machine())
    check(p.returncode == 0 and "GREEN" in p.stdout, "lo_check.py green with windows and a picture", p.stdout + p.stderr)
    check(os.path.exists(os.path.join(out, "lo", "writer.png")), "its picture is saved")
    p = go(Machine(lo_tally="ktest: passed=2 failed=1 skipped=0 refused=0\n"))
    check(p.returncode != 0 and "ul.pkg.libreoffice" in p.stderr, "lo_check.py red when the ktest fails", p.stdout + p.stderr)


def main():
    if len(sys.argv) != 2:
        raise SystemExit(__doc__)
    out = sys.argv[1]
    os.makedirs(out, exist_ok=True)
    os.environ["BITSOS_TOKEN"] = TOKEN
    ktests.STOP_WAIT_S, ktests.POLL_S = 1, 0.05
    judge_and_record()
    machine_put_back(out)
    lo_check(out)
    print("GREEN: the Mini-PC scripts against the stand-in", flush=True)


if __name__ == "__main__":
    main()
