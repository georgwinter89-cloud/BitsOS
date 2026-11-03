#!/usr/bin/env python3
"""MC-9a scenario client: install requests through bitsmcp to the installer bitsinstall, decided by the test
confirmer (CONFIG.TXT mcp_confirmer=test), under mcp=both.

Usage: installer_check.py <kernel port> <service port> <token> <qmp socket> <serial log>

Steps: the services up (init's glue, or typed into the root console shell through QEMU's USB keyboard); the
kernel refuses the installer's role to a non-root program, a copy of /bin/bitsinstall and a second claim; the hub
refuses a confirmer that is not the expected program; without a confirmer every request is refused; the test
confirmer then answers no, yes, silence (timeout), an injected yes, and sets a grant for `pin` that lets a pin
request through without a dialog until it expires by itself; the journal holds every decision and the grant's
end; last, a confirmed real reboot answers first and then restarts the machine."""

import http.client
import json
import re
import socket
import sys
import time

next_id = 0
KPORT, SPORT, TOKEN, QMP_PATH, LOG = int(sys.argv[1]), int(sys.argv[2]), sys.argv[3], sys.argv[4], sys.argv[5]
GRANT_S = 40


def need(cond, what, detail=""):
    if not cond:
        raise SystemExit(f"RED: {what}\n{str(detail)[:3000]}")
    print(f"ok: {what}", flush=True)


def serial():
    with open(LOG, "rb") as f:
        return f.read().decode("utf-8", "replace")


def wait_serial(pattern, what, timeout=60, after=0):
    end = time.time() + timeout
    rx = re.compile(pattern, re.M)
    while time.time() < end:
        m = rx.search(serial(), after)
        if m:
            return m
        time.sleep(0.5)
    raise SystemExit(f"RED: {what}: no /{pattern}/ in the serial log\n" + "\n".join(serial().splitlines()[-30:]))


class Client:
    def __init__(self, port, wait_s=60):
        self.port = port
        end = time.time() + wait_s
        while True:
            try:
                self.c = http.client.HTTPConnection("127.0.0.1", port, timeout=420)
                self.rpc("initialize", {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "mc9a", "version": "1"}})
                return
            except (OSError, http.client.HTTPException, ValueError) as e:
                if time.time() > end:
                    raise SystemExit(f"RED: no MCP answer on 127.0.0.1:{port}: {e}")
                time.sleep(1)

    def post(self, body):
        self.c.request("POST", "/mcp", json.dumps(body), {"Authorization": f"Bearer {TOKEN}", "Content-Type": "application/json"})
        r = self.c.getresponse()
        data = r.read()
        return r.status, (json.loads(data) if data else None)

    def rpc(self, method, params=None):
        global next_id
        next_id += 1
        st, j = self.post({"jsonrpc": "2.0", "id": next_id, "method": method, "params": params or {}})
        if st != 200 or "result" not in j:
            raise ValueError(f"{method}: HTTP {st} {j}")
        return j["result"]

    def call(self, name, args=None):
        global next_id
        next_id += 1
        st, j = self.post({"jsonrpc": "2.0", "id": next_id, "method": "tools/call", "params": {"name": name, "arguments": args or {}}})
        if st != 200 or "result" not in j:
            raise ValueError(f"tools/call {name}: HTTP {st} {j}")
        r = j["result"]
        return r["isError"], "\n".join(i.get("text", "") for i in r["content"] if i["type"] == "text")


# QEMU qcodes on V2's German console layout ('-' where the US slash is, '/' = shift+7, '&' = shift+6).
KEYS = {c: [c] for c in "abcdefghijklmnopqrstuvwx0123456789"}
KEYS.update({" ": ["spc"], "-": ["slash"], "\n": ["ret"], "/": ["shift", "7"], "&": ["shift", "6"]})


class Qmp:
    def __init__(self, path):
        end = time.time() + 60
        while True:
            try:
                self.s = socket.socket(socket.AF_UNIX)
                self.s.settimeout(30)
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

    def type(self, text):
        for ch in text:
            self.cmd("send-key", {"keys": [{"type": "qcode", "data": n} for n in KEYS[ch]], "hold-time": 40})
            time.sleep(0.08)


def refused(t, tool):
    return f"{tool} was not done:" in t


def decided(by, decision, tool, after=0):
    return wait_serial(rf"mcp svc\.install_decided id=\d+ tool={tool} decision={decision} by={by}", f"kernel event: {tool} {decision} by {by}", 30, after)


def main():
    k = Client(KPORT)
    print("kernel server up", flush=True)
    wait_serial(r"init: started the console shell", "the console shell", 120)
    q = Qmp(QMP_PATH)
    q.type("\n")
    # init's glue starts both before the shell; without it they are typed in (as root).
    if not re.search(r"init: started the MCP service", serial()):
        q.type("bitsmcp serve &\n")
        print("note: init has no bitsmcp glue; started from the console shell", flush=True)
    wait_serial(r"bitsmcp: serving MCP on port 8766", "bitsmcp serves", 60)
    if not re.search(r"init: started the installer", serial()):
        q.type("bitsinstall serve &\n")
        print("note: init has no bitsinstall glue; started from the console shell", flush=True)
    m = wait_serial(r"bitsinstall: serving installer requests; confirmer (\S+)", "bitsinstall serves", 60)
    need(m.group(1) == "/bin/bitsinstall", "under mcp_confirmer=test the confirmer is the test program", m.group(0))
    need(re.search(r"mcp installer\.claimed pid=\d+", serial()) is not None, "the kernel gave the boot's /bin/bitsinstall the installer's role")
    e, t = k.call("read_file", {"path": "/proc/bitsos/mcp", "encoding": "text"})
    need(not e and "mode=both port=8766 confirmer=test" in t, "/proc/bitsos/mcp names the test confirmer", t)
    s = Client(SPORT)
    names = {t["name"]: t for t in s.rpc("tools/list")["tools"]}
    need(all(n in names for n in ["install_kernel", "install_system", "reload", "reboot", "system_pin"]), "the service lists the installer's tools", sorted(names))

    # Who may not: a program that is not root, a copy, a second claim, a confirmer that is another program.
    e, t = k.call("run", {"path": "/bin/bitsinstall", "args": ["claim-probe"], "timeout_ms": 10000})
    need("claim-probe: open /dev/bitsos/mcp: errno 13" in t, "a program of uid 1000 cannot even open the gate (EACCES)", t)
    mark = len(serial())
    q.type("cp /bin/bitsinstall /tmp/bi\n")
    time.sleep(1)
    q.type("/tmp/bi claim-probe\n")
    wait_serial(r"bitsinstall: claim-probe: claim: errno 1\b", "a root copy of /bin/bitsinstall is refused (EPERM)", 30, mark)
    wait_serial(r"mcp installer\.claim_refused .*does not run the boot's /bin/bitsinstall", "the kernel says why", 10, mark)
    q.type("bitsinstall claim-probe\n")
    wait_serial(r"bitsinstall: claim-probe: claim: errno 16\b", "the real program cannot take the role while the installer holds it (EBUSY)", 30, mark)
    q.type("/tmp/bi test-confirmer --hold-ms 3000\n")
    wait_serial(r"bitsinstall: confirmer refused: pid \d+ runs /tmp/bi, not /bin/bitsinstall", "a confirmer that runs another file is refused", 30, mark)

    # No confirmer: refused.
    e, t = s.call("reboot", {"dry_run": True})
    need(e and refused(t, "reboot") and "no desktop session" in t, "without a session every request is refused", t)
    decided("no_session", "no", "reboot", mark)

    # The test confirmer (uid 1000 through the kernel's run; it runs on after the answer).
    mark = len(serial())
    k.call("run", {"path": "/bin/bitsinstall", "args": ["test-confirmer", "--answers", "no,yes,silent,injected,no,yes,yes", "--grant", f"pin:{GRANT_S}"], "timeout_ms": 1500})
    wait_serial(r"bitsinstall: confirmer connected: /bin/bitsinstall", "the test confirmer holds the place", 30, mark)
    wait_serial(r"bitsinstall: grant pin set at the keyboard", "the grant for pin is set", 30, mark)
    t_grant = time.time()

    e, t = s.call("system_pin", {"n": "off", "dry_run": True})
    need(not refused(t, "system_pin"), "a pin passes by its grant (the tool itself answered)", t)
    decided("grant", "yes", "system_pin", mark)
    wait_serial(r"bitsinstall: request \d+ system_pin \(pin\) from \S+: yes by the grant", "bitsinstall's journal line: yes by the grant", 10, mark)
    need(not re.search(r"system_pin \(pin\) from \S+: asking at the screen", serial()[mark:]), "no dialog was shown for it")

    e, t = s.call("reboot", {"dry_run": True})
    need(e and refused(t, "reboot") and "said no" in t, "the person said no: refused", t)
    decided("dialog", "no", "reboot", mark)

    e, t = s.call("reboot", {"dry_run": True})
    need(not e and "reboot (dry run, nothing happens)" in t, "the person said yes: the tool ran", t)
    m = decided("dialog", "yes", "reboot", mark)
    wait_serial(r"mcp svc\.call tool=reboot peer=10\.0\.2\.2:\d+ result=ok", "the kernel ran it after the yes", 10, m.start())

    t0 = time.time()
    e, t = s.call("reboot", {"dry_run": True})
    need(e and refused(t, "reboot") and "in time" in t and time.time() - t0 >= 4, "no answer: refused after the dialog's time", t)
    decided("timeout", "no", "reboot", mark)
    wait_serial(r"bitsinstall: dialog \d+ closed at the confirmer: no answer in time", "the confirmer was told to close the dialog", 10, mark)

    e, t = s.call("reboot", {"dry_run": True})
    need(e and refused(t, "reboot") and "injected" in t, "a yes from injected input is refused", t)
    decided("refused", "no", "reboot", mark)

    left = GRANT_S + 2 - (time.time() - t_grant)
    if left > 0:
        time.sleep(left)
    e, t = s.call("system_pin", {"n": "off", "dry_run": True})
    need(e and refused(t, "system_pin") and "said no" in t, "after the grant expired by itself the pin is asked again", t)
    wait_serial(r"bitsinstall: grant pin until \d+ \(unix\) expired", "the journal line of the grant's end", 10, mark)
    decided("dialog", "no", "system_pin", mark)

    # A guarded setting: settingsd's changing tool goes to the installer too, and runs in the service after the yes.
    listed = {t["name"]: t for t in s.rpc("tools/list")["tools"]}
    need("settings_set" in listed and "settings_get" in listed, "settingsd's tools are listed by the service (settings_set for the installer)", sorted(listed))
    e, t = s.call("settings_set", {"key": "mc9a.no_such_key", "value": True})
    need(t and not refused(t, "settings_set"), "settings_set went through the installer to settingsd after the yes (its own answer)", t)
    decided("dialog", "yes", "settings_set", mark)

    # The journal (bitsjournal keeps the kernel's events): decisions and the grant's end.
    end = time.time() + 40
    while True:
        e, t = k.call("events", {"subsystem": "mcp", "name": "svc.install_decided", "since": "1"})
        if not e and all(w in t for w in ["by=grant", "by=dialog", "by=timeout", "by=refused", "by=no_session"]):
            break
        if time.time() > end:
            raise SystemExit(f"RED: the decisions are not all in the events\n{t[:2000]}")
        time.sleep(1)
    print("ok: every decision is a kernel event (grant, dialog, timeout, refused, no_session)", flush=True)
    if TOKEN in serial():
        raise SystemExit("RED: the token appears in the log (R8i)")

    # Last: a confirmed real reboot answers first, then the machine restarts.
    mark = len(serial())
    e, t = s.call("reboot", {})
    need(not e and t.startswith("rebooting once this answer is out"), "the confirmed reboot answered first", t)
    wait_serial(r"mcp svc\.after_answer tool=reboot", "the action waits for the answer", 10, mark)
    wait_serial(r"mcp reboot\.requested", "then the reboot runs", 30, mark)
    print("installer check: green", flush=True)


main()
