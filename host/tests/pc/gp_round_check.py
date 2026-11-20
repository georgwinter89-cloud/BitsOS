#!/usr/bin/env python3
"""GP-ROUND rehearsal client (host/tests/pc/80-gp-round.sh): the first stick round on the Gaming-PC, in QEMU. MCP over
QEMU's hostfwd with the stick's token (as the remote desk will reach the real machine), QEMU's QMP for the keyboard
behind the second PCI root (as Georg's hands).

Usage: round_check.py <mcp port> <token> <qmp socket> <out dir>
  1. MCP answers; hw_info names the profile with both tiers.
  2. The report page is up by itself (no key was sent): MCP's screen text shows "Seite 1 von N", and hw_info's
     report section holds every group's title; the text is saved to <out dir>/mcp-report.txt.
  3. With more than one page: after 20 s the page turned by itself.
  4. Keys on the keyboard behind the second root turn the pages; after the last one the shell's prompt is there.
  5. `ls /boot/BITSOS2` typed on that keyboard lists the stick, REPORT among it.
  6. reboot (QEMU runs with -no-reboot, so the reset ends it).
Exit status 0 = green."""
import http.client
import json
import socket
import sys
import time

PORT, TOKEN, QMP, OUT = int(sys.argv[1]), sys.argv[2], sys.argv[3], sys.argv[4]
GROUPS = ["Startprogramm, Profil und Freigaben", "Prozessor, Uhr und Tabellen", "Geräteschutz", "Geräteliste (beide Anschluss-Stränge)",
          "Platten", "Bild", "USB", "Netzwerk", "Grafikkarte", "Wächter", "Ton"]
# The German layout's physical keys (as host/tests/in/tty_check.py).
KEYS = {c: [c] for c in "abcdefghijklmnopqrstuvwx"}
KEYS.update({"y": ["z"], "z": ["y"], " ": ["spc"], "/": ["shift", "7"], "\n": ["ret"], "2": ["2"], "0": ["0"]})
next_id = 0


def need(cond, what, detail=""):
    if not cond:
        raise SystemExit(f"RED: {what}\n{detail}")
    print(f"  ok: {what}")


def rpc(c, method, params=None):
    global next_id
    next_id += 1
    body = json.dumps({"jsonrpc": "2.0", "id": next_id, "method": method, "params": params or {}})
    c.request("POST", "/mcp", body, {"Authorization": f"Bearer {TOKEN}", "Content-Type": "application/json"})
    r = c.getresponse()
    data = r.read()
    j = json.loads(data) if data else None
    if r.status != 200 or not j or "result" not in j:
        raise ValueError(f"{method}: HTTP {r.status} {j}")
    return j["result"]


def connect(wait_s):
    end = time.time() + wait_s
    while True:
        try:
            c = http.client.HTTPConnection("127.0.0.1", PORT, timeout=30)
            rpc(c, "initialize", {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "gp-round", "version": "1"}})
            return c
        except (OSError, http.client.HTTPException, ValueError) as e:
            if time.time() > end:
                raise SystemExit(f"RED: no MCP answer on 127.0.0.1:{PORT}: {e}")
            time.sleep(1)


def call(c, name, args=None):
    r = rpc(c, "tools/call", {"name": name, "arguments": args or {}})
    return r["isError"], "\n".join(i.get("text", "") for i in r["content"] if i["type"] == "text")


class Qmp:
    def __init__(self, path):
        end = time.time() + 30
        while True:
            try:
                self.s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
                self.s.connect(path)
                break
            except OSError:
                if time.time() > end:
                    raise SystemExit(f"RED: no QMP socket at {path}")
                time.sleep(0.5)
        self.f = self.s.makefile("rw", encoding="utf-8", newline="\n")
        json.loads(self.f.readline())
        self.cmd("qmp_capabilities")

    def cmd(self, name, args=None):
        self.f.write(json.dumps({"execute": name, "arguments": args or {}}) + "\n")
        self.f.flush()
        while True:
            m = json.loads(self.f.readline())
            if "return" in m or "error" in m:
                return m

    def key(self, names):
        """Presses `names` together and lets go. The machine has no PS/2 (i8042=off) and one USB keyboard, the one
        behind the second root, so QEMU's keyboard input reaches only it (QMP's "device" names a display, not an
        input device)."""
        r = self.cmd("send-key", {"keys": [{"type": "qcode", "data": n} for n in names], "hold-time": 40})
        if "error" in r:
            raise SystemExit(f"RED: QMP send-key: {r['error']}")
        time.sleep(0.12)

    def type(self, text):
        for ch in text:
            self.key(KEYS[ch])


def screen(c):
    err, t = call(c, "screen", {"format": "text"})
    need(not err, "screen answers in text mode", t[:300])
    return t


def page_of(t):
    for line in t.splitlines()[:4]:
        if "Seite " in line and " von " in line:
            tail = line[line.rindex("Seite "):].split()
            return int(tail[1]), int(tail[3])
    return None


c = connect(300)
need(True, "MCP answers over the network with the stick's token")
err, t = call(c, "hw_info", {"section": "kernel"})
need(not err and "profile=gaming-pc reason=dmi tiers=net,stick" in t, "hw_info names the profile and both tiers", t[:1500])

end = time.time() + 150
report = ""
while time.time() < end:
    err, report = call(c, "hw_info", {"section": "report"})
    # The page comes first; the stick's file follows from its own thread and then renews the file's row.
    if not err and "not ready yet" not in report and "wird geschrieben, sobald" not in report:
        break
    time.sleep(2)
need("not ready yet" not in report and "BitsOS-Bericht für den Gaming-PC" in report, "the report is ready by itself, with no key sent", report[:800])
for g in GROUPS:
    need(f"\n{g}\n" in report, f"the report holds the group '{g}'", report[:3000])
for want in ["Sicherer Modus für den Gaming-PC (am Mainboard erkannt).", "Netzwerk über Kabel: an. WLAN bleibt aus.",
             "Start-Stick: darf beschrieben werden", "Eingebaute Platten: kein Treiber, nicht gelesen.",
             "Anschluss-Stränge: 2", "Speicherschutz für Geräte:", "USB-Baustein", "MCP erreichbar unter 10.0.2.15:8765",
             "Bericht auf dem Stick: \\BITSOS2\\REPORT\\GP000001.TXT", "Wächter im Chipsatz:", "Ton:"]:
    need(want in report, f"the report says: {want}", report[:4000])
open(f"{OUT}/mcp-report.txt", "w", encoding="utf-8").write(report + "\n")

t = screen(c)
p = page_of(t)
need(p is not None and p[0] == 1, "the page is on the screen by itself: Seite 1", t[:1500])
pages = p[1]
print(f"  the report has {pages} page(s) at this screen size")
# Line 0 is the tool's own line ("console 160x45 cells ..."); the page starts below it.
need(len(t.splitlines()) > 1 and "BitsOS-Bericht" in t.splitlines()[1], "the page's head names the report", t[:300])
open(f"{OUT}/screen-page1.txt", "w", encoding="utf-8").write(t + "\n")
if pages > 1:
    time.sleep(23)
    t2 = screen(c)
    p2 = page_of(t2)
    need(p2 is not None and p2[0] == 2, "after 20 s the page turned by itself to Seite 2", str(p2))
    open(f"{OUT}/screen-page2.txt", "w", encoding="utf-8").write(t2 + "\n")

q = Qmp(QMP)
shown = page_of(screen(c))
for _ in range(pages + 2):
    if shown is None:
        break
    q.key(["spc"])
    time.sleep(1.0)
    shown = page_of(screen(c))
need(shown is None, "keys on the keyboard behind the second root turned the pages, the last one ended the page")
time.sleep(1.0)
q.type("\n")
time.sleep(1.0)
q.type("ls /boot/bitsos2\n")
seen = ""
for _ in range(20):
    time.sleep(0.5)
    seen = screen(c)
    if "REPORT" in seen.upper() and "SYS" in seen:
        break
need("ls /boot/bitsos2" in seen.lower() and "REPORT" in seen.upper(), "the shell took 'ls /boot/bitsos2' from that keyboard and listed the stick", seen[-1500:])
try:
    call(c, "reboot")
except (OSError, http.client.HTTPException, ValueError):
    pass
print("  ok: reboot sent")
