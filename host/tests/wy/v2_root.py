#!/usr/bin/env python3
"""WY-7 (74-root.sh): bitscomp started from the console shell (root, as Georg starts it) runs as uid 1000.

Usage: v2_root.py <mcp host port> <token> <qmp socket> <run dir>
The console shell is init's (uid 0): the start line goes to it through MCP's keyboard (send_keys). Then, as MCP's
own uid 1000: bitscomp's ids (real, effective, saved, fs; groups), its sockets in /run/user/1000 are uid 1000's
and `bitscomp ctl` reaches them (Georg's 21.10. note), the compositor grant held across the drop (Interactive
class; focus changes still accepted), a foot by Strg+Alt+Enter runs as 1000 with XDG_RUNTIME_DIR=/run/user/1000
and its shell too, a keyboard plugged in after the drop (QMP device_add) is opened and grabbed, `ctl quit` (as
1000) ends it and the console comes back. Then (fixer): a refused drop (`--user 4294967295`) ends bitscomp,
`--user 0` keeps root, SIGKILL of bitscomp takes its foot along (parent-death signal), and the next root start
clears the root-owned lock the killed run left. Exit 0 = green."""
import os
import re
import sys
import time

sys.dont_write_bytecode = True
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import v2_foot as f  # noqa: E402  (sets argv for v2_check)
from v2_check import Qmp, call, need  # noqa: E402
import v2_check as v  # noqa: E402

RUN = "/run/user/1000"
ENV = {"PATH": "/bin:/usr/bin:/boot/BITSOS2/bin", "HOME": "/", "XDG_RUNTIME_DIR": RUN}
LOG = "/tmp/wy7-root.log"
# v2_foot's helpers (`ctl list`, `ctl hot`) go to the console's bitscomp: its default socket in the user's run dir.
v.SOCK = "wayland-bits-0"
v.ENV = dict(v.ENV, XDG_RUNTIME_DIR=RUN, WAYLAND_DISPLAY=v.SOCK)


def sh(c, cmd, timeout_ms=10000):
    _, t, _ = call(c, "run", {"path": "/bin/sh", "args": ["-c", cmd], "env": ENV, "timeout_ms": timeout_ms})
    m = re.search(r"--- stdout \(\d+ bytes\) ---\n(.*?)--- stderr", t, re.S)
    return m.group(1).strip() if m else t


def ctl(c, *words):
    _, t, _ = call(c, "run", {"path": "/bin/bitscomp", "args": ["ctl", *words], "env": ENV, "timeout_ms": 20000})
    return t


def wait(what, secs, probe):
    end = time.time() + secs
    while True:
        r = probe()
        if r:
            print(f"ok: {what}", flush=True)
            return r
        if time.time() > end:
            raise SystemExit(f"RED: {what} (waited {secs} s)")
        time.sleep(0.5)


def ids(c, pid):
    st = sh(c, f"cat /proc/{pid}/status")

    def get(k):
        m = re.search(rf"(?m)^{k}:[ \t]*(.*)$", st)
        return " ".join(m.group(1).split()) if m else "-"
    return get("Uid"), get("Gid"), get("Groups")


def log(c):
    return sh(c, f"cat {LOG}")


def main():
    c = v.connect(300)
    q = Qmp(v.QMP)
    scr = lambda: call(c, "screen", {"format": "text"})[1]  # noqa: E731
    try:
        wait("the console shell's prompt", 60, lambda: re.search(r"(?m)^/ #( |$)", scr()))
    except SystemExit:
        print(scr()[-1500:], flush=True)
        call(c, "send_keys", {"keys": ["enter"]})  # boot messages may have pushed the prompt away
        wait("the console shell's prompt after Enter", 30, lambda: re.search(r"(?m)^/ #( |$)", scr()))
    n = f"{os.getpid()}.{int(time.time())}"
    call(c, "send_keys", {"text": f"id -u >/tmp/wy7-shell.{n}; (setsid bitscomp --stats 5 </dev/null >{LOG} 2>&1 &)\n"})
    need(wait("the console shell wrote its uid", 30, lambda: sh(c, f"cat /tmp/wy7-shell.{n} 2>/dev/null")) == "0",
         "the console shell is root")
    wait("bitscomp says it runs as uid 1000", 60, lambda: "now running as uid 1000 gid 1000" in log(c))
    text = wait("bitscomp started on fbdev", 30, lambda: (lambda t: t if "backend=fbdev" in t else None)(log(c)))
    pid = sh(c, "pidof bitscomp").split()[0]
    uid, gid, groups = ids(c, pid)
    need((uid, gid, groups) == ("1000 1000 1000 1000", "1000 1000 1000 1000", "1000"),
         f"bitscomp (pid {pid}) runs as uid/gid 1000 with groups [1000]", f"Uid {uid} Gid {gid} Groups {groups}")
    need(f"XDG_RUNTIME_DIR={RUN} " in text, f"its run directory is {RUN}", text[-1500:])
    need("scheduling class Interactive" in text, "the compositor grant was taken before the drop", text[-1500:])
    own = sh(c, f"stat -c '%n %u:%g' {RUN}/wayland-bits-0 {RUN}/wayland-bits-0.ctl {RUN}/wayland-bits-0.lock")
    print(own, flush=True)
    need(all(l.endswith(" 1000:1000") for l in own.splitlines()) and len(own.splitlines()) == 3,
         "the Wayland socket, its lock and the control socket are uid 1000's", own)
    stats = ctl(c, "stats")
    need("frames=" in stats, "bitscomp ctl as uid 1000 reaches the root-started bitscomp", stats)
    # Linux commit_creds (kernel/cred.c): a credential change makes the process not dumpable, so its /proc files
    # are root's: uid 1000 cannot read the environment (or ptrace it and its root-opened fds). Kept on purpose.
    dump = sh(c, f"cat /proc/{pid}/environ >/dev/null 2>&1 && echo readable || echo refused")
    need(dump == "refused", "the dropped bitscomp is not dumpable (its /proc environ is root's)", dump)

    f.keys(c, "ctrl+alt+enter")
    win = wait("Strg+Alt+Enter opened a foot", 40, lambda: [w for w in f.windows(c)])
    fpid = wait("foot's pid", 20, lambda: f.foot_procs(c))[0][0]
    fuid, _, _ = ids(c, fpid)
    need(fuid == "1000 1000 1000 1000", f"foot (pid {fpid}) runs as uid 1000", fuid)
    # The trampoline (`bitscomp exec`) became foot: the pid bitscomp started is foot's, its parent is bitscomp.
    ppid = sh(c, f"sed -n 's/^PPid:[[:space:]]*//p' /proc/{fpid}/status")
    need(ppid == pid and sh(c, "pidof bitscomp").split() == [pid],
         "foot's pid is the started one (parent bitscomp, no trampoline left)", f"PPid {ppid}, bitscomp {sh(c, 'pidof bitscomp')}")
    blk = sh(c, f"sed -n 's/^SigBlk:[[:space:]]*//p' /proc/{fpid}/status")
    need(blk.strip("0") == "", f"foot starts with no blocked signal (SigBlk {blk}; bitscomp blocks TERM/INT/HUP)", blk)
    env = sh(c, f"tr '\\0' '\\n' < /proc/{fpid}/environ")
    need(f"XDG_RUNTIME_DIR={RUN}" in env.splitlines() and "HOME=/" in env.splitlines(),
         "foot has XDG_RUNTIME_DIR=/run/user/1000 and the user's HOME", env)
    f.wait_shells(c, 1, "foot's shell is ready")
    w, _ = f.hot(c)
    f.focus_in(c, win[0]["id"], w[win[0]["id"]]["body"])
    f.type_text(c, f"id -u >/tmp/wy7-foot.{n}\n")
    need(wait("the shell in foot wrote its uid", 20, lambda: sh(c, f"cat /tmp/wy7-foot.{n} 2>/dev/null")) == "1000",
         "the shell in foot is uid 1000")
    before = log(c)
    q.cmd("device_add", {"driver": "usb-kbd", "bus": "xb.0", "port": "1", "id": "kbd9"})
    added = wait("a keyboard plugged in after the drop is opened by bitscomp", 30,
                 lambda: (lambda t: t[len(before):] if re.search(r"bitscomp: input: /dev/input/event\d+ .*grab", t[len(before):]) else None)(log(c)))
    print("  " + "\n  ".join(l for l in added.splitlines() if "input:" in l), flush=True)
    need("grant dropped" not in log(c), "the grant held across the drop (focus changes accepted)", log(c)[-1500:])
    print("  " + ctl(c, "quit").strip().splitlines()[-1], flush=True)
    wait("bitscomp ended after ctl quit as uid 1000", 30, lambda: "gone" in sh(c, "pidof bitscomp || echo gone"))
    end = log(c)
    need("bitscomp: ended" in end, "bitscomp ended cleanly", end[-1500:])
    wait("the console has the screen again", 30, lambda: re.search(r"(?m)^/ #( |$)", call(c, "screen", {"format": "text"})[1]))
    print("--- bitscomp's log ---\n" + end, flush=True)
    refused(c, n)
    killed_root(c, q, n)
    stale_cleared(c, n)


def prompt(c, what):
    scr = lambda: call(c, "screen", {"format": "text"})[1]  # noqa: E731
    wait(what, 30, lambda: re.search(r"(?m)^/ #( |$)", scr()))


def refused(c, n):
    """A drop the kernel refuses ends bitscomp with the reason; it never goes on as root."""
    neg = f"/tmp/wy7-neg.{n}"
    call(c, "send_keys", {"text": f"bitscomp --user 4294967295 >{neg}.log 2>&1; echo $? >{neg}.rc\n"})
    rc = wait("the refused drop ended bitscomp", 60, lambda: sh(c, f"cat {neg}.rc 2>/dev/null"))
    text = sh(c, f"cat {neg}.log")
    need(rc not in ("", "0") and "giving up root for uid 4294967295" in text and "now running as" not in text,
         f"a refused drop ends bitscomp with the reason (exit {rc})", text[-1200:])
    prompt(c, "the console is back after the refused drop")


def killed_root(c, q, n):
    """`--user 0` keeps root on purpose; SIGKILL of bitscomp takes its started foot along (PR_SET_PDEATHSIG, set
    by the trampoline) and leaves a root-owned Wayland lock behind for stale_cleared()."""
    lg = f"/tmp/wy7-root0.{n}.log"
    call(c, "send_keys", {"text": f"(setsid bitscomp --user 0 --stats 5 </dev/null >{lg} 2>&1 &)\n"})
    wait("bitscomp --user 0 started on fbdev", 60, lambda: "backend=fbdev" in sh(c, f"cat {lg}"))
    pid = sh(c, "pidof bitscomp").split()[0]
    uid, _, _ = ids(c, pid)
    need(uid == "0 0 0 0" and "now running as" not in sh(c, f"cat {lg}"), f"--user 0 keeps root (pid {pid})", uid)
    f.mouse(c, x=640, y=400)
    f.keys(c, "ctrl+alt+enter")
    fpid = wait("Strg+Alt+Enter opened a foot as root", 40, lambda: f.foot_procs(c))[0][0]
    need(ids(c, fpid)[0] == "0 0 0 0", f"foot (pid {fpid}) of a --user 0 bitscomp is root's")
    f.wait_shells(c, 1, "the root foot's shell is ready")
    # A root run's lock as an older bitscomp (before WY-7) left it: root's, unheld (the live one is unlinked; its
    # flock stays on the old file). Clean ends remove the lock, and a lock that already exists keeps its owner.
    lock = f"{RUN}/wayland-bits-0.lock"
    f.type_text(c, f"id -u >/tmp/wy7-root0.{n}; rm -f {lock}; : >{lock}; kill -9 $(pidof bitscomp)\n")
    need(wait("the root foot's shell wrote its uid", 20, lambda: sh(c, f"cat /tmp/wy7-root0.{n} 2>/dev/null")) == "0",
         "the shell in that foot is root")
    t0 = time.time()
    wait("bitscomp is gone after SIGKILL", 20, lambda: "gone" in sh(c, "pidof bitscomp || echo gone"))
    wait("its foot ended with it (parent-death signal)", 15, lambda: not f.foot_procs(c))
    print(f"  foot gone {time.time() - t0:.1f} s after the SIGKILL", flush=True)
    prompt(c, "the console has the screen again after the SIGKILL")


def stale_cleared(c, n):
    """The next root start finds root's stale lock in the user's run directory and clears it while still root."""
    lock = f"{RUN}/wayland-bits-0.lock"
    own = sh(c, f"stat -c '%u:%g' {lock} 2>/dev/null || echo none")
    print(f"  {lock} after the killed root run: {own}", flush=True)
    lg = f"/tmp/wy7-again.{n}.log"
    call(c, "send_keys", {"text": f"(setsid bitscomp --stats 5 </dev/null >{lg} 2>&1 &)\n"})
    wait("bitscomp started again and runs as uid 1000", 60, lambda: "now running as uid 1000 gid 1000" in sh(c, f"cat {lg}"))
    text = wait("bitscomp is on fbdev again", 30, lambda: (lambda t: t if "backend=fbdev" in t else None)(sh(c, f"cat {lg}")))
    need(own == "0:0", "the killed root run left a root-owned lock", own)
    need(f"removed {lock} (left by another user's bitscomp)" in text, "the stale root-owned lock was cleared as root", text[-1500:])
    stats = ctl(c, "stats")
    need("frames=" in stats, "bitscomp ctl reaches it again (its socket is uid 1000's)", stats)
    ctl(c, "quit")
    wait("bitscomp ended after the second ctl quit", 30, lambda: "gone" in sh(c, "pidof bitscomp || echo gone"))
    prompt(c, "the console is back at the end")


if __name__ == "__main__":
    main()
