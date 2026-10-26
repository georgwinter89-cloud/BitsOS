#!/usr/bin/env python3
"""WY-4 scenario client (60-hung.sh): bitscomp never waits for a client (B4), on V2 in QEMU, driven over MCP.

Usage: v2_hung.py <mcp host port> <token> <qmp socket> <run dir>
Two rounds, the second with 64 busy threads (the freeze suite's `stress`) running: wyclient A animates (a frame per
callback, prints frame-interval p50/p99/max every 2 s), B stops itself with SIGSTOP, C answers normally. B must turn
"antwortet nicht" (MCP tool `windows` hung=true, grey veil on MCP screen), A must keep getting frame callbacks;
"Beenden" clicked with the pointer through the kernel's evdev path ends B by SIGTERM alone (bitscomp sends SIGCONT
with it, so the stopped process runs to its end); MCP `window_kill` on C, which ignores SIGTERM, needs the SIGKILL
2 s later; "Programm beenden" from A's circle menu ends A. Frame times are recorded, not judged (QEMU); only a loose
functional floor holds in both rounds: A gets at least FLOOR callbacks per 2 s and never waits for a buffer
(starved=0). Exit 0 = green."""
import json
import os
import re
import sys
import time

sys.dont_write_bytecode = True  # no __pycache__ in the tree
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
sys.argv = sys.argv[:5] + ["-"]
import v2_check as v  # noqa: E402
from v2_check import Job, call, ctl, need, screen  # noqa: E402

COLOURS = {"A": (0xc0, 0x30, 0x30), "B": (0x30, 0xa0, 0x40), "C": (0x30, 0x50, 0xc0)}
NUMBERS = []
# Loose functional floor for A in both rounds (QEMU timing is not judged; the loaded round gave 34-36 per 2 s).
FLOOR = 10


def windows(c):
    err, text, _ = call(c, "windows", {})
    if err:
        return None, text
    return json.loads(text), text


def wait_windows(c, cond, what, secs=30):
    end = time.time() + secs
    while True:
        ws, text = windows(c)
        if ws is not None and cond(ws):
            print(f"ok: {what}", flush=True)
            return ws
        if time.time() > end:
            raise SystemExit(f"RED: {what}\n{text}\n{ctl(c, 'list')}")
        time.sleep(0.5)


def by_title(ws, title):
    return next((w for w in ws if w["title"] == title), None)


def hot(c, title, field):
    for line in ctl(c, "hot").splitlines():
        if line.startswith("win ") and f'title="{title}"' in line:
            m = re.search(rf" {field}=(\S+)", line)
            if m:
                return [int(x) for x in m.group(1).split(",")]
    raise SystemExit(f"RED: no {field} for {title} in ctl hot\n{ctl(c, 'hot')}")


def mouse(c, **a):
    err, text, _ = call(c, "send_mouse", a)
    need(not err, f"send_mouse {a}", text)


def click(c, at, button="left"):
    mouse(c, x=at[0], y=at[1])
    time.sleep(0.2)
    mouse(c, button=button)


def pixel(img, x, y):
    w, _, px = img
    i = 3 * (y * w + x)
    return tuple(px[i:i + 3])


def frame_lines(job):
    return re.findall(r"frames n=(\d+) p50_ms=([\d.]+) p99_ms=([\d.]+) max_ms=([\d.]+) released=(\d+) starved=(\d+)", job.poll())


def round_(c, comp, tag, stressed):
    a_t, b_t, c_t = f"A{tag}", f"B{tag}", f"C{tag}"
    a = Job(c, "/bin/wyclient", ["--title", a_t, "--pattern", "0", "--animate"])
    wait_windows(c, lambda ws: by_title(ws, a_t), f"{tag}: A maps")
    b = Job(c, "/bin/wyclient", ["--title", b_t, "--pattern", "1", "--stop", "3000"])
    cc = Job(c, "/bin/wyclient", ["--title", c_t, "--pattern", "2", "--ignore-term"])
    ws = wait_windows(c, lambda ws: by_title(ws, b_t) and by_title(ws, c_t), f"{tag}: B and C map")
    for t in (a_t, b_t, c_t):
        w = by_title(ws, t)
        need(w["pid"] > 1 and w["app_id"] == "wyclient" and w["workspace"] == 0, f"{tag}: windows lists {t} with its pid", json.dumps(w))
    b.wait_for(r"stopping", f"{tag}: B stopped itself (SIGSTOP)")
    ws = wait_windows(c, lambda ws: by_title(ws, b_t)["hung"], f"{tag}: B is hung (windows hung=true)", 20)
    need(not by_title(ws, a_t)["hung"] and not by_title(ws, c_t)["hung"], f"{tag}: A and C are not hung", json.dumps(ws))
    comp.wait_for(rf"of pid {by_title(ws, b_t)['pid']}: antwortet nicht", f"{tag}: bitscomp logs 'antwortet nicht' for B")
    # A keeps its frame callbacks while B hangs: three more 2-s reports.
    seen = len(frame_lines(a))
    end = time.time() + 30
    while len(frame_lines(a)) < seen + 3:
        if time.time() > end:
            raise SystemExit(f"RED: {tag}: A's frame reports stopped while B hangs\n{a.text[-2000:]}")
        time.sleep(1)
    lines = frame_lines(a)[seen:]
    need(all(int(n) >= FLOOR for n, *_ in lines), f"{tag}: A gets at least {FLOOR} frame callbacks per 2 s while B hangs", a.text[-1500:])
    need(all(int(st) == 0 for *_, st in lines), f"{tag}: A never waits for a buffer release (starved=0)", a.text[-1500:])
    for n, p50, p99, mx, rel, starved in lines:
        NUMBERS.append(f"{tag} stress={stressed} A frames/2s={n} p50_ms={p50} p99_ms={p99} max_ms={mx} starved={starved}")
        print(f"numbers: {NUMBERS[-1]}", flush=True)
    need(int(lines[-1][4]) > 0, f"{tag}: A's buffers come back released", a.text[-500:])

    # The look: B's last frame under a grey veil, A untouched.
    img = screen(c, f"{tag}: hung")
    bx, by_, bw, bh = hot(c, b_t, "body")
    ax, ay, _, _ = hot(c, a_t, "body")
    corner, a_px = pixel(img, bx + 4, by_ + 4), pixel(img, ax + 4, ay + 4)
    need(a_px == COLOURS["A"], f"{tag}: A's picture on screen", str(a_px))
    need(corner != COLOURS["B"] and max(corner) - min(corner) <= 40, f"{tag}: B is grey under the veil", str(corner))
    # WY-5 look: the "Beenden" button's red border (#ff6b6b) left of its middle, and a "Warten" button.
    kx, ky = hot(c, b_t, "kill")[:2]
    need(any(pixel(img, x, ky)[0] > pixel(img, x, ky)[1] + 60 for x in range(max(0, kx - 120), kx)),
         f"{tag}: the red 'Beenden' button in B's middle")
    need(len(hot(c, b_t, "wait")) == 2, f"{tag}: a 'Warten' button next to it")

    # Beenden with the pointer (kernel evdev path); B is stopped: SIGTERM plus SIGCONT end it without SIGKILL.
    b_pid = by_title(windows(c)[0], b_t)["pid"]
    click(c, hot(c, b_t, "kill"))
    wait_windows(c, lambda ws: not by_title(ws, b_t), f"{tag}: Beenden removed B's window", 20)
    b.wait_for(r"ended", f"{tag}: B's process ended")
    time.sleep(2.5)
    need(not re.search(rf"of pid {b_pid}: still there", comp.poll()), f"{tag}: B ended by SIGTERM (with SIGCONT), no SIGKILL", comp.text[-1500:])

    # window_kill over MCP on C (it ignores SIGTERM, so the SIGKILL 2 s later ends it).
    cw = by_title(windows(c)[0], c_t)
    cid = cw["id"]
    err, text, _ = call(c, "window_kill", {"id": cid})
    need(not err and "SIGTERM sent" in text, f"{tag}: MCP window_kill {cid}", text)
    wait_windows(c, lambda ws: not by_title(ws, c_t), f"{tag}: window_kill removed C's window", 20)
    comp.wait_for(rf"of pid {cw['pid']}: still there 2 s after SIGTERM: SIGKILL", f"{tag}: C (ignores SIGTERM) needed SIGKILL")
    cc.wait_for(r"ended", f"{tag}: C's process ended")
    err, text, _ = call(c, "window_kill", {"id": cid})
    need(err or "rpc error" in text, f"{tag}: window_kill of a gone window is an error", text)

    # Programm beenden from A's circle menu (right click on its header, then the item).
    click(c, hot(c, a_t, "header"), "right")
    item = re.search(r'^item=5 at=(\d+),(\d+) enabled=true label="Programm beenden"', ctl(c, "hot"), re.M)
    need(item is not None, f"{tag}: the window menu has 'Programm beenden'", ctl(c, "hot"))
    click(c, (int(item.group(1)), int(item.group(2))))
    wait_windows(c, lambda ws: not by_title(ws, a_t), f"{tag}: Programm beenden removed A's window", 20)


def main():
    c = v.connect(300)
    comp = Job(c, "/bin/bitscomp", ["--socket", v.SOCK, "--stats", "5"])
    comp.wait_for(r"backend=fbdev size=1280x800", "bitscomp started on fbdev at 1280x800")
    comp.wait_for(r"MCP tools registered: windows, window_kill", "bitscomp registered windows and window_kill (MC-6)")
    grant = re.search(r"scheduling class (stays Normal|Interactive)[^\n]*", comp.text)
    print(f"note: {grant.group(0) if grant else 'no grant line'}", flush=True)
    err, text, _ = call(c, "windows", {})
    need(not err and text.strip() == "[]", "windows lists nothing before any client", text)

    round_(c, comp, "1", False)
    # 64 busy threads, as in the freeze suite (host/tests/freeze): started with run, they end by themselves.
    err, text, _ = call(c, "run", {"path": "stress", "args": ["-t", "64", "-s", "240"], "timeout_ms": 1500})
    m = re.search(r"\(call (\d+)\)", text)
    need(m is not None and "ended:" not in text, "64 busy threads run (stress)", text)
    stress = int(m.group(1))
    try:
        round_(c, comp, "2", True)
    finally:
        call(c, "stop", {"call": stress})
    ctl(c, "quit")
    comp.wait_for(r"bitscomp: ended", "bitscomp ended after ctl quit")
    bad = re.findall(r"bitscomp: client pid \d+ dropped[^\n]*", comp.text)
    need(not bad, "no client was dropped for an overflowing queue or a protocol error", "\n".join(bad))
    with open(os.path.join(v.RUN, "numbers.txt"), "w") as f:
        f.write("\n".join(NUMBERS) + "\n")
    print("GREEN: bitscomp never waits for a hung client", flush=True)


if __name__ == "__main__":
    main()
