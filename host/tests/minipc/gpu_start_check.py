#!/usr/bin/env python3
"""GS-1 over MCP (gpu-start.sh): where the time goes from the kernel's graphics start to the desktop's first picture,
after a start of the machine. Reads, never changes anything: bitscomp's summary (`bitscomp ctl boot`), the kernel's
marks (`/proc/bitsos/boot`, at every log level) and, when kernel.loglevel lets Info through, the events beside them.
Prints one table (stage, monotonic ms, ms since the stage before) and PASS or FAIL per step; exit 0 only when all passed.

Usage: gpu_start_check.py <host[:port]> (--tls <key store> | --plain) --token-file <file> [--out DIR]

  mcp       MCP answers
  summary   bitscomp says `boot.desktop_ready` (its first picture after this start)
  renderer  the summary names renderer=gles (the desktop draws only on the GPU)
  kernel    /proc/bitsos/boot names the kernel's marks of the start
  order     every mark lies after the one before it in the start's order
  events    (loglevel info or more verbose) the events say the same times as the marks, within 2 ms"""
import argparse
import json
import os
import re
import subprocess
import sys

ROOT = os.path.abspath(os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "..", ".."))
# The start's order; what each stage is, for the table.
STAGES = [
    ("gfx_start", "kernel: the GPU's bring-up starts (gx boot.start)"),
    ("engines", "kernel: PSP, GFX and MES are up (gx boot.engines_up)"),
    ("node", "kernel: the render node stands after the proof (gx boot.up)"),
    ("desktop_start", "kernel: the console starts bitscomp (console desktop.start)"),
    ("start", "bitscomp runs"),
    ("renderer_up", "the renderer is made (Mesa, EGL, shaders)"),
    ("frame", "the first frame is presented (the boot scene)"),
]
NEEDS = [s for s, _ in STAGES]
BITSCOMP = "/boot/BITSOS2/bin/bitscomp"
EVENTS = {"gfx_start": ("gx", "boot.start"), "engines": ("gx", "boot.engines_up"), "node": ("gx", "boot.up"), "desktop_start": ("console", "desktop.start")}


class Unreachable(Exception):
    pass


def summary_fields(text):
    """The words of the first `boot.desktop_ready` line: {key: value} (values without quotes)."""
    m = re.search(r"boot\.desktop_ready ([^\n\"]*)", text)
    if not m:
        return None
    return dict(w.split("=", 1) for w in m.group(1).split() if "=" in w)


def own_marks(text):
    """bitscomp's `boot.marks name=ms,name=ms` line of `ctl boot` (its marks, also those after the summary)."""
    m = re.search(r"boot\.marks (\S*)", text)
    return [tuple(kv.split("=", 1)) for kv in (m.group(1).split(",") if m else []) if "=" in kv]


def kernel_marks(text):
    return {m.group(1): int(m.group(2)) for m in re.finditer(r"^mark (\w+) (\d+)$", text, re.M)}


def event_ms(text, subsystem, name):
    """The first event's at_ms (MCP `events` lines: `#seq [time] L subsystem name k=v ...`)."""
    m = re.search(rf"\] \w {re.escape(subsystem)} {re.escape(name)} [^\n]*\bat_ms=(\d+)", text)
    return int(m.group(1)) if m else None


def table(marks):
    rows, last = [], None
    for s, what in STAGES:
        t = marks.get(s)
        if t is None:
            rows.append(f"  {s:<14} {'-':>9} {'':>9}  {what}")
            continue
        d = "" if last is None else f"+{t - last}"
        rows.append(f"  {s:<14} {t:>9} {d:>9}  {what}")
        last = t
    for s in sorted(set(marks) - {x for x, _ in STAGES}):
        rows.append(f"  {s:<14} {marks[s]:>9} {'':>9}  (more)")
    return "\n".join([f"  {'stage':<14} {'at ms':>9} {'step ms':>9}  what"] + rows)


def ordered(marks, needs):
    seen = [(s, marks[s]) for s, _ in STAGES if s in marks and s in needs]
    bad = [f"{a}={ta} > {b}={tb}" for (a, ta), (b, tb) in zip(seen, seen[1:]) if ta > tb]
    return bad


class GpuStart:
    def __init__(self, a):
        self.a = a
        self.passed = self.failed = 0
        os.makedirs(a.out, exist_ok=True)

    def step(self, name, ok, detail=""):
        self.passed += bool(ok)
        self.failed += not ok
        print(f"{'PASS' if ok else 'FAIL'} {name}" + (f": {detail}" if detail else ""), flush=True)
        return ok

    def mcp(self, tool, args=None):
        cmd = ["node", os.path.join(ROOT, "host", "mcp", "mcp-call.mjs"), self.a.target, tool, json.dumps(args or {}), "--token-file", self.a.token_file]
        if self.a.tls:
            cmd += ["--tls", self.a.tls]
        try:
            r = subprocess.run(cmd, capture_output=True, text=True, errors="replace", timeout=120)
        except subprocess.TimeoutExpired as e:
            raise Unreachable(f"{tool}: no answer within {e.timeout} s") from e
        if r.returncode not in (0, 1):
            raise Unreachable(f"{tool}: mcp-call exit {r.returncode}: {r.stderr.strip()[-300:]}")
        return r.returncode == 1, r.stdout

    def run(self, path, args):
        env = {"PATH": "/bin:/usr/bin", "XDG_RUNTIME_DIR": "/run/user/1000"}
        return self.mcp("run", {"path": path, "args": args, "env": env, "timeout_ms": 20000})[1]

    def save(self, name, text):
        with open(os.path.join(self.a.out, name), "w") as f:
            f.write(text)

    def main(self):
        try:
            return self.checks()
        except Unreachable as e:
            self.step("mcp", False, str(e))
            print(f"summary: {self.passed} passed, {self.failed} failed")
            return 1

    def checks(self):
        err, text = self.mcp("version")
        self.step("mcp", not err, text.strip().splitlines()[0] if text.strip() else "no answer")
        boot = self.run(BITSCOMP, ["ctl", "boot"])
        self.save("ctl-boot.txt", boot)
        f = summary_fields(boot)
        self.step("summary", f is not None, (re.search(r"boot\.desktop_ready[^\n]*", boot) or re.search(r"[^\n]*$", boot.strip())).group(0)[:400])
        proc = self.run("/bin/cat", ["/proc/bitsos/boot"])
        self.save("proc-boot.txt", proc)
        km = kernel_marks(proc)
        marks = dict(km)
        for k, v in list((f or {}).items()) + own_marks(boot):
            if (k in {s for s, _ in STAGES} or k == "shown") and v.isdigit():
                marks.setdefault(k, int(v))
        f = f or {}
        self.step("renderer", f.get("renderer") == "gles", f"renderer={f.get('renderer')}")
        needs = NEEDS
        kneeds = [s for s in needs if s in EVENTS]
        missing = [s for s in kneeds if s not in km]
        self.step("kernel", not missing, f"/proc/bitsos/boot marks {sorted(km)}" + (f", missing {missing}" if missing else ""))
        lacking = [s for s in needs if s not in marks]
        bad = ordered(marks, needs)
        self.step("order", not lacking and not bad, "; ".join(x for x in (f"missing {lacking}" if lacking else "", ", ".join(bad)) if x) or "rising")
        err, lv = self.mcp("settings_get", {"key": "kernel.loglevel"})
        level = (re.search(r"\b(error|warn|info|debug|trace)\b", lv) or [None, "?"])[1]
        if level in ("info", "debug", "trace"):
            ev = ""
            for sub, name in (("gx", "boot."), ("console", "desktop.start")):
                ev += self.mcp("events", {"from": 0, "subsystem": sub, "name": name, "level": "info", "max": 200})[1] + "\n"
            self.save("events.txt", ev)
            off = []
            for s, (sub, name) in EVENTS.items():
                e, m = event_ms(ev, sub, name), km.get(s)
                if e is not None and m is not None and abs(e - m) > 2:
                    off.append(f"{s}: event {e}, mark {m}")
                if s in kneeds and e is None:
                    off.append(f"{s}: no {sub} {name} event")
            self.step("events", not off, "; ".join(off) or "the events and the marks agree")
        else:
            print(f"  note: kernel.loglevel is {level}: the events are not kept, the marks above are the whole story", flush=True)
        print(table(marks), flush=True)
        if f.get("ms"):
            first = marks.get("gfx_start") or marks.get("desktop_start")
            print(f"  first picture {f['ms']} ms after the machine's start" + (f", {int(f['ms']) - first} ms after {('gfx_start' if marks.get('gfx_start') else 'desktop_start')}" if first else ""), flush=True)
        if marks.get("shown"):
            print(f"  desktop shown (boot scene ended) at {marks['shown']} ms", flush=True)
        print(f"summary: {self.passed} passed, {self.failed} failed", flush=True)
        return 0 if self.failed == 0 else 1


def selftest():
    """The parsing against lines as bitscomp, the kernel's file and MCP `events` write them (gpu-start.sh --selftest)."""
    s = summary_fields('call 3: exit 0\nboot.desktop_ready ms=6400 renderer=gles gfx_start=1200 frame=6400\n')
    assert s == {"ms": "6400", "renderer": "gles", "gfx_start": "1200", "frame": "6400"}, s
    assert summary_fields("boot none\nboot.marks start=1") is None
    assert own_marks("boot none\nboot.marks start=1,shown=9\n") == [("start", "1"), ("shown", "9")]
    k = kernel_marks("bitsos-boot 1\nstep gpu done 3100 gpu bereit\nmark gfx_start 1200\nmark node 3100\n")
    assert k == {"gfx_start": 1200, "node": 3100}, k
    ev = '#12 [    1.200345] I gx boot.start bound_s=30 at_ms=1200\n#40 [    3.300000] I console desktop.start program=/boot/BITSOS2/bin/bitscomp attempt=1 at_ms=3300\n'
    assert event_ms(ev, "gx", "boot.start") == 1200 and event_ms(ev, "console", "desktop.start") == 3300 and event_ms(ev, "gx", "boot.up") is None
    assert ordered({"gfx_start": 5, "node": 3}, NEEDS) == ["gfx_start=5 > node=3"]
    assert ordered({"gfx_start": 1, "node": 3, "start": 4, "frame": 9}, NEEDS) == []
    assert "+2" in table({"gfx_start": 1, "engines": 3})
    print("gpu_start_check selftest: ok")
    return 0


def args():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("target", nargs="?")
    tls = p.add_mutually_exclusive_group()
    tls.add_argument("--tls", help="key store for mutual TLS (the Mini-PC), or auto")
    tls.add_argument("--plain", action="store_true", help="plain HTTP (the kernel's own server, as in QEMU)")
    p.add_argument("--token-file")
    p.add_argument("--out", default=os.path.join(ROOT, "target", "minipc", "gpu-start"))
    p.add_argument("--selftest", action="store_true")
    a = p.parse_args()
    if not a.selftest and not (a.target and a.token_file and (a.tls or a.plain)):
        p.error("target, --token-file and --tls or --plain are needed")
    return a


if __name__ == "__main__":
    a = args()
    sys.exit(selftest() if a.selftest else GpuStart(a).main())
