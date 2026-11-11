#!/usr/bin/env python3
"""TS-1: a whole ktest run on the real Mini-PC over MCP (Georg 30.10., CONTRACTS section 4). The desk starts it through
desktop.sh or bare.sh; the running kernel must be one with ktests (the lean stick kernel has none: Reload first).

Usage: ktests.py <desktop|bare> <host[:port]> [--tls DIR|auto] [--token-file F] [--out DIR] [--generation N] [--record]

A run sets its own conditions and puts them back, also when it ends red:
  - the log level: `settings_set kernel.loglevel info` (some tests look at log records), then the value the machine
    had (`settings_get`), read back afterwards;
  - desktop: the desktop stays. Tests of class Screen show as skip, "belongs to the Mini-PC run without the desktop";
  - bare: `bitscomp ctl quit` first (the console takes the screen and the input back; dp.*, in.*, the screen tools get
    them to themselves), no process may keep /dev/fb* or /dev/input/event* open, and the desktop again at the end:
    `bitscomp` typed into the console shell, as Georg can. A desktop that does not come back is RED. In a boot with
    CONFIG desktop=off (the other way to this run, by hand) nothing holds the screen: nothing is stopped or started.

Safe by what MCP's class rule does: Audible and WithGeorg are refused, Picture too (GX-D9), QEMU's classes belong to
another run. A CONFIG.TXT with nvme_write=on, ktest_georg=on or ktest_picture=on is refused here: no NVMe write,
nothing audible, no change of the picture.

Green: no FAIL, every skip says it belongs to another run; in the bare run nothing still belongs to the bare run.
--record (after a round the desk accepts) adds the tests that passed to host/tests/minipc/ran.txt, which `cargo xtask
test` counts. Output: <out>/ktest-<run>.txt, default target/minipc/. Exit 0 = green."""
import base64
import datetime
import json
import os
import re
import signal
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import mp  # noqa: E402

RAN = os.path.join(HERE, "ran.txt")
LEVEL_KEY = "kernel.loglevel"
TEST_LEVEL = "info"
NORMAL_LEVEL = "error"  # Georg 27.10.: the Mini-PC normally runs at "Nur Fehler"
# dp.*, in.*, the screen tools: the tests that want the screen and the input; the bare run needs no other.
BARE_FILTER = "contract.console_ramframe,dp.,in.,ul.tools.,gx.flip.,gx.mode."
ENV = {"PATH": "/bin:/usr/bin:/boot/BITSOS2/bin", "XDG_RUNTIME_DIR": "/run/user/1000"}
BARE_PROBE = "dp.display.handover"
REFUSED_OK = ("Audible:", "WithGeorg:", "wlan:", "changes the picture:")
FORBIDDEN = ("nvme_write=on", "ktest_georg=on", "ktest_picture=on")
STOP_WAIT_S = 60
POLL_S = 2
QUIT_AGAIN_S = 10
HELD_DEVICE = re.compile(r" -> (/dev/fb\S*|/dev/input/event\S*)$")

def say(text):
    print(text, flush=True)


def red(text):
    raise SystemExit(f"RED: {text}")


def config_text(o):
    err, text, _ = mp.call(o, "read_file", {"path": "/boot/BITSOS2/CONFIG.TXT"})
    if err:
        red(f"read_file CONFIG.TXT: {text}")
    return base64.b64decode(json.loads(text)["data"]).decode(errors="replace")


def config_value(text, key, default):
    v = default
    for line in text.splitlines():
        if line.startswith(key + "="):
            v = line[len(key) + 1:].strip()
    return v


def get_level(o):
    """The machine's own value now, or None."""
    err, text, _ = mp.call(o, "settings_get", {"key": LEVEL_KEY})
    if err:
        return None
    try:
        v = json.loads(text)["value"]
    except (ValueError, KeyError, TypeError):
        return None
    return v if isinstance(v, str) and v else None


def level_to_restore(o, cfg):
    """What the machine runs at. CONFIG.TXT only when the machine does not say: the live value comes from settingsd."""
    v = get_level(o)
    if v:
        return v
    v = config_value(cfg, "loglevel", NORMAL_LEVEL)
    say(f"WARNING: settings_get {LEVEL_KEY} gave no value: at the end the level goes to {v} (CONFIG.TXT or the normal level), not proven to be the old one")
    return v


def set_level(o, value):
    err, text, _ = mp.call(o, "settings_set", {"key": LEVEL_KEY, "value": value})
    if err:
        red(f"settings_set {LEVEL_KEY} {value}: {text}")
    say(f"log level: {value}")


def restore_level(o, value):
    set_level(o, value)
    now = get_level(o)
    if now is None:
        say(f"WARNING: settings_get {LEVEL_KEY} gave no value: the restored level {value} is not proven")
    elif now != value:
        red(f"log level is {now} after setting it back to {value}")


def sh(o, cmd, ms=30000):
    return mp.call(o, "run", {"path": "/bin/sh", "args": ["-c", cmd], "env": ENV, "timeout_ms": ms})


def screen_held(o):
    """True while a program holds the screen: the class rule keeps a Screen test out (hardware), or the test itself
    answers Busy (QEMU). Any other FAIL of the test is not about the holder: the run's own lines show it."""
    err, text, _ = mp.call(o, "ktest", {"filter": BARE_PROBE, "timeout_ms": 60000})
    if err:
        red(f"ktest {BARE_PROBE}: {text}")
    return f"skip {BARE_PROBE}:" in text or (f"FAIL {BARE_PROBE}" in text and "Busy" in text)


def proc_text(o, path):
    """A small /proc file as the kernel reads it, or None (the process has gone)."""
    err, text, _ = mp.call(o, "read_file", {"path": path})
    return None if err else base64.b64decode(json.loads(text)["data"]).decode(errors="replace")


def holders(o):
    """Who keeps the screen's or the input's devices open, and a bitscomp that still runs: "<pid> <comm> <what>".
    Through MCP's file tools: `run` is uid 1000 and may not look at /proc/<pid>/fd of the other processes."""
    err, text, _ = mp.call(o, "list_dir", {"path": "/proc"})
    if err:
        red(f"list_dir /proc: {text}")
    found = []
    for pid in re.findall(r"^\S+\s+\S+\s+\S+\s+(\d+)/", text, re.M):
        comm = proc_text(o, f"/proc/{pid}/comm")
        status = proc_text(o, f"/proc/{pid}/status")
        err, fds, _ = mp.call(o, "list_dir", {"path": f"/proc/{pid}/fd"})
        if comm is None or status is None or re.search(r"^State:\s*Z", status, re.M):
            continue
        comm = comm.strip()
        if comm == "bitscomp":
            found.append(f"{pid} {comm} running")
        if not err:
            found += [f"{pid} {comm} {m.group(1)}" for m in map(HELD_DEVICE.search, fds.splitlines()) if m]
    return found


def wait(what, secs, cond):
    end = time.time() + secs
    while time.time() < end:
        if cond():
            say(f"ok: {what}")
            return True
        time.sleep(POLL_S)
    return False


def stop_desktop(o):
    before = holders(o)
    say("the desktop holds: " + ("; ".join(before) or "nothing /proc shows (so the look after the stop proves little)"))
    sent = [0.0]

    def gone():
        if not screen_held(o):
            return True
        # Again every few seconds: a desktop that has just taken the screen may not have its control socket yet.
        if time.time() - sent[0] >= QUIT_AGAIN_S:
            sent[0] = time.time()
            err, text, _ = sh(o, "exec bitscomp ctl quit")
            say(f"bitscomp ctl quit: {'error ' if err else ''}{text.strip()}")
        return False

    if not wait("the desktop gave the screen back", STOP_WAIT_S, gone):
        red(f"the desktop still holds the screen {STOP_WAIT_S} s after bitscomp ctl quit")
    left = holders(o)
    if left:
        red("the screen is free, but these still run or hold its devices: " + "; ".join(left))
    say("ok: no process holds /dev/fb* or /dev/input/event*")


def start_desktop(o):
    """As Georg can: the console shell has the keyboard again and starts it (DS-4: a desktop that ended on its own is
    not restarted by the kernel). Nothing is typed while a program holds the screen: it would land in that program."""
    if screen_held(o):
        say("WARNING: a program still holds the screen after the run: nothing typed; if the desktop ends later the machine shows the console")
        return
    err, text, _ = mp.call(o, "send_keys", {"text": "bitscomp\n"})
    if err:
        red(f"send_keys refused ({text}): the machine shows the console, a reboot brings the desktop back")
    if not wait("the desktop runs again", STOP_WAIT_S, lambda: screen_held(o)):
        red(f"the desktop did not come back within {STOP_WAIT_S} s: the machine shows the console, a reboot brings it back")


def attempt(what, problems, fn):
    """A step of putting things back: its failure is noted and the next step still runs."""
    try:
        fn()
    except (SystemExit, Exception) as e:  # noqa: BLE001 (nothing here may end the putting back)
        problems.append(f"{what}: {e}")
        say(f"RED: {what}: {e}")


def on_signal(signum, _frame):
    raise SystemExit(f"RED: signal {signum}: the machine is being put back")


def parse(text):
    results = {"ok": [], "FAIL": [], "skip": [], "refused": []}
    for line in text.splitlines():
        m = re.match(r"(ok|FAIL|skip|refused) ([^ :]+)(?:: | \()?(.*)$", line)
        if m:
            results[m.group(1)].append((m.group(2), m.group(3)))
    return results


def judge(run, r):
    bad = [f"FAIL {n}: {w}" for n, w in r["FAIL"]]
    for n, w in r["skip"]:
        if not w.startswith("belongs to") and "not Safe" not in w:
            bad.append(f"skip {n}: {w} (a test cannot skip itself)")
        if run == "bare" and "without the desktop" in w:
            bad.append(f"skip {n}: still held by a program in the bare run")
    for n, w in r["refused"]:
        if not w.startswith(REFUSED_OK):
            bad.append(f"refused {n}: {w}")
    return bad


def record(run, names, generation):
    today = datetime.date.today().isoformat()
    lines = []
    if os.path.exists(RAN):
        with open(RAN) as f:
            lines = [l.rstrip("\n") for l in f]
    fresh = set(names)
    kept = [l for l in lines if l.startswith("#") or not l.strip() or not (l.split()[0] in fresh and l.split()[1] == run)]
    kept += [f"{n} {run} {today} {generation}" for n in sorted(fresh)]
    with open(RAN, "w") as f:
        f.write("\n".join(kept) + "\n")
    say(f"recorded {len(fresh)} tests of run {run} in {RAN}")


def main():
    if len(sys.argv) < 2 or sys.argv[1] not in ("desktop", "bare"):
        raise SystemExit(__doc__)
    run = sys.argv[1]
    o = mp.options(sys.argv[2:], positional=1, flags=("--record",), values=("--out", "--generation"))
    out = o.values["--out"] or "target/minipc"
    os.makedirs(out, exist_ok=True)
    cfg = config_text(o)
    for f in FORBIDDEN:
        if any(line.strip() == f for line in cfg.splitlines()):
            red(f"CONFIG.TXT has {f}: this script runs only without it")
    level = level_to_restore(o, cfg)
    for sig in (signal.SIGTERM, signal.SIGHUP):
        signal.signal(sig, on_signal)
    path = os.path.join(out, f"ktest-{run}.txt")
    problems = []
    quit_sent = False
    set_level(o, TEST_LEVEL)
    try:
        if run == "bare":
            if screen_held(o):
                # From here on the desktop may go at any moment, also if this script's wait gives up first.
                quit_sent = True
                stop_desktop(o)
            else:
                say("nothing holds the screen (a boot with desktop=off?): nothing to stop, nothing to start afterwards")
        err, text, _ = mp.call(o, "ktest", {"filter": BARE_FILTER if run == "bare" else "", "timeout_ms": 1500000}, timeout=1600)
        with open(path, "w") as f:
            f.write(text)
    finally:
        if quit_sent:
            attempt("the desktop", problems, lambda: start_desktop(o))
        attempt("the log level", problems, lambda: restore_level(o, level))
    if err:
        red(f"ktest: {text[:400]}")
    r = parse(text)
    say(text.splitlines()[0] if text else "(no answer)")
    say(f"{run}: ok {len(r['ok'])}, FAIL {len(r['FAIL'])}, skip {len(r['skip'])}, refused {len(r['refused'])} (all lines: {path})")
    bad = judge(run, r) + problems
    for b in bad:
        say(b)
    if o.flags["--record"] and r["ok"]:
        record(run, [n for n, _ in r["ok"]], o.values["--generation"] or "-")
    if bad:
        red(f"{len(bad)} line(s) above")
    say(f"GREEN: Mini-PC ktest run {run}")


if __name__ == "__main__":
    main()
