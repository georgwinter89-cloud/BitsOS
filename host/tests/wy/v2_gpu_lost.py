#!/usr/bin/env python3
"""WY-21 scenario client (48-v2-gpu-lost.sh): the GPU desktop's way back from a lost device, over MCP.

Usage: v2_gpu_lost.py <mcp host port> <token> <qmp socket> <run dir> lost|stall
Exit 0 = green. Timing is never judged (QEMU): how long a way took is printed, the bounds are generous."""
import os
import re
import sys
import time

sys.dont_write_bytecode = True  # no __pycache__ in the tree
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
MODE = sys.argv[5]
sys.argv = sys.argv[:5] + ["-"]
import v2_check as v  # noqa: E402
from v2_check import Job, call, connect, differing, need, screen, shell  # noqa: E402

SOCK = "wayland-1"
NODE = "/dev/dri/renderD128"
JOBS = []


def ctl(c, *words, sock=SOCK):
    _, text, _ = call(c, "run", {"path": "/bin/bitscomp", "args": ["ctl", "--socket", sock, *words], "env": v.ENV, "timeout_ms": 30000})
    return text


def stat(c, key):
    m = re.search(rf"\b{key}=(\w+)", ctl(c, "stats"))
    return m.group(1) if m else None


def shape(c):
    """The session as bitscomp would save it, without its time and the recent list: per workspace its tiles with their
    programs. The tiles of one workspace as a sorted list: restarted programs map in whatever order they come up."""
    spaces = []
    for l in ctl(c, "session").splitlines():
        if l.startswith("ws "):
            spaces.append((l, []))
        elif l == "tile" and spaces:
            spaces[-1][1].append([])
        elif l.startswith("prog ") and spaces and spaces[-1][1]:
            spaces[-1][1][-1].append(l)
    return [f"{name}: {sorted(' + '.join(t) for t in tiles)}" for name, tiles in spaces]


POINTER = [0]


def moving(c, what):
    """New frames reach the screen: the pointer (drawn into the frames here) moved between two looks. The windows'
    own animation cannot tell: it alternates two pictures, so two looks an even number of frames apart are the same."""
    POINTER[0] += 1
    x = 200 + 160 * (POINTER[0] % 4)
    ctl(c, "pointer", str(x), "500")
    time.sleep(1)
    w, _, a = screen(c, f"{what}, first look")
    ctl(c, "pointer", str(x + 80), "560")
    time.sleep(1)
    _, _, b = screen(c, f"{what}, second look")
    n = differing(a, b, w)
    need(n > 50, f"{what}: the picture keeps changing", f"{n} pixels differ")


def start(c):
    comp = Job(c, "/bin/bitscomp", ["--socket", SOCK])
    JOBS.append(comp)
    comp.wait_for(r"build gpu, renderer gles \(CONFIG.TXT bitscomp=auto\): handing over", "bitscomp=auto hands over to the gpu build", 120)
    comp.wait_for(r"the rescue build waits as pid \d+ beside the gpu build \(pid \d+\)", "the rescue build waits beside it", 30)
    m = re.search(r"waits as pid (\d+) beside the gpu build \(pid (\d+)\)", comp.text)
    parent, child = m.group(1), m.group(2)
    comp.wait_for(rf"WAYLAND_DISPLAY={SOCK} .*pid={child} backend=fbdev size=1280x800 .*build=gpu renderer=gles", "the gpu build draws with GLES", 120)
    comp.wait_for(rf"gpu: watching {NODE}", "it watches the render node")
    need("bitscomp-rescue" in shell(c, f"cat /proc/{parent}/comm"), "the waiting rescue build is named bitscomp-rescue")
    return comp, parent, child


def windows(c, comp):
    ctl(c, "spawn", "wyclient", "--animate", "--title", "one")
    ctl(c, "ws", "new")
    ctl(c, "ws", "2")
    ctl(c, "spawn", "wyclient", "--animate", "--title", "two")
    wait_windows(c, comp, "both windows mapped")
    # The session is saved 2 s after a change (session.rs SAVE_AFTER).
    time.sleep(6)
    s = shape(c)
    need(len(s) == 2 and sum(l.count("wyclient") for l in s) == 2, "two workspaces, two programs", "\n".join(s))
    return s


def wait_windows(c, comp, what, secs=60):
    end = time.time() + secs
    while True:
        lst = ctl(c, "list")
        ws = ctl(c, "ws", "1") + ctl(c, "list") + ctl(c, "ws", "2") + ctl(c, "list")
        if 'title="one"' in ws and 'title="two"' in ws:
            break
        if time.time() > end:
            raise SystemExit(f"RED: {what}\n{lst}\n{ctl(c, 'ws')}\n{comp.poll()[-2500:]}")
        time.sleep(1)
    print(f"ok: {what}", flush=True)


def taken_over(c, comp, parent, before, why):
    comp.wait_for(rf"the gpu build {why}: the rescue build takes the screen, with the session", f"the rescue build takes over ({why})", 60)
    comp.wait_for(rf"WAYLAND_DISPLAY={SOCK} .*pid={parent} backend=fbdev size=1280x800 .*build=rescue renderer=pixman", "the rescue build draws", 60)
    comp.wait_for(r"the gpu build's session comes back \(2 tiles, 2 programs\)", "the session comes back without the start page's offer", 60)
    wait_windows(c, comp, "both windows are back")
    end = time.time() + 60
    while shape(c) != before:
        if time.time() > end:
            raise SystemExit(f"RED: the session after the takeover is not the one before\n{before}\n{shape(c)}")
        time.sleep(1)
    print("ok: the same workspaces, tiles and programs as before", flush=True)
    moving(c, "after the takeover")


def lost(c):
    comp, parent, child = start(c)
    before = windows(c, comp)
    moving(c, "on the GPU")
    t0 = time.time()
    out = shell(c, "/bin/abi-drm-jobs hang", timeout_ms=60000)
    need("done" in out, "abi-drm-jobs hangs the fake GPU", out[-1500:])
    comp.wait_for(rf"gpu: a new open of {NODE} answers .*: the desktop goes on with pixman, the windows stay", "the gpu build sees the lost device", 60)
    print(f"info: noticed {time.time() - t0:.1f} s after the hang's job ended", flush=True)
    comp.wait_for(r"gpu: renderer=pixman from now on", "it draws on with Pixman in the same process")
    need(stat(c, "renderer") == "pixman", "ctl stats says renderer=pixman")
    need("rescue build takes the screen" not in comp.poll(), "no takeover: the same process goes on", comp.text[-1500:])
    wait_windows(c, comp, "both windows stayed", secs=10)
    need(shape(c) == before, "the session is untouched", "\n".join(shape(c)))
    moving(c, "after the switch to Pixman")
    # Mesa ends a non-robust context's process when a submission fails: the waiting rescue build takes over.
    shell(c, f"kill -ABRT {child}")
    taken_over(c, comp, parent, before, r"crashed \(signal 6\)")
    ctl(c, "quit")
    comp.wait_for(r"bitscomp: ended", "the rescue build ends on ctl quit")
    again = Job(c, "/bin/bitscomp", ["--socket", "wayland-2"])
    JOBS.append(again)
    again.wait_for(rf"build rescue \(CONFIG.TXT bitscomp=auto\): the GPU is lost \(a new open of {NODE} answers", "bitscomp=auto stays with the rescue build on a lost GPU", 60)
    again.wait_for(r"WAYLAND_DISPLAY=wayland-2 .*build=rescue renderer=pixman", "the rescue build draws")
    need("handing over" not in again.text, "no hand-over to a lost GPU", again.text[-1500:])
    ctl(c, "quit", sock="wayland-2")


def stall(c):
    comp, parent, child = start(c)
    before = windows(c, comp)
    moving(c, "on the GPU")
    t0 = time.time()
    out = ctl(c, "stall", "40")
    need("the next present waits 40" in out, "ctl stall is taken", out)
    comp.wait_for(r"gpu: a present has not returned for \d+ ms \(stuck in the driver\): ending with 75", "the stall watch ends the stuck gpu build", 60)
    print(f"info: ended {time.time() - t0:.1f} s after the stall began", flush=True)
    taken_over(c, comp, parent, before, "gave the GPU up")
    need(not re.search(rf"\b{child}\b", shell(c, "pidof bitscomp")), "the gpu build is gone", shell(c, "pidof bitscomp"))
    ctl(c, "quit")
    comp.wait_for(r"bitscomp: ended", "the rescue build ends on ctl quit")
    # The waiting rescue build killed: the gpu build gave root up after its parent-death signal was set (the kernel
    # clears it with the credentials) and must end too. Root only from the console's shell (MCP runs as uid 1000).
    log = "/tmp/wy21-root.log"
    call(c, "send_keys", {"keys": ["enter"]})
    call(c, "send_keys", {"text": f"(setsid bitscomp --socket wayland-9 </dev/null >{log} 2>&1 &)\n"})
    end = time.time() + 120
    while not re.search(r"now running as uid 1000", text := shell(c, f"cat {log}")) or "waits as pid" not in text:
        if time.time() > end:
            raise SystemExit(f"RED: the root start did not hand over and give root up\n{text[-2000:]}")
        time.sleep(1)
    m = re.search(r"waits as pid (\d+) beside the gpu build \(pid (\d+)\)", text)
    parent, child = m.group(1), m.group(2)
    need(re.search(rf"pid={child} backend=fbdev", text) is not None, "the gpu build gave root up", text[-1500:])
    # MCP's kill acts as root (the console's keyboard is the desktop's now).
    err, out, _ = call(c, "kill", {"pid": int(parent), "signal": "SIGKILL"})
    need(not err, f"SIGKILL to the rescue build {parent}", out)
    end = time.time() + 15
    while os.path.basename(shell(c, f"cat /proc/{child}/comm 2>/dev/null || echo gone").strip()) != "gone":
        if time.time() > end:
            raise SystemExit(f"RED: the gpu build {child} outlived its killed rescue build {parent}\n{shell(c, f'cat {log}')[-1500:]}")
        time.sleep(0.5)
    print(f"ok: the gpu build {child} ended with its killed rescue build {parent}", flush=True)


def main():
    c = connect(300)
    try:
        {"lost": lost, "stall": stall}[MODE](c)
    finally:
        for i, job in enumerate(JOBS):
            try:
                job.poll()
            except Exception:  # noqa: BLE001 - the record of a red run must not hide its reason
                pass
            with open(os.path.join(v.RUN, f"job-{i}.txt"), "w") as f:
                f.write(job.text)
    print(f"GREEN: {MODE}")


if __name__ == "__main__":
    main()
