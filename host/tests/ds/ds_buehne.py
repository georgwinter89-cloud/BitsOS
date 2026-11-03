#!/usr/bin/env python3
"""DS-3 on the host: the Bühne preset's behaviour, drawn by headless bitscomp (built-in defaults, fixed status
values, two wyclient windows, 2560x1440).

Usage: ds_buehne.py <bitscomp> <wyclient> <dir> <WxH>
Prints `ok:` lines (and `skip:` for checks that need CR DS-3-1); exit 0 = green. Checks: the Bühne is the default
(the tiles' area is the stage, no classic list or favourites); the workspace chips switch (click) and "+" adds one;
the context address is read-only (a click does not start editing) while Super+L still edits; an Absicht headline
(desktop.absicht.<slug>) moves the tiles down and back; Claude's ring folds the right column and the tiles grow;
Startseite shows and hides the start page; a minimised window comes back from its chip in the bottom row; the sun
switches light and dark (CR DS-3-1); the hint line counts the programs hung on the shown workspace only;
the still desktop draws 0 frames (with a headline too); classic is a preset."""
import os
import re
import subprocess
import sys
import time

BIN, CLIENT, DIR, SIZE = sys.argv[1:5]
SOCK = "wayland-dsbuehne"
env = dict(os.environ, XDG_RUNTIME_DIR=DIR, WAYLAND_DISPLAY=SOCK)
env.pop("DISPLAY", None)


def need(cond, what, detail=""):
    if not cond:
        raise SystemExit(f"RED: {what}\n{detail}")
    print(f"ok: {what}", flush=True)


def ctl(*words):
    return subprocess.run([BIN, "ctl", "--socket", SOCK, *words], env=env, capture_output=True, text=True, timeout=20).stdout


def wait_for(pred, what):
    for _ in range(100):
        if pred():
            return
        time.sleep(0.1)
    raise SystemExit(f"RED: {what}\n{ctl('hot')}")


def hot_at(pattern, field="at"):
    for line in ctl("hot").splitlines():
        if pattern in line and f" {field}=" in line:
            v = line.split(f" {field}=")[1].split()[0]
            return None if v == "-" else v.split(",")
    return None


def click(xy):
    ctl("click", *xy)
    time.sleep(0.3)


def work():
    m = re.search(r"work=(-?\d+),(-?\d+),(\d+),(\d+)", ctl("hot"))
    need(m is not None, "the work area is listed", ctl("hot"))
    return tuple(int(v) for v in m.groups())


def body(title):
    v = hot_at(f'title="{title}"', "body")
    need(v is not None, f"window {title} is listed")
    return tuple(int(x) for x in v)


def frames():
    return int(ctl("stats").split("frames=")[1].split()[0])


def still(secs, what):
    ctl("pointer", "4", "4")
    time.sleep(1.0)
    f0 = frames()
    time.sleep(secs)
    f1 = frames()
    need(f1 == f0, f"{what}: 0 frames in {secs} s (frames={f0}, then {f1})")


def hint_box(name):
    """The bottom row's hint (left of the Feld bar) from a screenshot: board (48, 1240) 400x64 at 4/3."""
    path = os.path.join(DIR, f"hint-{name}.ppm")
    ctl("shot", path)
    with open(path, "rb") as f:
        data = f.read()
    parts, at = [], 0
    while len(parts) < 4:
        while data[at:at + 1].isspace():
            at += 1
        start = at
        while not data[at:at + 1].isspace():
            at += 1
        parts.append(data[start:at])
    w, pix = int(parts[1]), data[at + 1:]
    x0, y0, bw, bh = 64, 1306, 533, 85
    return b"".join(pix[((y0 + r) * w + x0) * 3:((y0 + r) * w + x0 + bw) * 3] for r in range(bh))


procs = []
comp = subprocess.Popen([BIN, "--backend", "headless", "--size", SIZE, "--socket", SOCK, "--fixed-status",
                         "--settings", "none", "--mcp", "none"], env=env,
                        stdout=subprocess.DEVNULL, stderr=open(os.path.join(DIR, "bitscomp.err"), "w"))
procs.append(comp)
try:
    wait_for(lambda: os.path.exists(os.path.join(DIR, SOCK + ".ctl")), "no control socket")
    for n, title in enumerate(["one", "two", "three"]):
        procs.append(subprocess.Popen([CLIENT, "--title", title, "--pattern", str(n % 2)], env=env,
                                      stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL))
        wait_for(lambda: f'title="{title}"' in ctl("list"), f"wyclient {title} did not map")
    time.sleep(0.5)
    hot = ctl("hot")
    has_cr = "\nbuehne sun=" in hot
    # The Bühne is the default: the stage at (48, 80) board px = (51, 85) logical, no favourites, no list.
    wx, wy, ww, wh = work()
    need((wx, wy) == (51, 85) and 1400 < ww < 1500 and 700 < wh < 800, f"the tiles' area is the stage ({wx},{wy},{ww},{wh})")
    need("\nfav " not in hot and "wsfold=0," in hot, "no favourites bar and no workspace list in the Bühne")

    # The chips: "+" adds a workspace (its chip appears, it is shown), a click on chip 1 goes back.
    click(hot_at("wsnew"))
    wait_for(lambda: "ws 2 active=true tiles=0" in ctl("ws"), "+ did not add and show a workspace")
    c0, c1 = hot_at("ws 0 "), hot_at("ws 1 ")
    need(c0 is not None and c1 is not None and int(c0[1]) < 75 and int(c1[1]) < 75, f"two chips in the header ({c0}, {c1})")
    need(hot_at("start new") is not None, "the empty workspace shows the start page on the stage")
    click(c0)
    wait_for(lambda: "ws 1 active=true tiles=3" in ctl("ws"), "the chip did not switch back")
    need(True, "a chip click switches the workspace")

    # The context address is read-only; Super+L still edits (its suggestions hang under the header).
    ax = hot_at("chrome address")
    click((str(int(ax[0])), "37"))
    need("editing=false" in ctl("hot"), "a click on the context address does not start editing")
    ctl("address", "progr")
    need("editing=true" in ctl("hot"), "Super+L's editing still works in the Bühne")
    ctl("key", "1")  # evdev KEY_ESC
    time.sleep(0.2)
    need("editing=false" in ctl("hot"), "Escape ends editing")

    # The Absicht headline: typed per workspace (desktop.absicht.<slug>), it takes its room from the tiles.
    b0 = body("one")
    ctl("set", "desktop.absicht.arbeitsraum-1", "Steuer für Oktober fertig machen")
    wait_for(lambda: work()[1] > wy, "the headline did not move the tiles' area")
    wait_for(lambda: body("one")[1] > b0[1], "the tiles did not move under the headline")
    need(True, f"the headline moves the tiles down (work y {wy} -> {work()[1]})")
    ctl("ws", "2")
    time.sleep(0.4)
    need(work()[1] == wy, "the headline belongs to its workspace (workspace 2 has none)")
    ctl("ws", "1")
    time.sleep(0.4)
    still(3.0, "still desktop with a headline")
    ctl("unset", "desktop.absicht.arbeitsraum-1")
    wait_for(lambda: work()[1] == wy and body("one")[1] == b0[1], "the tiles did not come back up")
    need(True, "without a headline the tiles take the stage again")

    # Claude's ring folds the right column: the stage and the tiles grow; again: back.
    click(hot_at("chrome address", "ring"))
    wait_for(lambda: work()[2] > ww + 400, "the ring did not fold the column")
    need("sidebar=false" in ctl("hot"), "the folded column is no sidebar")
    click(hot_at("chrome address", "ring"))
    wait_for(lambda: work()[2] == ww, "the ring did not bring the column back")
    need(True, "Claude's ring folds and unfolds the right column")

    # Startseite (bottom row) shows the start page over the tiles and hides it again.
    click(hot_at("home"))
    wait_for(lambda: hot_at("start new") is not None, "Startseite did not show the start page")
    click(hot_at("home"))
    wait_for(lambda: hot_at("start new") is None, "Startseite did not hide the start page")
    need(True, "Startseite shows and hides the start page")

    # A minimised window gets a chip in the bottom row; its click brings it back.
    need(ctl("minimize").strip() == "ok", "a window is minimised")
    m0 = hot_at("minimized 0")
    need(m0 is not None and int(m0[1]) > 1300, f"its chip is in the bottom row ({m0})")
    click(m0)
    wait_for(lambda: hot_at("minimized 0") is None, "the chip did not bring the window back")
    need(True, "the chip restores the minimised window")

    # The hint line counts the programs that hang on the shown workspace ("· 1 Programm hängt"), not those of a
    # parked one: a fourth window stops answering pings (wyclient --hang), the hint changes; on workspace 2 it
    # looks as before the hang.
    ctl("ws", "2")
    time.sleep(0.4)
    hint2 = hint_box("ws2-before")
    ctl("ws", "1")
    time.sleep(0.4)
    hanger = subprocess.Popen([CLIENT, "--title", "four", "--pattern", "1", "--hang", "1500"], env=env,
                              stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    procs.append(hanger)
    wait_for(lambda: 'title="four"' in ctl("list"), "wyclient four did not map")
    time.sleep(0.4)
    answering = hint_box("four-answers")
    wait_for(lambda: re.search(r'title="four" pid=\d+ hung=true', ctl("list")) is not None, "four did not hang")
    time.sleep(0.4)
    need(hint_box("four-hangs") != answering, "the hint line names the hung program on the shown workspace")
    ctl("ws", "2")
    time.sleep(0.4)
    need(hint_box("ws2-while") == hint2, "a program hung on a parked workspace is not counted")
    ctl("ws", "1")
    time.sleep(0.4)
    hanger.kill()
    hanger.wait()
    procs.remove(hanger)
    wait_for(lambda: 'title="four"' not in ctl("list"), "four did not go")

    # The sun: light and dark (needs CR DS-3-1: spots.rs asks the Bühne's parts).
    if has_cr:
        click(hot_at("buehne", "sun"))
        wait_for(lambda: "mode=light" in ctl("theme"), "the sun did not switch to light")
        click(hot_at("buehne", "sun"))
        wait_for(lambda: "mode=dark" in ctl("theme"), "the sun did not switch back to dark")
        need(True, "the sun switches light and dark")
        for part in ["bell", "feld", "strip"]:
            click(hot_at("buehne", part))
        need("editing=false" in ctl("hot") and ctl("ws").count("\n") >= 2, "bell, Feld bar and strip open nothing yet")
    else:
        need(os.environ.get("DS_REQUIRE_CRS") != "1", "DS_REQUIRE_CRS=1 needs CR DS-3-1 (the sun's click)")
        print("skip: the sun's click (CR DS-3-1 not applied: spots.rs does not ask the Bühne's parts)", flush=True)

    still(3.0, "still Bühne desktop")
    # Classic stays a preset: the list and the favourites come back, the tiles' area is today's.
    ctl("set", "theme.layout.preset", "classic")
    time.sleep(0.5)
    hot = ctl("hot")
    need(work()[:2] == (212, 90) and "\nfav 0 " in hot, f"classic is today's chrome ({work()})")
    ctl("set", "theme.layout.preset", "buehne")
    time.sleep(0.5)
    need(work()[:2] == (51, 85), "and back to the Bühne")
    ctl("quit")
finally:
    for p in reversed(procs):
        p.kill()
        p.wait()
