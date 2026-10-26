#!/usr/bin/env python3
"""WY-1b scenario client (50-foot.sh): foot, CachyOS's Wayland terminal, in bitscomp on V2, driven over MCP.

Usage: v2_foot.py <mcp host port> <token> <qmp socket> <run dir>
bitscomp through MCP `run`; foot by Strg+Alt+Enter and by the circle menu's "Neues Terminal" (keys and pointer
through the kernel's evdev path); German text with umlauts and AltGr typed into foot's shell and read back with
read_file; clipboard from one foot to the other; foot's own resize (stty size) on tile changes; stack, float and
close by `bitscomp ctl` and by pointer clicks; `exit` in the shell closes its window; `ctl quit` gives the console
back. Startup time and memory of foot are printed (not judged: QEMU). Exit 0 = green."""
import base64
import json
import os
import re
import sys
import time

sys.dont_write_bytecode = True  # no __pycache__ in the tree
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
sys.argv = sys.argv[:5] + ["-"]
import v2_check as v  # noqa: E402
from v2_check import Job, Qmp, call, ctl, need, screen  # noqa: E402

TEXT = "Grüße aus Köln: äöü ÄÖÜ ß @ { } € | ~ [ ] \\"
CLIP = "Zwischenablage4711"
CLIP2 = "Weg4712"
# ENOSYS that foot's shell is known to meet. Any other ENOSYS of bitscomp, foot or the shell turns the run red.
# rseq: ENOSYS by contract (CONTRACTS.md AB-8), glibc goes on without it. setuid/setgid answer since INT-W4.
KNOWN_ENOSYS = {"rseq"}


def memfd_probe(c):
    """foot's buffer pools (four memfds of 512 MiB, two pages touched) on fresh 2 GB: CachyOS says "result ok"
    (50-foot.sh checks it), and so does V2 since FS-14 charges memfd pages when they are allocated; foot runs with
    its SHM scrolling (no tweak.max-shm-pool-size-mb override in terminal.rs)."""
    elf = base64.b64encode(open(os.environ["WY_MEMFD_PROBE"], "rb").read()).decode()
    _, text, _ = call(c, "run", {"elf_b64": elf, "path": "memfd-lazy", "timeout_ms": 30000})
    m = re.search(r"result (\S+)(?: at (\w+))?", text)
    need(m is not None, "the memfd probe ran", text)
    need(m.group(1) == "ok", "memfd pools are charged lazily, as on Linux (four 512 MiB pools fit)", text)


def pid_of(comp, win_id):
    m = re.search(rf"window {win_id} \(foot\) of pid (\d+):", comp.poll())
    need(m is not None, f"the pid of window {win_id} from its first-frame line", comp.text[-2000:])
    return m.group(1)


def windows(c):
    """The foot windows of `ctl list` as dicts (place, focused, x of the frame, id)."""
    out = []
    for l in ctl(c, "list").splitlines():
        m = re.match(r"(tile=\d+ page=\d+/\d+|float=\d+) visible=(\w+) focused=(\w+) frame=(-?\d+),(-?\d+) (\d+)x(\d+) id=(\d+) app_id=(\S*)", l)
        if m and m.group(9) == "foot":
            out.append({"place": m.group(1), "visible": m.group(2) == "true", "focused": m.group(3) == "true",
                        "x": int(m.group(4)), "w": int(m.group(6)), "id": int(m.group(8))})
    return out


def wait_windows(c, n, what, secs=40):
    end = time.time() + secs
    while True:
        ws = windows(c)
        if len(ws) == n:
            print(f"ok: {what}", flush=True)
            return ws
        if time.time() > end:
            raise SystemExit(f"RED: {what}: {len(ws)} foot windows, want {n}\n{ctl(c, 'list')}")
        time.sleep(0.3)


def hot(c):
    """`ctl hot` window lines by window id: header, float, close, content (x, y) and body (x, y, w, h) in pixels."""
    text = ctl(c, "hot")
    wins = {}
    for l in text.splitlines():
        if not l.startswith("win "):
            continue
        d = {k: tuple(int(n) for n in val.split(",")) for k, val in re.findall(r"(\w+)=(-?\d+(?:,-?\d+)+)", l)}
        wins[int(re.search(r" id=(\d+)", l).group(1))] = d
    return wins, text


def mouse(c, **a):
    err, text, _ = call(c, "send_mouse", a)
    need(not err, f"send_mouse {a}", text)


def click(c, at, button="left", action=None):
    mouse(c, x=at[0], y=at[1])
    a = {"button": button}
    if action:
        a["action"] = action
    mouse(c, **a)


def type_text(c, text):
    # send_keys takes a bounded number of strokes per call.
    for i in range(0, len(text), 40):
        err, out, _ = call(c, "send_keys", {"text": text[i:i + 40]})
        need(not err, f"send_keys {text[i:i + 40]!r}", out)


def keys(c, *k):
    err, out, _ = call(c, "send_keys", {"keys": list(k)})
    need(not err, f"send_keys {k}", out)


def read_file(c, path):
    err, text, _ = call(c, "read_file", {"path": path})
    if err:
        return None
    j = json.loads(text)
    return j["text"] if "text" in j else base64.b64decode(j["data"]).decode("utf-8", "replace")


def wait_file(c, path, pattern, what, secs=20, red=True):
    end = time.time() + secs
    while True:
        t = read_file(c, path)
        if t is not None and re.fullmatch(pattern, t, re.S):
            print(f"ok: {what}: {t.strip()!r}", flush=True)
            return t
        if time.time() > end:
            if not red:
                return None
            raise SystemExit(f"RED: {what}: {path} holds {t!r}")
        time.sleep(0.5)


LOST_INPUT = []


def typed_into_shell(c, typer, path, pattern, what):
    """`typer()` types a command that writes `path` in the focused foot. On V2 in QEMU the first keys of a line
    were lost now and then (the shell starts a fresh prompt line; not seen on CachyOS, plan/notes/wy-WY-1b.md
    open point, WY-1b change request 3): then a clean line and the whole line again, at most twice; a third loss
    is red. A clean line is Strg+C first: a lost closing quote leaves the shell in a continuation line that Strg+U
    alone does not leave (that turned one 50-foot run red in WY-4)."""
    typer()
    for attempt in (1, 2):
        t = wait_file(c, path, pattern, what, secs=10, red=False)
        if t is not None:
            return t
        LOST_INPUT.append(what)
        print(f"WARN: lost typed input ({what}, try {attempt}): {path} holds {read_file(c, path)!r}; typing the line again", flush=True)
        clean_line(c)
        typer()
    return wait_file(c, path, pattern, what)


def clean_line(c):
    """Back to a fresh, empty prompt line in the focused shell: Strg+C ends any continuation line, Strg+U
    empties what was typed after it."""
    keys(c, "ctrl+c")
    time.sleep(0.5)
    keys(c, "ctrl+u")


def focus_in(c, win_id, body):
    """A click into the middle of `body`, then wait until bitscomp has given that window the focus, so the keys
    typed next cannot reach the window that had it (QEMU: the click and the keys come from two devices)."""
    click(c, (body[0] + body[2] // 2, body[1] + body[3] // 2))
    end = time.time() + 10
    while not any(x["id"] == win_id and x["focused"] for x in windows(c)):
        if time.time() > end:
            raise SystemExit(f"RED: a click did not focus window {win_id}\n{ctl(c, 'list')}")
        time.sleep(0.2)
    time.sleep(0.2)


def show_word(c, word, marker):
    """`clear; echo word` in the focused foot; the marker file (the word again) says the shell got the whole line,
    then foot has a moment to draw it and its multi-click window since the focus click is over."""
    typed_into_shell(c, lambda: type_text(c, f"clear; echo {word}; echo {word} > {marker}\n"), marker,
                     re.escape(word) + r"\n", f"the shell ran clear; echo {word}")
    time.sleep(1.0)


def size_in(c, win_id, path, what):
    """Focus window `win_id` by a click into its body and run `stty size` in its shell: (rows, cols)."""
    w, _ = hot(c)
    focus_in(c, win_id, w[win_id]["body"])
    t = typed_into_shell(c, lambda: type_text(c, f"stty size > {path}\n"), path, r"\d+ \d+\n", what)
    r, k = map(int, t.split())
    return r, k


def foot_procs(c):
    _, text, _ = call(c, "ps", {})
    procs = []
    for l in text.splitlines():
        f = l.split()
        if len(f) > 13 and f[13].endswith("foot") and f[0].isdigit():
            procs.append((int(f[0]), f[11]))
    return procs


def wait_shells(c, n, what, secs=30):
    """n foot processes, each the leader of its own process group (bitscomp's spawn), each with its shell asleep
    (state S: it waits for input). Keys typed before that can meet a shell that is still starting."""
    end = time.time() + secs
    while True:
        rows = [l.split() for l in call(c, "ps", {})[1].splitlines()]
        rows = [f for f in rows if len(f) > 13 and f[0].isdigit()]
        foots = [f for f in rows if f[13].endswith("foot")]
        ready = [f for f in foots if any(g[1] == f[0] and g[13].endswith("sh") and g[5] == "S" for g in rows)]
        if len(foots) == n and len(ready) == n:
            need(all(f[2] == f[0] for f in foots), f"{what}: each foot leads its own process group",
                 "\n".join(" ".join(f) for f in foots))
            time.sleep(0.3)
            return
        if time.time() > end:
            raise SystemExit(f"RED: {what}: {len(ready)} of {len(foots)} foot shells ready, want {n}\n"
                             + "\n".join(" ".join(f) for f in rows))
        time.sleep(0.3)


def main():
    c = v.connect(300)
    global COMP, QM
    q = QM = Qmp(v.QMP)
    print("memory before the probe: " + call(c, "ps", {})[1].splitlines()[0], flush=True)
    if not os.environ.get("WY_SKIP_PROBE"):
        memfd_probe(c)
    print("memory after the probe: " + call(c, "ps", {})[1].splitlines()[0], flush=True)
    comp = COMP = Job(c, "/bin/bitscomp", ["--socket", v.SOCK, "--stats", "5"])
    comp.wait_for(r"backend=fbdev size=1280x800", "bitscomp started on fbdev at 1280x800")
    need(re.search(r"started programs also get SHELL=/bin/sh LC_CTYPE=C\.UTF-8 XDG_CACHE_HOME=/tmp/wy/cache", comp.text) is not None,
         "bitscomp fills the started programs' environment (shell, locale, font cache)", comp.text[-2000:])

    # Strg+Alt+Enter through the kernel's keyboard: bitscomp starts foot.
    keys(c, "ctrl+alt+enter")
    (a,) = wait_windows(c, 1, "Strg+Alt+Enter opened a foot window")
    comp.wait_for(r"\(foot\) of pid \d+: first frame \d+ ms after its start", "foot drew its first frame")
    wait_shells(c, 1, "the first foot's shell is ready")
    rows1, cols1 = size_in(c, a["id"], "/tmp/wy-size1", "foot alone: stty size in its shell")

    # The circle menu's "Neues Terminal" by pointer: right click on empty space, left click on the item.
    w, text = hot(c)
    click(c, tuple(int(n) for n in re.search(r"^empty at=(\d+),(\d+)", text, re.M).groups()), "right")
    _, text = hot(c)
    m = re.search(r'^item=\d+ at=(\d+),(\d+) enabled=true label="Neues Terminal"', text, re.M)
    need(m is not None, "the circle menu offers Neues Terminal", text)
    click(c, (int(m.group(1)), int(m.group(2))))
    ws = wait_windows(c, 2, "Neues Terminal opened a second foot window")
    starts = re.findall(r"\(foot\) of pid \d+: first frame (\d+) ms after its start", comp.poll())
    end = time.time() + 20
    while len(starts) < 2 and time.time() < end:
        time.sleep(0.3)
        starts = re.findall(r"\(foot\) of pid \d+: first frame (\d+) ms after its start", comp.poll())
    need(len(starts) == 2, "both foot windows drew a frame", comp.text[-2000:])
    print(f"foot startup (spawn to first frame, QEMU, not judged): {', '.join(starts)} ms", flush=True)
    tiles = sorted((x for x in ws), key=lambda x: x["x"])
    need(all(x["visible"] and x["place"].startswith("tile=") for x in ws) and tiles[0]["place"] != tiles[1]["place"]
         and tiles[0]["x"] + tiles[0]["w"] <= tiles[1]["x"], "two foot windows side by side", ctl(c, "list"))
    b = next(x for x in ws if x["id"] != a["id"])
    need(b["focused"], "the new terminal has the focus")
    wait_shells(c, 2, "both foot shells are ready")

    # foot resizes itself when its tile shrinks: fewer columns, the same rows.
    rows2, cols2 = size_in(c, a["id"], "/tmp/wy-size2", "foot beside another: stty size")
    need(rows2 == rows1 and cols2 < cols1, f"foot resized to its half tile ({rows1}x{cols1} -> {rows2}x{cols2})")

    # German text with umlauts and AltGr characters into the shell of b.
    w, _ = hot(c)
    bb = w[b["id"]]["body"]
    focus_in(c, b["id"], bb)
    typed_into_shell(c, lambda: type_text(c, f"printf '%s\\n' '{TEXT}' > /tmp/wy-text.txt\n"), "/tmp/wy-text.txt",
                     re.escape(TEXT) + r"\n", "German text typed into foot arrived in the shell")

    # Clipboard: a word in a (double click selects it, Strg+Umschalt+C), pasted in b (Strg+Umschalt+V). More
    # tries when the paste is not the word (typed input lost now and then on V2 in QEMU, notes); a third miss is
    # red.
    w, _ = hot(c)
    ab = w[a["id"]]["body"]
    for attempt in (1, 2, 3):
        focus_in(c, a["id"], ab)
        show_word(c, CLIP, f"/tmp/wy-shown{attempt}")
        click(c, (ab[0] + 6, ab[1] + 8), action="double")
        time.sleep(0.3)
        keys(c, "ctrl+shift+c")
        time.sleep(0.3)
        focus_in(c, b["id"], bb)
        type_text(c, "echo ")
        keys(c, "ctrl+shift+v")
        time.sleep(1.0)  # the paste travels foot a -> pipe -> foot b -> pty
        type_text(c, f" > /tmp/wy-clip{attempt}.txt\n")
        got = None
        end = time.time() + 20
        while (got is None or not got.endswith("\n")) and time.time() < end:
            got = read_file(c, f"/tmp/wy-clip{attempt}.txt")
            time.sleep(0.5)
        if got == CLIP + "\n":
            print(f"ok: copied in one foot, pasted in the other: {got.strip()!r}", flush=True)
            break
        print(f"WARN: copy and paste try {attempt} gave {got!r}", flush=True)
    else:
        raise SystemExit("RED: copied in one foot, pasted in the other: wrong three times")

    # Stack by ctl: one tile, two pages; the shown page is as wide as the whole area again.
    ctl(c, "stack")
    end = time.time() + 10
    while not all(x["place"].startswith("tile=0 page=") and x["place"].endswith("/2") for x in windows(c)):
        if time.time() > end:
            raise SystemExit(f"RED: ctl stack\n{ctl(c, 'list')}")
        time.sleep(0.3)
    print("ok: ctl stack put both terminals into one tile (tabs)", flush=True)
    shown = next(x for x in windows(c) if x["visible"])
    rows3, cols3 = size_in(c, shown["id"], "/tmp/wy-size3", "foot as a tab of the whole area: stty size")
    need(cols3 > cols2, f"foot grew with the stacked tile ({cols2} -> {cols3} columns)")
    # A click on the hidden tab shows that terminal (pointer).
    _, text = hot(c)
    m = re.search(r"^tab tile=0 page=\d+/2 shown=false id=(\d+) at=(\d+),(\d+)", text, re.M)
    need(m is not None, "the stacked tile shows two tabs", text)
    click(c, (int(m.group(2)), int(m.group(3))))
    end = time.time() + 10
    while not any(x["visible"] and x["id"] == int(m.group(1)) for x in windows(c)):
        if time.time() > end:
            raise SystemExit(f"RED: tab click\n{ctl(c, 'list')}")
        time.sleep(0.3)
    print("ok: a click on the other tab shows that terminal", flush=True)
    # Drag (pointer down, eight moves, up) the second tab onto the left edge of the first: the order changes.
    _, text = hot(c)
    t0 = re.search(r"^tab tile=0 page=0/2 shown=\w+ id=(\d+) at=\d+,\d+ close=\S+ rect=(\d+),(\d+)", text, re.M)
    t1 = re.search(r"^tab tile=0 page=1/2 shown=\w+ id=(\d+) at=(\d+),(\d+)", text, re.M)
    need(t0 is not None and t1 is not None, "both tabs in ctl hot", text)
    (x1, y1), (x2, y2) = (int(t1.group(2)), int(t1.group(3))), (int(t0.group(2)) + 4, int(t1.group(3)))
    mouse(c, x=x1, y=y1)
    mouse(c, button="left", action="down")
    for i in range(1, 9):
        mouse(c, x=x1 + (x2 - x1) * i // 8, y=y1 + (y2 - y1) * i // 8)
    mouse(c, button="left", action="up")
    moved = int(t1.group(1))
    end = time.time() + 10
    while not any(x["id"] == moved and x["place"].startswith("tile=0 page=0/2") for x in windows(c)):
        if time.time() > end:
            raise SystemExit(f"RED: tab drag\n{ctl(c, 'list')}\n{hot(c)[1]}")
        time.sleep(0.3)
    print("ok: dragging a tab by the pointer put it first", flush=True)
    ctl(c, "unstack")
    wait_windows(c, 2, "ctl unstack")
    need(len({x["place"].split()[0] for x in windows(c)}) == 2, "two tiles again after ctl unstack", ctl(c, "list"))

    # Float by the header's float button (pointer), back to a tile by ctl.
    focused = next(x for x in windows(c) if x["focused"])
    w, _ = hot(c)
    click(c, w[focused["id"]]["float"])
    end = time.time() + 10
    while not any(x["place"].startswith("float=") for x in windows(c)):
        if time.time() > end:
            raise SystemExit(f"RED: float button\n{ctl(c, 'list')}")
        time.sleep(0.3)
    print("ok: the float button made a terminal float", flush=True)
    ctl(c, "float")
    end = time.time() + 10
    while any(x["place"].startswith("float=") for x in windows(c)):
        if time.time() > end:
            raise SystemExit(f"RED: ctl float back to a tile\n{ctl(c, 'list')}")
        time.sleep(0.3)
    print("ok: ctl float put it back into a tile", flush=True)

    # Memory of the running foots (ps RSS and /proc/<pid>/status), printed, not judged.
    for pid, rss in foot_procs(c):
        st = read_file(c, f"/proc/{pid}/status") or ""
        vm = {k: val.strip() for k, val in re.findall(r"^(VmRSS|VmHWM|VmSize|Threads):\s*(.*)$", st, re.M)}
        print(f"foot pid {pid}: ps RSS_KB={rss} {vm}", flush=True)
    need(len(foot_procs(c)) == 2, "two foot processes run")

    # Close by ctl: the focused foot gets xdg close and ends.
    ctl(c, "close")
    (left,) = wait_windows(c, 1, "ctl close closed one terminal")
    end = time.time() + 10
    while len(foot_procs(c)) != 1:
        if time.time() > end:
            raise SystemExit("RED: the closed foot did not end")
        time.sleep(0.3)
    print("ok: its foot process ended", flush=True)

    # `exit` in the other's shell: foot ends and its window goes.
    w, _ = hot(c)
    lb = w[left["id"]]["body"]
    focus_in(c, left["id"], lb)
    type_text(c, "exit\n")
    end = time.time() + 10
    while windows(c) and time.time() < end:
        time.sleep(0.3)
    if windows(c):
        LOST_INPUT.append("exit")
        print("WARN: lost typed input (exit); typing the line again", flush=True)
        clean_line(c)
        type_text(c, "exit\n")
    wait_windows(c, 0, "exit in foot's shell closed its window")

    # Close by the pointer: a new foot (Strg+Alt+Enter), its header's close button.
    keys(c, "ctrl+alt+enter")
    (d,) = wait_windows(c, 1, "Strg+Alt+Enter again")
    time.sleep(1.0)
    w, _ = hot(c)
    click(c, w[d["id"]]["close"])
    wait_windows(c, 0, "the close button closed the terminal")
    end = time.time() + 10
    while foot_procs(c):
        if time.time() > end:
            raise SystemExit("RED: a foot process stayed")
        time.sleep(0.3)
    print("ok: no foot process left", flush=True)

    # Hostile: a foot killed (SIGKILL) while it holds the clipboard. bitscomp drops its window, stays responsive,
    # and a paste in the other foot gives nothing (no hang).
    keys(c, "ctrl+alt+enter")
    (y,) = wait_windows(c, 1, "a terminal that stays")
    keys(c, "ctrl+alt+enter")
    (x,) = [w for w in wait_windows(c, 2, "a terminal that will be killed") if w["id"] != y["id"]]
    wait_shells(c, 2, "both shells of the last pair are ready")
    time.sleep(1.0)
    w, _ = hot(c)
    xb, yb = w[x["id"]]["body"], w[y["id"]]["body"]
    focus_in(c, x["id"], xb)
    show_word(c, CLIP2, "/tmp/wy-shown-k")
    click(c, (xb[0] + 6, xb[1] + 8), action="double")
    time.sleep(0.3)
    keys(c, "ctrl+shift+c")
    time.sleep(0.5)
    pid = pid_of(comp, x["id"])
    err, text, _ = call(c, "run", {"path": "/bin/kill", "args": ["-KILL", pid], "env": v.ENV, "timeout_ms": 5000})
    need(not err, f"SIGKILL to foot pid {pid}", text)
    (left2,) = wait_windows(c, 1, "the killed foot's window is gone")
    need(left2["id"] == y["id"], "the other terminal is still there", ctl(c, "list"))
    focus_in(c, y["id"], yb)

    def paste_line():
        type_text(c, "echo ")
        keys(c, "ctrl+shift+v")
        time.sleep(1.0)
        type_text(c, " > /tmp/wy-clip2.txt\n")
    typed_into_shell(c, paste_line, "/tmp/wy-clip2.txt", r"\n",
                     "a paste after the clipboard owner was killed gives nothing, the shell goes on")

    # The foot still open when bitscomp quits ends with it; the console comes back.
    ctl(c, "quit")
    comp.wait_for(r"bitscomp: ended", "bitscomp ended after ctl quit")
    end = time.time() + 10
    while foot_procs(c):
        if time.time() > end:
            raise SystemExit("RED: foot outlived bitscomp")
        time.sleep(0.3)
    print("ok: foot ended with bitscomp", flush=True)
    # The console may still be printing (kernel events at loglevel info): compare until two pictures agree.
    end, n = time.time() + 15, 0
    while True:
        n += 1
        sw, sh, px = screen(c, "after quit")
        dw, dh, dpx = q.screendump(n)
        if (dw, dh) == (sw, sh) and v.differing(px, dpx, sw) < sw * sh // 200:
            break
        if time.time() > end:
            raise SystemExit("RED: after quit: MCP screen and the QEMU screendump never agreed")
        time.sleep(1)
    print("ok: after quit: MCP screen == screendump (console)", flush=True)
    if LOST_INPUT:
        print(f"WARN: typed lines lost once and typed again: {len(LOST_INPUT)} ({'; '.join(LOST_INPUT)})", flush=True)
    _, text, _ = call(c, "screen", {"format": "text"})
    need("$" in text or "#" in text, "the console shows the shell again", text[-1500:])
    if re.search(r"No writable cache|Fontconfig error", comp.text):
        raise SystemExit(f"RED: fontconfig complained\n{comp.text[-3000:]}")
    print("bitscomp and foot stderr (warnings, once each):")
    for l in dict.fromkeys(l.strip() for l in comp.text.splitlines() if re.match(r"\s*(warn|err):", l)):
        print("  " + l)
    # What the programs met in the kernel: ENOSYS (logged once per syscall) and the info events about programs.
    _, text, _ = call(c, "log", {"level": "info", "max": 2000})
    open(os.path.join(v.RUN, "klog-info.txt"), "w").write(text)
    print("kernel events naming foot, fontconfig or the shell, or ENOSYS:")
    for l in dict.fromkeys(re.sub(r"^\S+\s+\[[^]]*\]\s*", "", l) for l in text.splitlines()
                           if re.search(r"enosys|program=(foot|sh)\b|enotty|unsupported|not_built", l)):
        print("  " + l)
    # ENOSYS is logged once per syscall number since boot (crates/syscall/src/lib.rs), so a number another program
    # met first is not seen here; the known ones stay listed in the notes.
    new = sorted({m.group(1) for m in re.finditer(r"syscall enosys nr=\d+ name=(\S+) program=(?:foot|sh|bitscomp)\b", text)}
                 - KNOWN_ENOSYS)
    need(not new, f"no ENOSYS for bitscomp, foot or its shell beyond the known {sorted(KNOWN_ENOSYS)}", ", ".join(new))


COMP = None
QM = None

if __name__ == "__main__":
    try:
        main()
    except SystemExit as e:
        if e.code not in (0, None) and COMP is not None:
            print(str(e.code))
            print("--- bitscomp and its programs, last output ---")
            try:
                COMP.poll()
            except Exception as x:  # the call may be gone already
                print(f"(no more output: {x})")
            print(COMP.text[-4000:])
            try:  # the picture at the red step, for the reader of the run
                QM.screendump("-red")
                print(f"screen at the red step: {os.path.join(v.RUN, 'dump-red.ppm')}")
            except Exception as x:
                print(f"(no screendump: {x})")
            raise SystemExit(1)
        raise
