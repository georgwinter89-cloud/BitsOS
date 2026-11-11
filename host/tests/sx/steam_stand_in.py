#!/usr/bin/env python3
"""GM-2a: host/tests/minipc/steam_check.py against a stand-in for the machine (selftest.py's server): a run that reaches its
window, runs that fail at each kind of stage and must say why, and the pure parts. Usage: steam_stand_in.py <out dir>"""
import os
import re
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
MINIPC = os.path.join(HERE, "..", "minipc")
sys.path.insert(0, MINIPC)
sys.argv, ARGV = ["steam_check.py", "x:1", "/nonexistent", "--token-file", "/dev/null"], sys.argv
os.environ["BITSOS_TOKEN"] = "selftest-token"
import selftest  # noqa: E402

CHECK_OK = "steam: tree ok\nrc=0\n"


def status(stage, procs=None, windows=()):
    lines = [f"steam: last stage: 1 stage={stage}"]
    for n in "steam srt-client-container pressure-vessel-wrap srt-bwrap pv-adverb steamwebhelper".split():
        lines.append(f"steam: process {n}: {(procs or {}).get(n, '')}")
    lines += [f"steam: window: {w}" for w in windows]
    return "\n".join(lines) + "\n"


class Machine:
    def __init__(self, check=CHECK_OK, script=(), logs=None, enosys=""):
        self.check, self.script, self.logs, self.enosys = check, list(script), logs or {}, enosys
        self.calls = []

    def tool(self, name, a):
        self.calls.append((name, a))
        if name == "screen":
            return False, "picture"
        if name != "run":
            return True, f"unexpected tool {name}"
        cmd = a["args"][1]
        if "--check" in cmd:
            return False, self.check
        if "--status" in cmd:
            return False, self.script.pop(0) if len(self.script) > 1 else self.script[0]
        if "--stop" in cmd:
            return False, "steam: stopped\nrc=0\n"
        if "setsid" in cmd:
            return False, "started\n"
        if "enosys" in cmd:
            return (False, self.enosys) if self.enosys else (True, "")
        if m := re.search(r"tail -n 400 ([^\s;]+)", cmd):
            f = self.logs.get(os.path.basename(m.group(1)))
            return False, f if f is not None else "no-such-log\n"
        return True, f"unexpected command {cmd}"

    def starts(self):
        return [a["args"][1] for n, a in self.calls if n == "run" and "setsid" in a["args"][1]]


def go(m, out, *extra, secs="2"):
    srv = selftest.serve(m)
    env = dict(os.environ, BITSOS_TOKEN=selftest.TOKEN, STEAM_CHECK_POLL="0.05", STEAM_CHECK_PAINT="0")
    cmd = [sys.executable, "-I", os.path.join(MINIPC, "steam_check.py"), f"127.0.0.1:{srv.server_address[1]}", out, "--secs", secs, *extra]
    try:
        return subprocess.run(cmd, env=env, capture_output=True, text=True, timeout=60, check=False)
    finally:
        srv.shutdown()


def check(cond, what, p=None):
    if not cond:
        raise SystemExit(f"RED: {what}\n{(p.stdout + p.stderr) if p else ''}")
    print(f"ok: {what}", flush=True)


def pure():
    sys.argv = ["steam_check.py", "x:1", "/nonexistent"]
    sys.path.insert(0, MINIPC)
    import steam_check as c

    last, procs, win = c.parse_status(status("steam", {"steam": "5 6"}, ["app_id=steam x11=0x1"]))
    check(last == "1 stage=steam" and procs["steam"] == ["5", "6"] and win == ["app_id=steam x11=0x1"], "parse_status reads the launcher's lines")
    check(c.reached("", {}, []) == "nothing", "reached: nothing without a stage")
    check(c.reached("1 stage=install seeding", {}, []) == "install", "reached: the launcher's stage")
    check(c.reached("1 stage=steam", {"steam": ["1"]}, []) == "client", "reached: the client's process")
    check(c.reached("1 stage=steam", {"steam": ["1"], "srt-bwrap": ["2"]}, []) == "container", "reached: the container")
    check(c.reached("1 stage=steam", {"steamwebhelper": ["3"]}, []) == "webhelper", "reached: the web helper")
    check(c.reached("1 stage=steam", {"steam": ["1"]}, ["w"]) == "update-window", "reached: a window without a web helper is the update window")
    check(c.reached("1 stage=steam", {"steamwebhelper": ["1"]}, ["w"]) == "window", "reached: a window with the web helper")
    rs = c.reasons("/usr/bin/env: /bin/bash: No such file\nbwrap: Can't mount proc\nld: error while loading shared libraries: libfoo.so.1: cannot open\n")
    check(any("/bin/bash" in r for r in rs) and any("bubblewrap" in r for r in rs) and any("libfoo" in r for r in rs), "reasons names each kind with its line")
    check(c.reasons("all fine\n") == [], "reasons is empty for a clean log")
    check(c.size_note("tile=0 visible=true frame=0,0 1280x1440 id=3 app_id=steam x11=0x1") == "size 1280x1440", "size_note reads the frame")
    check("not a failure" in c.size_note("float=0 frame=-4,10 300x900 x11=0x1"), "size_note: a small window is named, not failed")
    check(c.size_note("app_id=steam x11=0x1") == "size unknown", "size_note: no frame in the line")


def main():
    out = ARGV[1]
    os.makedirs(out, exist_ok=True)
    pure()

    d = os.path.join(out, "green")
    m = Machine(
        script=[
            status("start"),
            status("install seeding"),
            status("steam", {"steam": "7"}),
            status("steam", {"steam": "7"}, ["app_id=steam x11=0x1"]),
            status("steam", {"steam": "7", "srt-bwrap": "8"}),
            status("steam", {"steam": "7", "steamwebhelper": "9"}, ["app_id=steam x11=0x2"]),
        ],
        logs={"steam.log": "1 stage=start\n"},
    )
    p = go(m, d)
    check(p.returncode == 0 and "GREEN" in p.stdout, "a run that reaches the window is green", p)
    check(os.path.exists(f"{d}/login.png") and os.path.exists(f"{d}/window-1.png"), "both pictures are saved", p)
    check(all(st in p.stdout for st in ("stage: start", "stage: install", "stage: client", "stage: update-window", "stage: window")), "it prints every stage it passes", p)
    check("SB_VERBOSE=1 setsid /boot/BITSOS2/bin/steam" in m.starts()[0] and not [c for c in m.calls if c[0] in ("input", "type")], "the launcher starts verbose, in the background; nothing is typed", p)
    check(not [c for c in m.calls if c[0] == "run" and "--stop" in c[1]["args"][1]], "Steam stays up for Georg's login without --stop", p)
    m = Machine(script=[status("steam", {"steamwebhelper": "9"}, ["app_id=steam x11=0x2"])], logs={"steam.log": ""})
    p = go(m, os.path.join(out, "stop"), "--stop", "--steam-args", "-cef-disable-sandbox")
    check(p.returncode == 0 and "steam: stopped" in p.stdout and "-cef-disable-sandbox" in m.starts()[0], "--stop ends it at the end, --steam-args reach the launcher", p)

    m = Machine(check="steam: running: launcher 5, 1 stage=steam\nrc=0\n", script=[status("steam", {"steamwebhelper": "9"}, ["float=0 frame=0,0 300x200 app_id=steam x11=0x2"])], logs={"steam.log": ""})
    p = go(m, os.path.join(out, "follow"))
    check(p.returncode == 0 and "already runs" in p.stdout and not m.starts(), "a launcher that already runs is followed, none is started", p)
    check("window size 300x200 (smaller than 600x400" in p.stdout, "the window's size is in the report; small is green", p)

    p = go(Machine(check="steam: /bin/bash is missing\nrc=1\n", logs={}), os.path.join(out, "check"))
    check(p.returncode == 1 and "RED: steam --check" in p.stdout and "/bin/bash is missing" in p.stdout and "stage reached: nothing" in p.stdout, "a failing --check stops the run and says what", p)

    log = "1 stage=start\n1 stage=steam\nbwrap: Can't mount proc on /newroot/proc: Operation not permitted\n" + "\n".join(f"line {i}" for i in range(60)) + "\n"
    m = Machine(script=[status("steam", {"steam": "7", "srt-bwrap": "8"}), status("ended code 1")], logs={"steam.log": log}, enosys=" 101 ptrace calls=0 errors=3 ENOSYS\n")
    d = os.path.join(out, "ended")
    p = go(m, d)
    check(p.returncode == 1 and "stage reached: container" in p.stdout and "the launcher ended" in p.stdout, "an ended launcher is red with the stage reached", p)
    check("why: bubblewrap stopped" in p.stdout and "kernel:  101 ptrace" not in p.stdout and "kernel: 101 ptrace calls=0 errors=3 ENOSYS" in p.stdout, "it says why and names the ENOSYS calls", p)
    check("line 59" in p.stdout and "line 19" not in p.stdout, "it shows the last 40 lines of the log", p)
    check(os.path.exists(f"{d}/steam.log"), "the log is saved", p)

    p = go(Machine(script=[status("data-failed data: /data is vfat")], logs={"steam.log": ""}), os.path.join(out, "failed"))
    check(p.returncode == 1 and "data-failed data: /data is vfat" in p.stdout, "a stage that failed is red at once with its reason", p)

    m = Machine(script=[status("steam", {"steam": "7"})], logs={"steam.log": "1 stage=steam\nstill downloading\n"})
    p = go(m, os.path.join(out, "timeout"), secs="1")
    check(p.returncode == 1 and "no window of Steam within 1 s" in p.stdout and "stage reached: client" in p.stdout and "still downloading" in p.stdout, "no window in time is red with the stage and the log", p)
    print("GREEN: steam_check.py against the stand-in", flush=True)


if __name__ == "__main__":
    main()
