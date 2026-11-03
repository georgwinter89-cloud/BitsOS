#!/usr/bin/env python3
"""MC-8a scenario client: the user-space MCP service bitsmcp against the kernel's own server.

Usage:
  service_check.py both <kernel port> <service port> <token>      compare tool by tool, then kill the service
  service_check.py service <service port> <token>                  mcp=service: only the service answers
  service_check.py kill <service port> <token> <qmp socket> <serial log>
                   mcp=service: the service killed from the console shell (QEMU's USB keyboard), init starts it again
Every step prints one line; exit status 0 = green. The service's answers must equal the kernel server's for
each tool the service offers (texts with numbers that move between two calls are compared with the numbers
masked); dropped tools are refused with their reason, installer tools with EPERM (no installer) or the
installer's refusal (MC-9a: nobody confirmed)."""
import http.client
import json
import re
import socket
import sys
import time

MODE = sys.argv[1]
next_id = 0
OFFERED = ["events", "hw_info", "ktest", "log", "metrics", "output", "reload_info", "stop", "system_list", "version"]
INSTALLER = ["install_kernel", "install_system", "reboot", "reload", "system_pin"]


def need(cond, what, detail=""):
    if not cond:
        raise SystemExit(f"RED: {what}\n{str(detail)[:3000]}")
    print(f"ok: {what}")


class Client:
    def __init__(self, port, token, wait_s=0):
        self.port, self.token = port, token
        end = time.time() + wait_s
        while True:
            try:
                self.c = http.client.HTTPConnection("127.0.0.1", port, timeout=60)
                self.init = self.rpc("initialize", {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "mc8a", "version": "1"}})
                return
            except (OSError, http.client.HTTPException, ValueError) as e:
                if time.time() > end:
                    raise SystemExit(f"RED: no MCP answer on 127.0.0.1:{port}: {e}")
                time.sleep(1)

    def post(self, body, token=None):
        self.c.request("POST", "/mcp", json.dumps(body), {"Authorization": f"Bearer {token or self.token}", "Content-Type": "application/json"})
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
        if st == 200 and "error" in j:
            # A JSON-RPC error (a bad argument): compared like a result.
            return "rpc-error", json.dumps(j["error"], sort_keys=True)
        if st != 200 or "result" not in j:
            raise ValueError(f"tools/call {name}: HTTP {st} {j}")
        r = j["result"]
        text = "\n".join(i.get("text", "") for i in r["content"] if i["type"] == "text")
        return r["isError"], text

    def tools(self):
        return {t["name"]: t for t in self.rpc("tools/list")["tools"]}


def masked(t):
    return re.sub(r"\d+", "#", t)


# (tool, arguments, how to compare): exact, or masked (numbers that change between two calls), or head (the
# first line masked: event and log lists move on between the two calls).
CALLS = [
    ("version", {}, "exact"),
    ("hw_info", {"section": "cpu"}, "masked"),
    ("hw_info", {"section": "firmware"}, "masked"),
    ("hw_info", {"section": "kernel"}, "masked"),
    ("hw_info", {"section": "pci"}, "masked"),
    ("reload_info", {}, "masked"),
    ("system_list", {}, "masked"),
    ("metrics", {}, "head"),
    ("events", {"max": 3, "subsystem": "kernel"}, "masked"),
    ("log", {}, "head"),
    ("ktest", {"filter": "kreg.mcp_tools_unique"}, "masked"),
    ("output", {}, "head"),
    ("hw_info", {"section": "nonesuch"}, "exact"),
]


def compare(k, s):
    kt, st = k.tools(), s.tools()
    want = sorted(n for n in kt if n in OFFERED or n in INSTALLER or (kt[n]["annotations"].get("service_pid") and kt[n]["annotations"].get("verified")))
    # MC-10: the service's own two tools (no kernel tool) come on top.
    own = ["access_status", "request_access"]
    need(sorted(st) == sorted(want + own), "the service lists exactly the narrow set, the installer's tools and its own access tools", f"service {sorted(st)}\nwant    {sorted(want + own)}")
    e, t = s.call("access_status", {})
    need(e and "only for a paired client over TLS" in t, "access_status over plain HTTP is refused", t)
    for n in (x for x in st if x not in own):
        need(json.dumps(st[n], sort_keys=True) == json.dumps(kt[n], sort_keys=True), f"tools/list entry of {n} is the kernel's", f"{st[n]}\n{kt[n]}")
    need(s.init["protocolVersion"] == k.init["protocolVersion"] and s.init["serverInfo"] == k.init["serverInfo"], "initialize: the same protocol and server", f"{s.init}\n{k.init}")
    need("user-space service bitsmcp" in s.init["instructions"], "initialize says it is the user-space service")
    for tool, args, how in CALLS:
        ke, ktext = k.call(tool, args)
        se, stext = s.call(tool, args)
        if how == "exact":
            same = (ke, ktext) == (se, stext)
        elif how == "masked":
            same = ke == se and masked(ktext) == masked(stext)
        else:
            same = ke == se and masked(ktext.split("\n")[0]) == masked(stext.split("\n")[0])
        need(same, f"{tool} {json.dumps(args)} answers alike ({how})", f"kernel ({ke}):\n{ktext[:1500]}\nservice ({se}):\n{stext[:1500]}")
    for tool, args in [("run", {"path": "/bin/true"}), ("read_phys", {"addr": 0, "len": 16}), ("write_file", {"path": "/tmp/x", "data": "eA=="}), ("send_keys", {"text": "x"}), ("sched", {})]:
        e, t = s.call(tool, args)
        need(e and t.startswith(f"Refused: {tool} is not offered by the user-space MCP service"), f"{tool} is refused with its reason", t)
    e, t = s.call("reload", {"url": "http://10.0.2.2:1/"})
    # MC-9a: EPERM while no installer runs; with one (init's glue) and nobody to confirm, its refusal.
    need(e and (("goes to the installer" in t and "EPERM" in t) or "reload was not done" in t), "reload goes to the installer: refused", t)
    e, t = s.call("stop", {"pid": 1, "confirm": True})
    need(e and "pid" in t and "not reached" in t, "stop by pid is not reached through the service", t)


def wait_journal(k):
    end = time.time() + 180
    while "journal" not in k.tools():
        if time.time() > end:
            raise SystemExit("RED: bitsjournal's tool journal not registered within 180 s")
        time.sleep(2)
    print("ok: bitsjournal registered its tool")


def journaled(c, who, args, want):
    end = time.time() + 40
    while True:
        e, t = c.call("events", args)
        if not e and t.startswith("journal events ") and all(w in t for w in want):
            print(f"ok: {who}: the journal has {args['name']} with {' '.join(want)}")
            return
        if time.time() > end:
            raise SystemExit(f"RED: {who}: {args} not in the journal\n{t[:2000]}")
        time.sleep(1)


def service_pid(k):
    e, t = k.call("ps", {})
    for line in t.splitlines():
        if "bitsmcp serve" in line:
            m = re.match(r"\s*(\d+)\s", line)
            if m:
                return int(m.group(1)), line
    return None, t


def wrong_token(port, header="Bearer wrong-token-0000"):
    c = http.client.HTTPConnection("127.0.0.1", port, timeout=30)
    h = {"Content-Type": "application/json"}
    if header is not None:
        h["Authorization"] = header
    c.request("POST", "/mcp", '{"jsonrpc":"2.0","id":1,"method":"ping"}', h)
    r = c.getresponse()
    r.read()
    return r.status


def filter_probe(k):
    # The service's filter on V2 itself: open and socket refused, a thread allowed, execve and fork end it.
    for last in ["exec", "fork"]:
        e, t = k.call("run", {"path": "/bin/bitsmcp", "args": ["filter-probe", last], "timeout_ms": 20000})
        lines = t.splitlines()
        # `run` starts programs as an ordinary user, so the probe has no root to give up (no `drop:` line).
        need(not any(l.startswith("drop:") and l != "drop: uid 961" for l in lines), f"filter-probe {last}: drop", t)
        for want in ["filter: on", "open: errno 1", "socket: errno 1", "thread: ok"]:
            need(want in lines, f"filter-probe {last}: {want}", t)
        need("returned" not in t and "signal 31 (SIGSYS)" in lines[0], f"filter-probe {last}: ended by SIGSYS", t)


def both(kport, sport, token):
    k = Client(kport, token, wait_s=150)
    s = Client(sport, token, wait_s=120)
    need(wrong_token(sport) == 401, "a wrong token gets 401 from the service")
    need(wrong_token(sport, None) == 401, "no token gets 401 from the service")
    wait_journal(k)
    compare(k, s)
    # Every action through the service is in the journal: tool, the caller's address, the result.
    calls = {"subsystem": "mcp", "name": "svc.call", "fields": "tool=hw_info", "since": "1"}
    journaled(k, "kernel server", calls, ["tool=hw_info", "peer=10.0.2.2:", "result=ok"])
    journaled(s, "service", calls, ["tool=hw_info", "peer=10.0.2.2:", "result=ok"])
    e, t = s.call("ktest", {"filter": "mcp.class_probe.display"})
    need("not Safe" in t, "a test that is not Safe is skipped through the service", t)
    # MC-9a: refused before anyone was asked (no installer), or decided no by the installer (init's glue).
    end = time.time() + 40
    while True:
        found = [n for n in ("svc.install_refused", "svc.install_decided") if (lambda r: not r[0] and r[1].startswith("journal events ") and "tool=reload" in r[1] and "peer=10.0.2.2:" in r[1])(k.call("events", {"subsystem": "mcp", "name": n, "since": "1"}))]
        if found:
            print(f"ok: kernel server: the journal has {found[0]} with tool=reload")
            break
        if time.time() > end:
            raise SystemExit("RED: kernel server: neither svc.install_refused nor svc.install_decided for reload in the journal")
        time.sleep(1)
    pid, line = service_pid(k)
    need(pid is not None, "ps shows bitsmcp serve", line)
    e, st = k.call("read_file", {"path": f"/proc/{pid}/status", "encoding": "text"})
    st = json.loads(st).get("text", "") if st.startswith("{") else st
    need(re.search(r"^Uid:\s+961\s+961\s+961", st, re.M) is not None, "the service runs as uid 961", st)
    need(re.search(r"^Seccomp:\s+2", st, re.M) is not None, "the service runs under a seccomp filter", st)
    need(re.search(r"^NoNewPrivs:\s+1", st, re.M) is not None or "NoNewPrivs" not in st, "no_new_privs is set", st)
    filter_probe(k)
    e, t = k.call("kill", {"pid": pid, "signal": "SIGKILL"})
    need(not e, f"killed the service (pid {pid})", t)
    time.sleep(0.5)
    e, t = k.call("version", {})
    need(not e and t.startswith("BitsOS V2 kernel"), "the kernel answers after the service was killed", t)
    s2 = Client(sport, token, wait_s=30)
    e, t = s2.call("hw_info", {"section": "cpu"})
    need(not e, "the service answers again after its restart", t)
    pid2, line = service_pid(k)
    need(pid2 is not None and pid2 != pid, f"init started a new service (pid {pid2})", line)
    print(f"killed={pid} restarted={pid2}")


def plain_status(port, token, method="initialize", wait_s=0):
    """The HTTP status of one plain request with `token` (None: the service did not answer within wait_s)."""
    end = time.time() + wait_s
    while True:
        try:
            c = http.client.HTTPConnection("127.0.0.1", port, timeout=30)
            c.request("POST", "/mcp", json.dumps({"jsonrpc": "2.0", "id": 1, "method": method, "params": {}}), {"Authorization": f"Bearer {token}", "Content-Type": "application/json"})
            r = c.getresponse()
            r.read()
            return r.status
        except (OSError, http.client.HTTPException):
            if time.time() > end:
                return None
            time.sleep(1)


def service(sport, token):
    # MC-10: under mcp=service a token over plain HTTP opens nothing but the pairing (403); the full checks of
    # this mode over TLS are host/tests/mcp/95-access.sh's.
    for m in ("initialize", "tools/list", "tools/call"):
        need(plain_status(sport, token, m, wait_s=150 if m == "initialize" else 10) == 403, f"plain HTTP with the right token: 403 for {m} (mcp=service)")
    need(wrong_token(sport) == 401, "a wrong token gets 401")
    need(wrong_token(sport, None) == 401, "no token gets 401")


# The keys `kill -9 <pid>` needs (QEMU qcodes; V2's console layout is German: '-' sits where the US slash is).
KEYS = {c: [c] for c in "abcdefghijklmnopqrstuvwx0123456789"}
KEYS.update({" ": ["spc"], "-": ["slash"], "\n": ["ret"]})


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


def serial(path):
    with open(path, "rb") as f:
        return f.read().decode("utf-8", "replace")


def started_pids(path):
    return [int(p) for p in re.findall(r"^init: started the MCP service \S+ \(pid (\d+)\)", serial(path), re.M)]


def kill(sport, token, qmp_path, log):
    # mcp=service has no second way in through MCP: the console shell (root) kills the service by pid.
    need(plain_status(sport, token, wait_s=60) == 403, "the service answers (403 for plain HTTP)")
    pids = started_pids(log)
    need(pids, "init started the service", "")
    pid = pids[-1]
    end = time.time() + 120
    while "init: started the console shell" not in serial(log):
        if time.time() > end:
            raise SystemExit("RED: no console shell to type into")
        time.sleep(1)
    q = Qmp(qmp_path)
    ended = re.compile(rf"^init: bitsmcp \(pid {pid}\) ended", re.M)
    for _ in range(3):
        q.type("\n")
        q.type(f"kill -9 {pid}\n")
        until = time.time() + 15
        while time.time() < until and not ended.search(serial(log)):
            time.sleep(0.5)
        if ended.search(serial(log)):
            break
    need(ended.search(serial(log)) is not None, f"the console shell killed the service (pid {pid}) and init saw it end")
    need(plain_status(sport, token, wait_s=60) == 403, "the service answers again after its restart (403 for plain HTTP)")
    new = [p for p in started_pids(log) if p != pid]
    need(new, f"init started a new service (pid {new[-1] if new else '?'})")
    print(f"killed={pid} restarted={new[-1]}")


if MODE == "both":
    both(int(sys.argv[2]), int(sys.argv[3]), sys.argv[4])
elif MODE == "service":
    service(int(sys.argv[2]), sys.argv[3])
elif MODE == "kill":
    kill(int(sys.argv[2]), sys.argv[3], sys.argv[4], sys.argv[5])
else:
    raise SystemExit("usage: service_check.py both KPORT SPORT TOKEN | service SPORT TOKEN | kill SPORT TOKEN QMP SERIAL")
print(f"service check {MODE}: green")
