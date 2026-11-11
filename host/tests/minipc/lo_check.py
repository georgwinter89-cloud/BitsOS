#!/usr/bin/env python3
"""LO-1 Mini-PC run (Georg 30.10.: programs are tested on the Mini-PC only): LibreOffice from the stick, over MCP.

Usage: lo_check.py <host:port> <out dir> [--tls DIR|auto] [--token-file F] [--desktop]   (token: BITSOS_TOKEN or the file)
The desk runs it against the Mini-PC (`bash host/tests/minipc/lo.sh`), with the stick carrying \\BITSOS2\\libreoffice\\
and \\BITSOS2\\bin\\libreoffice, the data volume mounted. It replaces the QEMU scenario host/tests/lo/10-office.sh:
  1. MCP ktest `ul.pkg.libreoffice` (class Program: tree, links, licences, libraries, --check, --env, a headless
     conversion in a scratch home): passed=3 failed=0, nothing skipped or refused.
  2. `libreoffice --check` as MCP's user: the tree, libraries, home on /data/home/1000 (data volume), the language.
  3. A headless conversion into the user's real home (/data/home/1000/lo1-check/, removed again at the end): a whole
     PDF and an ODF text.
  4. With --desktop (the desktop runs, with nothing the desk needs focused): the desktop's address `programm/writer`
     opens Writer (bitscomp ctl list: a libreoffice-writer window), MCP `screen` saves a picture
     (writer.png), the window's close ends LibreOffice; the same for `programm/calc` (calc.png). GTK found its icons
     (the launcher's log has no "Could not find the icon").
What a person has to see (typing, saving through the dialog, the state after a reboot) stays in the check list of
plan/notes/lo-LO-1.md. Exit 0 = green."""
import base64
import json
import os
import re
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import mp  # noqa: E402

OPTS = mp.options(sys.argv[1:], positional=2, flags=("--desktop",))
OUT = OPTS.rest[1]
DESKTOP = OPTS.flags["--desktop"]
LAUNCHER = "/boot/BITSOS2/bin/libreoffice"
DIR = "/data/home/1000/lo1-check"
ENV = {"PATH": "/bin:/usr/bin:/boot/BITSOS2/bin", "XDG_RUNTIME_DIR": "/run/user/1000"}


def need(cond, what, detail=""):
    if not cond:
        raise SystemExit(f"RED: {what}\n{detail}")
    print(f"ok: {what}", flush=True)


def call(name, args=None):
    return mp.call(OPTS, name, args)


def sh(cmd, ms=300000):
    err, text, _ = call("run", {"path": "/bin/sh", "args": ["-c", cmd], "env": ENV, "timeout_ms": ms})
    need(not err, f"run {cmd[:80]}", text)
    return text


def read_file(path):
    data, off = b"", 0
    while True:
        err, text, _ = call("read_file", {"path": path, "offset": off})
        need(not err, f"read_file {path}", text)
        j = json.loads(text)
        data += base64.b64decode(j["data"])
        off += j["length"]
        if j["eof"] or not j["length"]:
            return data


def ctl(*words):
    return sh("exec bitscomp ctl " + " ".join(words), 20000)


def window(app, secs):
    t0 = time.time()
    while time.time() - t0 < secs:
        lines = [l for l in ctl("list").splitlines() if f"app_id={app} " in l]
        if lines:
            return lines[0]
        time.sleep(2)
    raise SystemExit(f"RED: no {app} window within {secs} s\n{ctl('list')}")


def office(addr, app, what):
    text = ctl("address", "open", addr)
    need("notice" not in text, f"the desktop opens {addr}", text)
    t0 = time.time()
    line = window(app, 120)
    print(f"{what}: window after {time.time() - t0:.1f} s: {line}", flush=True)
    time.sleep(3)
    err, text, imgs = call("screen", {})
    need(not err and imgs, f"MCP screen with {what}", text)
    open(os.path.join(OUT, f"{app.split('-')[1]}.png"), "wb").write(imgs[0])
    ctl("close")  # the focused window: the one just opened
    t0 = time.time()
    while f"app_id={app} " in ctl("list"):
        need(time.time() - t0 < 60, f"{what}: its window closes", ctl("list"))
        time.sleep(1)
    print(f"ok: {what} closed ({time.time() - t0:.1f} s)", flush=True)
    return line


def main():
    os.makedirs(OUT, exist_ok=True)
    err, text, _ = call("ktest", {"filter": "ul.pkg.libreoffice", "timeout_ms": 900000})
    print(text, flush=True)
    need(not err and re.search(r"passed=3 failed=0 skipped=0 refused=0", text), "ktest ul.pkg.libreoffice: 3 passed, none failed, skipped or refused", text)

    text = sh(f"{LAUNCHER} --check; echo rc=$?", 120000)
    print(text, flush=True)
    need("rc=0" in text and re.search(r"libraries: \d+ found, none missing", text), "libreoffice --check: tree and libraries", text)
    need("libreoffice: home: /data/home/1000 (data volume" in text, "the home is /data/home/1000 on the data volume", text)

    sh(f"mkdir -p {DIR} && printf 'LO-1 Mini-PC\\n\\nZweite Zeile.\\n' > {DIR}/brief.txt")
    for fmt in ("pdf", "odt"):
        t0 = time.time()
        text = sh(f"{LAUNCHER} --convert-to {fmt} --outdir {DIR} {DIR}/brief.txt; echo rc=$?")
        need("rc=0" in text and "LibreOffice ended (code 0)" in text, f"--convert-to {fmt} ends with code 0 ({time.time() - t0:.1f} s)", text)
    pdf = read_file(f"{DIR}/brief.pdf")
    need(pdf.startswith(b"%PDF-") and b"%%EOF" in pdf[-1024:], f"brief.pdf is a whole PDF ({len(pdf)} bytes)")
    odt = read_file(f"{DIR}/brief.odt")
    need(odt.startswith(b"PK") and b"opendocument.text" in odt[:200], f"brief.odt is an ODF text ({len(odt)} bytes)")

    if DESKTOP:
        office("programm/writer", "libreoffice-writer", "Writer")
        office("programm/calc", "libreoffice-calc", "Calc")
        log = sh("cat /run/user/1000/libreoffice/libreoffice.log")
        bad = [l for l in log.splitlines() if re.search(r"Could not find the icon|theme was not found|Unrecognized image", l)]
        need(not bad, "GTK finds its icons and image loaders in the tree", "\n".join(bad[:5]))
    else:
        print("note: without --desktop: Writer and Calc windows are not checked (the check list's points 2-6 are)", flush=True)
    sh(f"rm -rf {DIR}")
    print("GREEN: LibreOffice on the Mini-PC", flush=True)


if __name__ == "__main__":
    main()
