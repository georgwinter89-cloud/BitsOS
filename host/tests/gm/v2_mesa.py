#!/usr/bin/env python3
"""GM-1a scenario client (10-mesa.sh): the staged Mesa tree on V2, software drivers only, driven over MCP.

Usage: v2_mesa.py <mcp host port> <token> <qmp socket> <run dir> <host reference dir>
No /dev/dri; the tree's `run --check`; vulkaninfo --summary lists lavapipe; eglinfo -B finds llvmpipe; vkcube --wsi
wayland, eglgears_wayland and es2gears_wayland each 20 s in bitscomp (wl_shm): a window with a first frame, a picture
that is not empty and moves (two screendumps), the program still running at the end, then closed by `ctl close` and
ending without a signal. Every program runs with errlog.so preloaded; its failed calls and the kernel's
ENOSYS/ENOTTY events are reported against the host's (host-ref.sh). Exit 0 = green; report.json has the findings."""
import base64
import json
import os
import re
import sys
import time

sys.dont_write_bytecode = True
HOSTREF = sys.argv[5]
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "wy"))
sys.argv = sys.argv[:5] + ["-"]
import v2_check as v  # noqa: E402
from v2_check import Qmp, call, ctl, need  # noqa: E402

T = "/boot/BITSOS2/mesa"
SHIM = "/boot/BITSOS2/gmtest/errlog.so"
WORK = "/tmp/gm"
ENV = dict(v.ENV, HOME=f"{WORK}/home", LD_PRELOAD=SHIM)
WINDOWED = [("vkcube", ["--wsi", "wayland"]), ("eglgears_wayland", []), ("es2gears_wayland", [])]
SECS = 20
REPORT = {"programs": {}}


def sh(c, script, timeout_ms=30000):
    _, text, _ = call(c, "run", {"path": "/bin/sh", "args": ["-c", script], "env": v.ENV, "timeout_ms": timeout_ms})
    return text


def ended(text):
    """(how, value) of a finished MCP run: ("exit", n) or ("signal", n); None while it runs."""
    m = re.search(r"ended: (exit|signal) (\d+)", text)
    return (m.group(1), int(m.group(2))) if m else None


def mesa(c, prog, args, tag=None, extra=None, timeout_ms=180000):
    tag = tag or prog
    env = dict(ENV, ERRLOG_FILE=f"{WORK}/err-{tag}.log", **(extra or {}))
    err, text, _ = call(c, "run", {"path": "/bin/sh", "args": [f"{T}/run", prog, *args], "env": env, "timeout_ms": timeout_ms})
    open(os.path.join(v.RUN, f"{tag}.out"), "w").write(text)
    return err, text


def read_file(c, path):
    err, text, _ = call(c, "read_file", {"path": path})
    if err:
        return None
    j = json.loads(text)
    return j["text"] if "text" in j else base64.b64decode(j["data"]).decode("utf-8", "replace")


class Prog:
    """A window program started with MCP `run` that keeps running; `text` collects its output."""

    def __init__(self, c, prog, args):
        self.c, self.prog = c, prog
        env = dict(ENV, ERRLOG_FILE=f"{WORK}/err-{prog}.log")
        _, text, _ = call(c, "run", {"path": "/bin/sh", "args": [f"{T}/run", prog, *args], "env": env, "timeout_ms": 1500})
        m = re.search(r"\(call (\d+)\)", text)
        need(m is not None, f"{prog} {' '.join(args)} keeps running", text)
        self.n, self.text = int(m.group(1)), text

    def poll(self):
        _, text, _ = call(self.c, "output", {"call": self.n})
        self.text += "\n" + text
        open(os.path.join(v.RUN, f"{self.prog}.out"), "w").write(self.text)
        return self.text


def windows(c):
    """(id, app_id) of every window in `ctl list`."""
    return [(int(m.group(1)), m.group(2)) for m in re.finditer(r" id=(\d+) app_id=(\S*)", ctl(c, "list"))]


def body_of(c, wid):
    for l in ctl(c, "hot").splitlines():
        if l.startswith("win ") and re.search(rf" id={wid}\b", l):
            m = re.search(r"body=(-?\d+),(-?\d+),(\d+),(\d+)", l)
            if m:
                return tuple(int(x) for x in m.groups())
    return None


def crop(px, w, box):
    x0, y0, bw, bh = box
    return b"".join(px[((y0 + y) * w + x0) * 3:((y0 + y) * w + x0 + bw) * 3] for y in range(bh))


def colours(px):
    return len({px[i:i + 3] for i in range(0, len(px), 3 * 7)})


def windowed(c, q, comp, prog, args):
    before = {w for w, _ in windows(c)}
    p = Prog(c, prog, args)
    t0 = time.time()
    end = t0 + 90
    new = []
    while not new:
        new = [w for w in windows(c) if w[0] not in before]
        if time.time() > end:
            raise SystemExit(f"RED: {prog} opened no window in 90 s\n{p.poll()[-3000:]}\n{ctl(c, 'list')}")
        if ended(p.poll()):
            raise SystemExit(f"RED: {prog} ended before it opened a window\n{p.text[-3000:]}")
        time.sleep(0.5)
    wid, app = new[0]
    end = time.time() + 60
    while not re.search(rf"window {wid} \([^)]*\) of pid \d+: first frame", comp.poll()):
        if time.time() > end:
            raise SystemExit(f"RED: {prog}'s window {wid} drew no first frame\n{p.poll()[-2000:]}")
        time.sleep(0.5)
    first = time.time() - t0
    print(f"ok: {prog}: window {wid} (app_id {app or '-'}), first frame {first:.1f} s after the start", flush=True)
    box = body_of(c, wid)
    need(box is not None, f"{prog}: its window body in ctl hot", ctl(c, "hot"))
    time.sleep(max(0.0, 5 - (time.time() - t0)))
    w, h, a = q.screendump(f"-{prog}-a")
    time.sleep(max(0.0, SECS - 2 - (time.time() - t0)))
    _, _, b = q.screendump(f"-{prog}-b")
    ca, cb = crop(a, w, box), crop(b, w, box)
    moved = v.differing(ca, cb, box[2])
    print(f"{prog}: {colours(cb)} colours in the window, {moved} pixels changed between the two dumps", flush=True)
    need(colours(cb) >= 8, f"{prog}: the window shows a picture (not one flat colour)")
    need(moved > 500, f"{prog}: the picture moves ({moved} pixels changed in {SECS - 7} s)")
    while time.time() - t0 < SECS:
        time.sleep(0.5)
    need(ended(p.poll()) is None, f"{prog} still runs after {SECS} s", p.text[-2000:])
    ctl(c, "close")
    end = time.time() + 20
    while not ended(p.poll()):
        if time.time() > end:
            call(c, "stop", {"call": p.n})
            time.sleep(2)
            print(f"WARN: {prog} did not end within 20 s of ctl close; stopped by MCP", flush=True)
            break
        time.sleep(0.5)
    how = ended(p.poll())
    fps = re.findall(r"= *([\d.]+) FPS", p.text)
    REPORT["programs"][prog] = {"first_frame_s": round(first, 1), "changed_px": moved, "end": how, "fps": fps[-3:],
                                "dumps": [f"dump-{prog}-a.ppm", f"dump-{prog}-b.ppm"]}
    need(how is not None and how[0] == "exit", f"{prog} ended without a signal after ctl close ({how})", p.text[-3000:])
    print(f"{prog}: ended {how}; FPS lines (QEMU, not judged): {fps[-3:]}", flush=True)


def norm(line):
    """A failure line without what differs between runs and hosts (pointers, fd paths of pipes and logs)."""
    line = re.sub(r"arg=0x[0-9a-f]+", "arg=*", line)
    line = re.sub(r"fd=(pipe|socket|anon_inode):\[\d+\]", r"fd=\1", line)
    return re.sub(r"fd=/\S*?(/[^/,]+\.(out|log))", r"fd=LOG", line)


def main():
    c = v.connect(300)
    q = Qmp(v.QMP)
    sh(c, f"mkdir -p {WORK}/home && rm -f {WORK}/err-*.log")
    out = sh(c, "test -e /dev/dri && echo dri=present || echo dri=absent")
    need("dri=absent" in out, "V2 has no /dev/dri (software drivers only)", out)

    _, text, _ = call(c, "run", {"path": "/bin/sh", "args": [f"{T}/run", "--check"], "env": ENV, "timeout_ms": 120000})
    open(os.path.join(v.RUN, "check.out"), "w").write(text)
    need(ended(text) == ("exit", 0) and "resolve inside" in text and "render nodes: none" in text,
         "run --check: every library resolves inside the tree, no render node", text)

    t0 = time.time()
    _, text = mesa(c, "vulkaninfo", ["--summary"])
    need(ended(text) == ("exit", 0), f"vulkaninfo --summary ended with exit 0 ({time.time() - t0:.1f} s)", text[-3000:])
    need("DRIVER_ID_MESA_LLVMPIPE" in text and "PHYSICAL_DEVICE_TYPE_CPU" in text, "vulkaninfo lists lavapipe (llvmpipe, a CPU device)", text[-3000:])
    need("RADV" not in text, "RADV lists no device without /dev/dri", text[-3000:])
    _, text = mesa(c, "vulkaninfo", ["--summary"], tag="vulkaninfo-radv", extra={"MESA_VK_DRIVERS": "radeon_icd"})
    how = ended(text)
    need(how is not None and how[0] == "exit" and "RADV" not in text,
         f"vulkaninfo with RADV alone: no device, no crash ({how})", text[-3000:])
    REPORT["vulkaninfo_radv_only"] = {"end": how, "says": [l for l in text.splitlines() if "ERROR" in l or "rror" in l][:5]}

    t0 = time.time()
    _, text = mesa(c, "eglinfo", ["-B"])
    how = ended(text)
    need(how is not None and how[0] == "exit", f"eglinfo -B ended without a signal ({how}, {time.time() - t0:.1f} s)", text[-3000:])
    surf = text.split("Surfaceless platform:")[-1].split("Device platform:")[0] if "Surfaceless platform:" in text else ""
    need("renderer: llvmpipe" in surf, "eglinfo: llvmpipe on the surfaceless platform", text[-3000:])
    need(re.search(r"Platform Device platform:.*?renderer: llvmpipe", text, re.S) is not None,
         "eglinfo: llvmpipe on the device platform", text[-3000:])
    REPORT["eglinfo_end"] = how

    comp = v.Job(c, "/bin/bitscomp", ["--socket", v.SOCK, "--stats", "5"])
    comp.wait_for(r"backend=fbdev size=1280x800", "bitscomp started on fbdev at 1280x800")
    for prog, args in WINDOWED:
        windowed(c, q, comp, prog, args)
    ctl(c, "quit")
    comp.wait_for(r"bitscomp: ended", "bitscomp ended after ctl quit")
    open(os.path.join(v.RUN, "bitscomp.out"), "w").write(comp.text)

    # Failed calls: V2's against the host's (the same programs and tree, host-ref.sh).
    progs = ["vulkaninfo", "vulkaninfo-radv", "eglinfo"] + [p for p, _ in WINDOWED]
    v2, host = set(), set()
    for p in progs:
        t = read_file(c, f"{WORK}/err-{p}.log") or ""
        open(os.path.join(v.RUN, f"err-{p}.log"), "w").write(t)
        name = p.split("-")[0]
        v2 |= {norm(l) for l in t.splitlines() if l.startswith(f"errlog {name} ")}
        try:
            host |= {norm(l) for l in open(os.path.join(HOSTREF, f"err-{p}.log")).read().splitlines() if l.startswith(f"errlog {name} ")}
        except OSError:
            pass
    REPORT["failed_calls_v2_only"] = sorted(v2 - host)
    REPORT["failed_calls_both"] = sorted(v2 & host)
    print("failed calls on V2 only (errlog):")
    for l in REPORT["failed_calls_v2_only"]:
        print("  " + l)
    print("failed calls on V2 and the host alike:")
    for l in REPORT["failed_calls_both"]:
        print("  " + l)

    _, text, _ = call(c, "log", {"level": "info", "max": 2000})
    open(os.path.join(v.RUN, "klog-info.txt"), "w").write(text)
    gaps = list(dict.fromkeys(re.sub(r"^\S+\s+\[[^]]*\]\s*", "", l) for l in text.splitlines()
                              if re.search(r"syscall (enosys|enotty|enoprotoopt|nr\.invalid)", l)))
    faults = [l for l in text.splitlines() if re.search(r"fault|segv|sigill|sigbus", l, re.I)]
    REPORT["kernel_gaps"] = gaps
    REPORT["kernel_fault_lines"] = faults[:50]
    print("kernel events (ENOSYS/ENOTTY/ENOPROTOOPT, once per key since boot):")
    for l in gaps:
        print("  " + l)
    if faults:
        print("kernel lines naming faults:")
        for l in faults[:50]:
            print("  " + l)
    json.dump(REPORT, open(os.path.join(v.RUN, "report.json"), "w"), indent=1)
    print(f"report: {os.path.join(v.RUN, 'report.json')}")


if __name__ == "__main__":
    main()
