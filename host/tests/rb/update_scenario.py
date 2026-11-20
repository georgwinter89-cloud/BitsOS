#!/usr/bin/env python3
"""UPDATE scenario driver (56-update.sh). The stick holds version 164 (OK) with the old counting's 103 beside it.
Georg's keyboard is QEMU's USB keyboard (QMP send-key: physical keys); the console shell (BusyBox ash, root) runs
`bitsupdate`; what it prints is read with MCP's `screen` (text). MCP's own keyboard is injected input.
 1. `bitsupdate status`: version 164 runs.
 2. `bitsupdate install`: check (it says what comes: Kern, Schreibtisch, the fake tree), download from the fake
    GitHub, signature and SHA-256; the code it shows is typed first through MCP's injected keyboard (must not
    count), then on the USB keyboard; version 165's parts (bin, fake) are written and sealed, then it is installed
    as slot 165 (NEW) naming them.
 3. Reboot: 165 starts as its one try with its parts bound (`fat stick.bind`). No MCP call in this boot until it
    is confirmed: `bitsupdate frage` typed on the USB keyboard asks, the USB keyboard's J answers, the kernel confirms
    165 by Georg's yes (`boot.confirm_proof ... by=georg`), never by MCP (QEMU has no desktop, Georg 18.11.: the
    desktop's own proof is the Mini-PC's). Then MCP reads 165's bitscomp stand-in and fake tree at the fixed paths.
 4. Reboot (165 NEWEST_OK, its parts again). Version 166 appears on the fake GitHub (only its stand-in changed):
    `bitsupdate install` says the fake tree is unchanged, writes only the new bin part (no `parts.begin` of the
    fake tree), installs 166; it starts as its try with the new bin and 165's fake tree bound and is confirmed by
    Georg's yes as 165 was; MCP reads 166's stand-in and 165's fake tree.
 5. Reboot (166 NEWEST_OK), `bitsupdate back`: PIN 165; reboot: 165 (PINNED) binds its own bin part again: 165's
    stand-in. `bitsupdate back`: PIN 164; reboot: bitsboot starts 164 (PINNED) with nothing bound: the stick's own
    (164's) stand-in and fake tree; `bitsupdate status` says version 164, `bitsupdate check` says 166 is installed
    but Zurück was chosen (not "neueste Version").
 6. `bitsupdate back` again: PIN 103, the kernel from before UPDATE; reboot: it starts (PINNED), MCP answers,
    system_list reads 103 164 165 and the pin, it reads 164's fake tree (it knows nothing of parts), and the token in
    settings/update.tok is out of reach: read_file is refused, a program (MCP's run, uid 1000) cannot read it.

Usage: update_scenario.py <mcp port> <token> <serial log> <qmp socket> <github token> <release 166> <its place>"""
import base64
import http.client
import json
import os
import re
import socket
import sys
import time

PORT, TOKEN, LOG, QMP_PATH, GHTOKEN = int(sys.argv[1]), sys.argv[2], sys.argv[3], sys.argv[4], sys.argv[5]
LATER, LATER_AT = sys.argv[6], sys.argv[7]
PROMPT = "/ #"
nid = 0


def need(cond, what, detail=""):
    if not cond:
        raise SystemExit(f"RED: {what}\n{detail}")
    print(f"ok: {what}", flush=True)


def rpc(c, method, params=None):
    global nid
    nid += 1
    c.request("POST", "/mcp", json.dumps({"jsonrpc": "2.0", "id": nid, "method": method, "params": params or {}}),
              {"Authorization": f"Bearer {TOKEN}", "Content-Type": "application/json"})
    r = c.getresponse()
    data = r.read()
    j = json.loads(data) if data else None
    if r.status != 200 or j is None or "result" not in j:
        raise ValueError(f"{method}: HTTP {r.status} {j}")
    return j["result"]


def connect(wait_s):
    end = time.time() + wait_s
    while True:
        try:
            c = http.client.HTTPConnection("127.0.0.1", PORT, timeout=120)
            rpc(c, "initialize", {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "upd", "version": "1"}})
            return c
        except (OSError, http.client.HTTPException, ValueError) as e:
            if time.time() > end:
                raise SystemExit(f"RED: no MCP answer on 127.0.0.1:{PORT}: {e}")
            time.sleep(2)


def call(c, name, args):
    r = rpc(c, "tools/call", {"name": name, "arguments": args})
    return "\n".join(i.get("text", "") for i in r["content"] if i["type"] == "text"), r.get("isError", False)


def screen(c):
    return call(c, "screen", {"format": "text"})[0]


def wait_screen(c, pattern, what, timeout):
    end = time.time() + timeout
    t = ""
    while time.time() < end:
        t = screen(c)
        m = re.search(pattern, t.replace("\n", " "))
        if m:
            print(f"ok: {what}", flush=True)
            return m
        time.sleep(1)
    raise SystemExit(f"RED: {what}: {pattern!r} not on the screen\n{t}")


class Qmp:
    def __init__(self, path):
        end = time.time() + 60
        while True:
            try:
                self.s = socket.socket(socket.AF_UNIX)
                self.s.settimeout(20)
                self.s.connect(path)
                break
            except OSError:
                if time.time() > end:
                    raise SystemExit("RED: no QMP socket")
                time.sleep(0.5)
        self.f = self.s.makefile("rw")
        self.f.readline()
        self.cmd("qmp_capabilities")

    def cmd(self, name, args=None):
        self.f.write(json.dumps({"execute": name, "arguments": args or {}}) + "\n")
        self.f.flush()
        while True:
            r = json.loads(self.f.readline())
            if "return" in r:
                return r["return"]
            if "error" in r:
                raise SystemExit(f"RED: qmp {name}: {r['error']}")

    def key(self, qcode):
        self.cmd("send-key", {"keys": [{"type": "qcode", "data": qcode}], "hold-time": 60})
        time.sleep(0.08)

    def line(self, text):
        # Letters and spaces only: the same keys on the German and the US layout (no y, z).
        for ch in text:
            self.key("spc" if ch == " " else ch)
        self.key("ret")

    def move(self, dx, dy):
        self.cmd("input-send-event", {"events": [{"type": "rel", "data": {"axis": "x", "value": dx}}, {"type": "rel", "data": {"axis": "y", "value": dy}}]})


def log_text(since=0):
    with open(LOG, "rb") as f:
        f.seek(since)
        return f.read().decode("utf-8", "replace")


def log_size():
    with open(LOG, "rb") as f:
        return len(f.read())


def wait_log(pattern, since, timeout, what):
    end = time.time() + timeout
    while time.time() < end:
        m = re.search(pattern, log_text(since))
        if m:
            print(f"ok: {what}", flush=True)
            return m
        time.sleep(1)
    raise SystemExit(f"RED: {what}: no {pattern!r} in the serial log within {timeout} s")


def console(q, text):
    q.line("clear")
    time.sleep(0.5)
    q.line(text)


def read(c, path):
    """A small file's text through MCP read_file (its answer is JSON with the bytes in base64), or None."""
    t, err = call(c, "read_file", {"path": path})
    if err:
        return None, t
    try:
        return base64.b64decode(json.loads(t)["data"]).decode("utf-8", "replace"), t
    except (ValueError, KeyError):
        return None, t


def files_are(c, v, what, tree=None):
    """The fixed paths show version v's bitscomp stand-in and the fake tree of version `tree` (default v)."""
    t = tree or v
    for path, want, of in (("/boot/BITSOS2/bin/bitscomp", f"stand-in of version {v}", v), ("/boot/BITSOS2/fake/hello.txt", f"fake tree of version {t}", t),
                           ("/boot/BITSOS2/fake/sub/dir/deep file.txt", f"deep {t}", t)):
        got, raw = read(c, path)
        need(got is not None and want in got, f"{what}: {path} is {of}'s", raw)


def reboot(c):
    mark = log_size()
    call(c, "reboot", {})
    c.close()
    return mark


def install(c, q, v, comes):
    """`bitsupdate install` of version v at the console: what comes, the code on the real keyboard (MCP's injected
    keys must not count), installed."""
    console(q, "bitsupdate install")
    m = wait_screen(c, r"echten Tastatur: ([A-Z](?: [A-Z]){3})", f"{v}: the download was checked and the code is shown", 900)
    code = m.group(1).split()
    t = screen(c)
    need(re.search(rf"Version {v} ist gepr.ft und von deinem Desk unterschrieben", t.replace("\n", " ")) is not None, f"{v}: signature and hashes checked in the guest", t)
    need(re.search(comes, t.replace("\n", " ")) is not None, f"{v}: the console says what comes", t)
    time.sleep(2.5)  # past the arming
    for letter in code:
        call(c, "send_keys", {"text": letter.lower()})
    time.sleep(3)
    need("ist installiert" not in screen(c).replace("\n", " "), f"{v}: keys injected by MCP do not install")
    for letter in code:
        q.key(letter.lower())
    wait_screen(c, rf"Version {v} ist installiert", f"the real keyboard's code installed version {v}", 300)


def confirm_by_georg(q, mark, v):
    """No MCP call in this boot until it is confirmed: `bitsupdate frage` and J on the USB keyboard (without a look at
    the screen, which only MCP gives: a J that came too early is typed again with the question)."""
    wait_log(rf"boot\.confirm_pending[^\n]*generation={v}", mark, 300, f"{v} waits for its confirmation")
    time.sleep(15)  # the console shell comes up after the kernel's early services
    proof = rf"boot\.confirm_proof[^\n]*generation={v}[^\n]*by=georg"
    for _ in range(4):
        console(q, "bitsupdate frage")
        time.sleep(6)  # the question is up and past its arming
        q.key("j")
        end = time.time() + 20
        while time.time() < end and not re.search(proof, log_text(mark)):
            time.sleep(1)
        if re.search(proof, log_text(mark)):
            break
    need(re.search(proof, log_text(mark)) is not None, f"Georg's own J confirmed version {v}")
    wait_log(rf"boot\.confirmed[^\n]*generation={v}", mark, 120, f"OK written for {v}")
    need(re.search(r"boot\.confirm_proof[^\n]*by=(mcp|desktop)", log_text(mark)) is None, "neither the MCP nor the desktop proof did it")


def main():
    q = Qmp(QMP_PATH)
    c = connect(300)
    wait_screen(c, re.escape(PROMPT), "the console shell is there", 120)
    console(q, "bitsupdate status")
    wait_screen(c, r"Installiert: Version 164", "version 164 runs", 60)
    files_are(c, 164, "164 from the stick's own folders")

    # 2. The install.
    install(c, q, 165, r"Es kommt: Kern \d+ MB, Schreibtisch \d+ MB, Programme \d+ MB \(fake\)")
    log = log_text(0)
    need(re.search(r"kexec parts\.sealed[^\n]*part=bin", log) is not None and re.search(r"kexec parts\.sealed[^\n]*part=fake", log) is not None, "the kernel sealed both parts")

    # 3. Reboot into 165; no MCP call in this boot until it is confirmed.
    mark = reboot(c)
    wait_log(r"generation\.chosen number=165 reason=NEWEST_TRY", mark, 300, "bitsboot starts 165 as its one try")
    wait_log(r"fat stick\.bind[^\n]*part=\"?bin ", mark, 300, "165 starts with its bin part bound")
    wait_log(r"fat stick\.bind[^\n]*part=\"?fake ", mark, 300, "and its fake tree bound")
    confirm_by_georg(q, mark, 165)
    c = connect(120)
    files_are(c, 165, "165 with its parts")

    # 4. Version 166: only the stand-in changed; the fake tree on the medium is used again.
    mark = reboot(c)
    wait_log(r"generation\.chosen number=165 reason=NEWEST_OK", mark, 300, "165 starts as the confirmed newest")
    c = connect(300)
    wait_screen(c, re.escape(PROMPT), "the console shell is there again", 120)
    console(q, "bitsupdate status")
    wait_screen(c, r"Installiert: Version 165", "version 165 runs", 60)
    os.rename(LATER, LATER_AT)
    since = log_size()
    install(c, q, 166, r"Es kommt: Kern \d+ MB, Schreibtisch \d+ MB, 1 Programm unver.ndert\.")
    log = log_text(since)
    need(re.search(r"kexec parts\.sealed[^\n]*part=bin", log) is not None, "166's new bin part was sealed")
    need(re.search(r"kexec parts\.begin[^\n]*part=fake", log) is None, "the unchanged fake tree was not written again", log[-3000:])
    mark = reboot(c)
    wait_log(r"generation\.chosen number=166 reason=NEWEST_TRY", mark, 300, "bitsboot starts 166 as its one try")
    wait_log(r"fat stick\.bind[^\n]*part=\"?bin ", mark, 300, "166 starts with its bin part bound")
    wait_log(r"fat stick\.bind[^\n]*part=\"?fake ", mark, 300, "and the shared fake tree bound")
    confirm_by_georg(q, mark, 166)
    c = connect(120)
    files_are(c, 166, "166 with its new bin part and 165's fake tree", tree=165)

    # 5. Back to 165 (its own parts), then to 164 (the stick's own folders).
    mark = reboot(c)
    wait_log(r"generation\.chosen number=166 reason=NEWEST_OK", mark, 300, "166 starts as the confirmed newest")
    c = connect(300)
    wait_screen(c, re.escape(PROMPT), "the console shell is there again", 120)
    console(q, "bitsupdate status")
    wait_screen(c, r"Installiert: Version 166", "version 166 runs", 60)
    console(q, "bitsupdate back")
    wait_screen(c, r"kommt Generation 165", "Zurück: the next start takes 165", 60)
    mark = reboot(c)
    wait_log(r"generation\.chosen number=165 reason=PINNED", mark, 300, "bitsboot starts 165 (pinned)")
    wait_log(r"fat stick\.bind[^\n]*part=\"?bin ", mark, 300, "165 binds its own bin part")
    c = connect(300)
    files_are(c, 165, "the fall back to 165 uses 165's own parts")
    wait_screen(c, re.escape(PROMPT), "the console shell is there", 120)
    console(q, "bitsupdate back")
    wait_screen(c, r"kommt Generation 164", "Zurück from 165: the next start takes 164", 60)
    mark = reboot(c)
    wait_log(r"generation\.chosen number=164 reason=PINNED", mark, 300, "bitsboot starts 164 (pinned)")
    c = connect(300)
    need(re.search(r"fat stick\.bind", log_text(mark)) is None, "164 binds nothing (its slot names no parts)")
    files_are(c, 164, "the fall back to 164 uses 164's own files")
    wait_screen(c, re.escape(PROMPT), "the console shell is there", 120)
    console(q, "bitsupdate status")
    wait_screen(c, r"Installiert: Version 164", "version 164 runs again", 60)
    console(q, "bitsupdate check")
    wait_screen(c, r"Version 166 ist installiert, es l.uft aber Version 164", "check names the installed 166, not the newest version", 120)
    need("neueste Version." not in screen(c).replace("\n", " "), "no 'neueste Version' while 164 runs")

    # 6. Back once more: the kernel from before UPDATE (slot 103) with the numbered slots and the token beside it.
    console(q, "bitsupdate back")
    wait_screen(c, r"kommt Generation 103", "Zurück from 164: the next start takes 103", 60)
    mark = reboot(c)
    wait_log(r"generation\.chosen number=103 reason=PINNED", mark, 300, "bitsboot starts 103 (pinned)")
    m = wait_log(r"kernel start [^\n]*", mark, 300, "the old kernel starts")
    need("bitsos_version" not in m.group(0), "it is a kernel from before UPDATE", m.group(0))
    c = connect(300)
    t, err = call(c, "system_list", {})
    need(not err and all(re.search(rf"\b{g}\b", t) for g in ("103", "164", "165", "166")), "the old kernel's system_list reads 103, 164, 165 and 166", t)
    need(re.search(r"(?i)pin[^\n]*103", t) is not None, "and the pin on 103", t)
    files_are(c, 164, "the kernel from before UPDATE sees the stick's own folders")
    t, err = call(c, "read_file", {"path": "/boot/BITSOS2/settings/update.tok"})
    need(err and GHTOKEN not in t, "the old kernel's read_file refuses the token", t)
    t, err = call(c, "run", {"path": "/bin/sh", "args": ["-c", "id -u; cat /boot/BITSOS2/settings/update.tok; echo rc=$?"]})
    need(GHTOKEN not in t and re.search(r"rc=[1-9]", t) is not None and re.search(r"\b1000\b", t) is not None, "a program (uid 1000) cannot read the token", t)
    print("update scenario: green", flush=True)


main()
