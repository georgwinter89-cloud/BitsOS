#!/usr/bin/env python3
"""VB-1 scenario driver (76-confirm.sh). The stick holds version 165's files twice: slot 164 (OK) and slot 165
(unmarked), so bitsboot starts 165 as its one try and the kernel asks "Version 165 läuft. Bestätigen?". Georg's
keyboard is QEMU's USB keyboard (QMP send-key: a real device); MCP's keys and pointer are injected input. The
proofs cannot come first: boot_confirm_s=600 and each boot here is shorter.

  ja       MCP's injected keys type `echo confirm 165 > /dev/bitsos/update` into the console shell: the kernel
           refuses it (no real press read by a physical-only reader within 3 s). `bitsupdate frage` (typed on the USB keyboard) shows the
           question with Zurück to 164; MCP's injected J changes nothing; the USB keyboard's J confirms 165
           (boot.confirm_proof by=georg, OK written).
  zurueck  `bitsupdate frage`, the USB keyboard's Z (the German layout's Z is QEMU's qcode y): PIN 164, init
           restarts, bitsboot starts 164 (PINNED).

Usage: confirm_scenario.py <mode> <mcp port> <token> <serial log> <qmp socket>"""
import http.client
import json
import re
import socket
import sys
import time

MODE, PORT, TOKEN, LOG, QMP_PATH = sys.argv[1], int(sys.argv[2]), sys.argv[3], sys.argv[4], sys.argv[5]
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
            rpc(c, "initialize", {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "vb1", "version": "1"}})
            return c
        except (OSError, http.client.HTTPException, ValueError) as e:
            if time.time() > end:
                raise SystemExit(f"RED: no MCP answer on 127.0.0.1:{PORT}: {e}")
            time.sleep(2)


def call(c, name, args):
    r = rpc(c, "tools/call", {"name": name, "arguments": args})
    return "\n".join(i.get("text", "") for i in r["content"] if i["type"] == "text"), r.get("isError", False)


def screen(c):
    return call(c, "screen", {"format": "text"})[0].replace("\n", " ")


def wait_screen(c, pattern, what, timeout):
    end = time.time() + timeout
    t = ""
    while time.time() < end:
        t = screen(c)
        m = re.search(pattern, t)
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


def log_text(since=0):
    with open(LOG, "rb") as f:
        f.seek(since)
        return f.read().decode("utf-8", "replace")


def wait_log(pattern, timeout, what):
    end = time.time() + timeout
    while time.time() < end:
        m = re.search(pattern, log_text())
        if m:
            print(f"ok: {what}", flush=True)
            return m
        time.sleep(1)
    raise SystemExit(f"RED: {what}: no {pattern!r} in the serial log within {timeout} s")


def no_answer(what):
    t = log_text()
    need(re.search(r"boot\.confirm_answer |boot\.confirm_proof|boot\.confirmed", t) is None, what,
         "\n".join(l for l in t.splitlines() if "confirm" in l)[-2000:])


def start(q):
    wait_log(r"generation\.chosen number=165 reason=NEWEST_TRY", 300, "bitsboot starts 165 as its one try")
    wait_log(r"boot\.confirm_pending[^\n]*generation=165", 300, "165 waits for its confirmation")
    c = connect(300)
    wait_screen(c, re.escape(PROMPT), "the console shell is there", 120)
    return c


def frage(c, q):
    q.line("clear")
    time.sleep(0.5)
    q.line("bitsupdate frage")
    wait_screen(c, r"Version 165 l.uft\. Best.tigen\?", "the console asks", 60)
    wait_screen(c, r"Taste Z: Zur.ck zur vorherigen \(Version 164", "with the way back to 164", 10)


def main():
    q = Qmp(QMP_PATH)
    c = start(q)
    if MODE == "ja":
        call(c, "send_keys", {"text": "echo confirm 165 > /dev/bitsos/update"})
        call(c, "send_keys", {"keys": ["enter"]})
        wait_log(r"boot\.confirm_answer_refused[^\n]*generation=165[^\n]*real key", 30, "the kernel refuses an answer without real input (typed by MCP into the shell)")
        no_answer("nothing confirmed by it")
        frage(c, q)
        time.sleep(2.5)
        call(c, "send_keys", {"text": "j"})
        call(c, "send_keys", {"keys": ["enter"]})
        time.sleep(3)
        no_answer("MCP's injected J and Enter answer nothing")
        need("ist bestätigt" not in screen(c), "the console did not take them")
        q.key("j")
        wait_screen(c, r"Version 165 ist best.tigt\. Sie startet ab jetzt immer\.", "the USB keyboard's J confirmed 165", 120)
        wait_log(r"boot\.confirm_proof[^\n]*generation=165[^\n]*by=georg", 30, "by Georg's answer")
        wait_log(r"boot\.confirmed[^\n]*generation=165", 120, "OK written for 165")
        need(re.search(r"boot\.confirm_proof[^\n]*by=(mcp|desktop)", log_text()) is None, "no proof did it")
    elif MODE == "zurueck":
        frage(c, q)
        time.sleep(2.5)
        call(c, "send_keys", {"text": "yz"})
        time.sleep(2)
        need("Zurück zu Version" not in screen(c), "MCP's injected Z does nothing")
        mark = len(log_text())
        q.key("y")
        # The restart follows the line at once: MCP may go away while the screen is read. The console writes the same
        # line to the serial log, which outlives the restart.
        try:
            wait_screen(c, r"Zur.ck zu Version 164: BitsOS startet jetzt neu\.", "the USB keyboard's Z goes back to 164", 60)
        except (OSError, http.client.HTTPException):
            need(re.search(r"Zur.ck zu Version 164: BitsOS startet jetzt neu\.", log_text(mark)) is not None,
                 "the USB keyboard's Z goes back to 164 (the serial log; MCP went away with the restart)")
        c.close()
        end = time.time() + 300
        while time.time() < end and not re.search(r"generation\.chosen number=164 reason=PINNED", log_text(mark)):
            time.sleep(1)
        need(re.search(r"generation\.chosen number=164 reason=PINNED", log_text(mark)) is not None, "init restarted, bitsboot starts 164 (pinned)")
        need(re.search(r"boot\.confirm_proof[^\n]*generation=165", log_text()) is None, "165 was never confirmed")
    else:
        raise SystemExit(f"RED: unknown mode {MODE}")
    print(f"confirm scenario {MODE}: green", flush=True)


main()
