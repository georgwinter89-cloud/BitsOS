#!/usr/bin/env python3
"""GM-2a Mini-PC run: Steam's client from the stick up to the login window, over MCP.

Usage: steam_check.py <host:port> <out dir> [--tls DIR|auto] [--token-file F] [--secs N] [--steam-args "A B"] [--stop] [--ktest] [--sound]
Steps: `steam --check` as MCP's user; the launcher in the background (verbose, its own log file), unless one already runs, which
is then followed; `steam --status` every 10 s;
a picture and its size when an X11 window appears (window-<n>.png, the update window comes first), the last picture once steamwebhelper
draws too (login.png: a person looks at it). Exit 0 = a window of Steam with its web helper was on the screen. No window: RED with
the stage reached, the reasons found in the logs, the system calls the kernel answered ENOSYS, and the last 40 lines of each log.
Steam keeps running afterwards for Georg's login unless --stop. Nothing here types anything or knows a credential."""
import os
import re
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import mp  # noqa: E402

LAUNCHER = "/boot/BITSOS2/bin/steam"
ENV = {"PATH": "/bin:/usr/bin:/boot/BITSOS2/bin", "XDG_RUNTIME_DIR": "/run/user/1000"}
LOG = "/run/user/1000/steam/steam.log"
STEAM_LOGS = "/data/home/1000/.local/share/Steam/logs"
MIN_W, MIN_H = 600, 400
POLL = float(os.environ.get("STEAM_CHECK_POLL", "10"))
PAINT = float(os.environ.get("STEAM_CHECK_PAINT", "20"))
# From nothing to the login window, in the order they come. The launcher's own stages, then what its processes show.
ORDER = ["nothing", "start", "xwayland", "data", "install", "provider", "reaper", "steam", "client", "container", "webhelper", "update-window", "window"]
# (pattern in the logs, what it means)
WHY = [
    (r"/bin/bash.*(No such file|not found|missing)", "/bin/bash is not on the image (manifest: initrd bash host:/usr/bin/bash)"),
    (r"no-symlinks", "the host's realpath is not the tree's bin/realpath (is PATH reaching pv-run.sh?)"),
    (r"Exec format error|cannot execute binary", "a 32-bit program was started: V2 runs none yet (AB-10a/b)"),
    (r"[Cc]ould not resolve|couldn't resolve|Temporary failure in name|Name or service not known", "no name lookup from Steam (DNS)"),
    (r"certificate|SSL_|TLS", "TLS trouble towards Valve's CDN"),
    (r"No space left", "the data volume is full"),
    (r"[Cc]annot open display|Unable to open display|Failed to connect to.*X|Check your DISPLAY", "no X display reached (Xwayland, /tmp/.X11-unix in the container)"),
    (r"does not meet steamrt requirements", "steam-runtime-check-requirements refused this machine"),
    (r"error while loading shared libraries: \S+", "a library is missing where the program runs"),
    (r"^bwrap: .*|srt-bwrap.*: .*", "bubblewrap stopped (mount, namespace or /proc/self/mountinfo)"),
    (r"pressure-vessel-wrap.*(E:|[Ee]rror).*|pv-adverb.*(E:|[Ee]rror).*", "pressure-vessel stopped"),
    (r"Function not implemented", "a system call answered ENOSYS and the program stopped on it"),
    (r"Segmentation fault|SIGSEGV|core dumped|Illegal instruction", "a program crashed"),
    (r"Failed to (load|verify|unzip).*|Corrupt package.*", "the bootstrapper could not install the client"),
    (r"Failed to start steamrt|Cannot be run inside", "the bootstrapper could not start the container"),
]


def parse_status(text):
    """`steam --status` -> (last launcher stage line, {program: pids}, [window lines])"""
    last, procs, windows = "", {}, []
    for line in text.splitlines():
        if m := re.match(r"steam: last stage: (.*)", line):
            last = m.group(1).strip()
        elif m := re.match(r"steam: process (\S+): (.*)", line):
            procs[m.group(1)] = m.group(2).split()
        elif m := re.match(r"steam: window: (.*)", line):
            windows.append(m.group(1))
    return last, procs, windows


def reached(last, procs, windows):
    """The furthest stage this status shows."""
    stage = "nothing"
    if m := re.search(r"stage=([a-z-]+)", last):
        name = m.group(1)
        stage = name if name in ORDER else stage
    if procs.get("steam"):
        stage = max(stage, "client", key=ORDER.index)
    if procs.get("srt-bwrap") or procs.get("pressure-vessel-wrap") or procs.get("pv-adverb"):
        stage = max(stage, "container", key=ORDER.index)
    if procs.get("steamwebhelper"):
        stage = max(stage, "webhelper", key=ORDER.index)
    if windows:
        stage = max(stage, "window" if procs.get("steamwebhelper") else "update-window", key=ORDER.index)
    return stage


def size_note(window):
    """The size in a window line of `bitscomp ctl list`, for the report. A small one is no failure: X11 windows tile here (WY-3)."""
    m = re.search(r"frame=-?\d+,-?\d+ (\d+)x(\d+)", window)
    if not m:
        return "size unknown"
    w, h = int(m.group(1)), int(m.group(2))
    small = f" (smaller than {MIN_W}x{MIN_H}: tiled or a dialog; not a failure, look at the picture)" if w < MIN_W or h < MIN_H else ""
    return f"size {w}x{h}{small}"


def reasons(text):
    """What the logs say went wrong: each kind once, with its first line, the latest kinds last."""
    out = []
    for pat, why in WHY:
        for line in text.splitlines():
            if re.search(pat, line):
                out.append(f"{why}: {line.strip()[:200]}")
                break
    return out


class Run:
    def __init__(self, argv):
        self.o = mp.options(argv, positional=2, flags=("--stop", "--ktest", "--sound"), values=("--secs", "--steam-args"))
        self.out = self.o.rest[1]
        self.secs = int(self.o.values["--secs"] or 1800)
        self.pics = 0
        self.best = "nothing"

    def call(self, name, args=None):
        return mp.call(self.o, name, args)

    def sh(self, cmd, ms=120000):
        err, text, _ = self.call("run", {"path": "/bin/sh", "args": ["-c", cmd], "env": ENV if self.o.flags["--sound"] else {**ENV, "STEAM_SILENT": "1"}, "timeout_ms": ms})
        return err, text

    def picture(self, name):
        err, text, imgs = self.call("screen", {})
        if err or not imgs:
            print(f"note: no picture ({text[:200]})", flush=True)
            return
        with open(os.path.join(self.out, name), "wb") as f:
            f.write(imgs[0])
        print(f"picture: {os.path.join(self.out, name)}", flush=True)

    def logs(self):
        """(whole text the reasons are looked for in, the report's last 40 lines of each log)"""
        whole, shown = "", []
        files = [LOG] + [f"{STEAM_LOGS}/{n}" for n in ("bootstrap_log.txt", "stderr.txt", "console-linux.txt", "webhelper-linux.txt")]
        for f in files:
            err, text = self.sh(f"if [ -e {f} ]; then tail -n 400 {f}; else echo no-such-log; fi")
            if err or "no-such-log" in text:
                continue
            whole += text + "\n"
            tail = "\n".join(text.splitlines()[-40:])
            shown.append(f"--- last 40 lines of {f}\n{tail}")
            with open(os.path.join(self.out, os.path.basename(f)), "w") as w:
                w.write(text)
        return whole, "\n".join(shown)

    def enosys(self):
        """The calls the kernel has no handler for, as /proc/bitsos/syscalls counts them."""
        err, text = self.sh("grep -i enosys /proc/bitsos/syscalls")
        return [] if err else [l.strip() for l in text.splitlines() if "enosys" in l.lower()]

    def red(self, what):
        whole, shown = self.logs()
        print(f"RED: {what}", flush=True)
        print(f"stage reached: {self.best}", flush=True)
        for r in reasons(whole):
            print(f"why: {r}", flush=True)
        for e in self.enosys():
            print(f"kernel: {e}", flush=True)
        print(shown, flush=True)
        self.end()
        raise SystemExit(1)

    def end(self):
        if self.o.flags["--stop"]:
            _, text = self.sh(f"{LAUNCHER} --stop; echo rc=$?", 120000)
            print(text.strip(), flush=True)

    def main(self):
        os.makedirs(self.out, exist_ok=True)
        if self.o.flags["--ktest"]:
            err, text, _ = self.call("ktest", {"filter": "ul.pkg.steam", "timeout_ms": 900000})
            print(text, flush=True)
            if err or not re.search(r"failed=0 skipped=0 refused=0", text):
                self.red("ktest ul.pkg.steam")
        err, text = self.sh(f"{LAUNCHER} --check; echo rc=$?")
        print(text.strip(), flush=True)
        if err or "rc=0" not in text:
            self.red("steam --check names what is not ready")
        args = self.o.values["--steam-args"] or ""
        if not re.fullmatch(r"[\w =.+:/-]*", args):
            raise SystemExit("--steam-args: plain words only")
        # A launcher that already runs would refuse a second start with its reason sent to nowhere: follow that one instead.
        if re.search(r"^steam: running: ", text, re.M):
            print("note: a launcher already runs; this run follows it and starts nothing", flush=True)
        else:
            err, text = self.sh(f"SB_VERBOSE=1 setsid {LAUNCHER} {args} >/dev/null 2>&1 </dev/null & echo started")
            if err:
                self.red(f"the launcher does not start: {text[:300]}")
        end = time.time() + self.secs
        seen = 0
        while time.time() < end:
            time.sleep(POLL)
            err, text = self.sh(f"{LAUNCHER} --status")
            if err:
                self.red(f"steam --status: {text[:300]}")
            last, procs, windows = parse_status(text)
            now = reached(last, procs, windows)
            if ORDER.index(now) > ORDER.index(self.best):
                self.best = now
                print(f"stage: {now} ({time.strftime('%H:%M:%S')}): {last}", flush=True)
            if len(windows) > seen:
                seen = len(windows)
                self.pics += 1
                self.picture(f"window-{self.pics}.png")
                print("\n".join(f"window: {w}\nwindow {size_note(w)}" for w in windows), flush=True)
            if now == "window":
                time.sleep(PAINT)
                self.picture("login.png")
                self.logs()
                print("GREEN: a Steam window with its web helper is on the screen; look at login.png for the login window", flush=True)
                self.end()
                return
            if re.search(r"stage=(ended|[a-z]+-failed)", last):
                self.red(f"the launcher ended: {last}")
        self.red(f"no window of Steam within {self.secs} s")


if __name__ == "__main__":
    Run(sys.argv[1:]).main()
