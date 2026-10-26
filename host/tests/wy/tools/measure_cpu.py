#!/usr/bin/env python3
"""WY-0 CPU measurement of bitscomp's software path (not a fullrun scenario: needs Brave, takes minutes).

Scenes: static (Brave test page + foot side by side, idle), scroll (Brave alone, page scrolls 4 px per
frame), terminal (foot alone printing text without pause). For each: bitscomp's CPU share of one core,
its frame counters, and the clients' CPU for comparison. Processes are stopped by PID only.

usage: measure_cpu.py [--backend headless|winit] [--size 2560x1440] [--secs 10] [--pin CPU] [--logs DIR] [--out FILE]
"""
import argparse, os, shutil, signal, subprocess, sys, time

ROOT = os.path.abspath(os.path.join(os.path.dirname(__file__), "../../../.."))
TARGET = os.environ.get("CARGO_TARGET_DIR", os.path.join(ROOT, "target"))
BIN = os.path.join(TARGET, "release", "bitscomp")
PAGE_DIR = os.path.join(ROOT, "host/tests/wy/page")
PORT = 30500
HZ = os.sysconf("SC_CLK_TCK")
BRAVE_FLAGS = ["--ozone-platform=wayland", "--disable-gpu", "--password-store=basic", "--no-first-run",
               "--no-default-browser-check", "--disable-breakpad"]


def ticks(pid):
    try:
        with open(f"/proc/{pid}/stat") as f:
            parts = f.read().rsplit(")", 1)[1].split()
        return int(parts[11]) + int(parts[12])
    except OSError:
        return 0


def children(pid):
    out = []
    for p in os.listdir("/proc"):
        if p.isdigit():
            try:
                with open(f"/proc/{p}/stat") as f:
                    if int(f.read().rsplit(")", 1)[1].split()[1]) == pid:
                        out.append(int(p))
            except OSError:
                pass
    return out


def tree(pid):
    todo, seen = [pid], []
    while todo:
        p = todo.pop()
        seen.append(p)
        todo.extend(children(p))
    return seen


def cpu(pids_fn, secs):
    before = {p: ticks(p) for p in pids_fn()}
    time.sleep(secs)
    after = {p: ticks(p) for p in pids_fn()}
    used = sum(after[p] - before.get(p, 0) for p in after)
    return 100.0 * used / HZ / secs


def ctl(sock, *cmd):
    return subprocess.run([BIN, "ctl", "--socket", sock, *cmd], capture_output=True, text=True).stdout.strip()


def stop(proc):
    if proc and proc.poll() is None:
        proc.send_signal(signal.SIGTERM)
        try:
            proc.wait(5)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait()


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--backend", default="headless")
    ap.add_argument("--size", default="2560x1440")
    ap.add_argument("--secs", type=float, default=10)
    ap.add_argument("--out", default="-")
    ap.add_argument("--pin", help="CPU number to pin bitscomp to (hybrid CPUs: P- and E-cores differ)")
    ap.add_argument("--logs", help="directory for the compositor's and the clients' output")
    a = ap.parse_args()
    sock = f"wayland-wy-m{os.getpid()}"
    env = dict(os.environ, WAYLAND_DISPLAY=sock)
    env.pop("DISPLAY", None)
    # A fresh profile per run: a restored session from the last run could show a different tab in front.
    profile = f"/tmp/claude-{os.getuid()}/wy-brave-measure"
    http = subprocess.Popen([sys.executable, "-m", "http.server", str(PORT), "--bind", "127.0.0.1", "--directory", PAGE_DIR],
                            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    where = f", pinned to CPU {a.pin}" if a.pin else ""
    lines = [f"# bitscomp CPU, back end {a.backend}, output {a.size} at scale 1.25, {a.secs:.0f} s per scene{where}"]
    lines.append("scene | bitscomp CPU % of one core | frames/s | render ms/frame | copy ms/frame | damaged Mpx/frame | clients CPU % | windows")
    scenes = [
        ("static", [["brave", *BRAVE_FLAGS, f"--user-data-dir={profile}", f"http://127.0.0.1:{PORT}/"], ["foot", "sh"]]),
        ("scroll", [["brave", *BRAVE_FLAGS, f"--user-data-dir={profile}", f"http://127.0.0.1:{PORT}/?scroll=4"]]),
        ("terminal", [["foot", "sh", "-c", f"while :; do cat {ROOT}/plan/V2-PLAN.md; done"]]),
    ]
    try:
        for name, clients in scenes:
            log = lambda who: open(os.path.join(a.logs, f"{name}-{who}.log"), "w") if a.logs else subprocess.DEVNULL
            pin = ["taskset", "-c", a.pin] if a.pin else []
            shutil.rmtree(profile, ignore_errors=True)
            os.makedirs(profile)
            comp = subprocess.Popen([*pin, BIN, "--backend", a.backend, "--size", a.size, "--socket", sock],
                                    stdout=log("bitscomp"), stderr=subprocess.STDOUT)
            time.sleep(0.5)
            # Clients stay off bitscomp's CPU, so its frame times are not stretched by preemption.
            others = ",".join(str(c) for c in sorted(os.sched_getaffinity(0)) if str(c) != a.pin)
            cpin = ["taskset", "-c", others] if a.pin else []
            procs = [subprocess.Popen([*cpin, *c], env=env, stdout=log(c[0]), stderr=subprocess.STDOUT) for c in clients]
            time.sleep(10)  # page load, first frames
            listing = ctl(sock, "list")
            windows = listing.count("visible=true")
            if "brave" in listing and "WY-0" not in listing:
                windows = f"{windows} (test page not shown)"
            ctl(sock, "stats", "reset")
            client_pids = lambda: [p for pr in procs for p in tree(pr.pid)]
            t0 = {p: ticks(p) for p in client_pids()}
            comp_cpu = cpu(lambda: [comp.pid], a.secs)
            t1 = {p: ticks(p) for p in client_pids()}
            clients_cpu = 100.0 * sum(t1[p] - t0.get(p, 0) for p in t1) / HZ / a.secs
            st = dict(kv.split("=") for kv in ctl(sock, "stats").split())
            fps = int(st["frames"]) / a.secs
            lines.append(f"{name} ({st['output']}) | {comp_cpu:.1f} | {fps:.1f} | {float(st['render_avg_ms']):.2f} | "
                         f"{float(st['present_avg_ms']):.2f} | {float(st['damaged_mpx_avg']):.2f} | {clients_cpu:.0f} | {windows}")
            for pr in procs:
                stop(pr)
            ctl(sock, "quit")
            stop(comp)
            time.sleep(1)
    finally:
        stop(http)
    text = "\n".join(lines) + "\n"
    if a.out == "-":
        print(text, end="")
    else:
        with open(a.out, "w") as f:
            f.write(text)
        print(text, end="")


if __name__ == "__main__":
    main()
