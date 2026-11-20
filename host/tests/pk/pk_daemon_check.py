#!/usr/bin/env python3
"""PK-6f on V2 through MCP (QEMU hostfwd) and the root console shell (QMP keys): bitspkgd.
Usage: pk_daemon_check.py <kernel port> <bitsmcp port> <token> <qmp socket> <serial log> <http port> <slow port>.
Exit 0 = green. The fixture mirror is the host's 127.0.0.1:<http port> (the guest's 10.0.2.2), the slow mirror the
same at <slow port>. Root asks pkg.sock from the console; uid 1000 (the kernel's `run`) is neither root nor the
compositor; Claude's tools go through bitsmcp (8766) and the installer with the test confirmer. The first refresh
of the run is Claude's (programs_refresh): nothing is loaded before it, nothing is fetched before the yes."""
import base64
import http.client
import json
import os
import re
import socket
import sys
import time

sys.dont_write_bytecode = True
KPORT, SPORT, TOKEN, QMP_PATH, LOG, HTTP, SLOW = int(sys.argv[1]), int(sys.argv[2]), sys.argv[3], sys.argv[4], sys.argv[5], int(sys.argv[6]), int(sys.argv[7])
D, BB = "/bin/bitspkgd", "/bin/busybox"
W = "/tmp/pkd"
next_id = 0


def need(cond, what, detail=""):
    if not cond:
        raise SystemExit(f"RED: {what}\n{str(detail)[:4000]}")
    print(f"ok   {what}", flush=True)


class Client:
    def __init__(self, port, wait_s):
        self.port = port
        end = time.time() + wait_s
        while True:
            try:
                self.rpc("initialize", {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "pk6f", "version": "1"}})
                return
            except (OSError, http.client.HTTPException, ValueError) as e:
                if time.time() > end:
                    raise SystemExit(f"RED: no MCP answer on 127.0.0.1:{port}: {e}")
                time.sleep(1)

    def rpc(self, method, params=None, timeout=120):
        global next_id
        next_id += 1
        c = http.client.HTTPConnection("127.0.0.1", self.port, timeout=timeout)
        c.request("POST", "/mcp", json.dumps({"jsonrpc": "2.0", "id": next_id, "method": method, "params": params or {}}),
                  {"Authorization": f"Bearer {TOKEN}", "Content-Type": "application/json"})
        r = c.getresponse()
        j = json.loads(r.read() or b"null")
        c.close()
        if r.status != 200 or not j or "result" not in j:
            raise ValueError(f"{method}: HTTP {r.status} {j}")
        return j["result"]

    def call(self, name, args=None, timeout=120):
        r = self.rpc("tools/call", {"name": name, "arguments": args or {}}, timeout)
        return r["isError"], "\n".join(i.get("text", "") for i in r["content"] if i["type"] == "text")


def read(c, path):
    err, t = c.call("read_file", {"path": path})
    if err:
        return None
    try:
        j = json.loads(t)
        return j["text"] if "text" in j else base64.b64decode(j.get("data", "")).decode("utf-8", "replace")
    except (ValueError, KeyError, TypeError):
        return t


def put(c, path, data):
    step = 96 * 1024
    for off in range(0, max(len(data), 1), step):
        err, t = c.call("write_file", {"path": path, "data": base64.b64encode(data[off:off + step]).decode(), "offset": off})
        if err:
            raise SystemExit(f"RED: write_file {path}: {t}")


def run(c, argv, timeout_ms=60000):
    err, t = c.call("run", {"path": argv[0], "args": argv[1:], "timeout_ms": timeout_ms, "kill_after_ms": timeout_ms}, timeout_ms / 1000 + 60)
    if err or "\n--- stdout (" not in t:
        return None, t, t
    head, rest = t.split("\n--- stdout (", 1)
    out, errpart = rest.split("\n--- stderr (", 1)
    body = lambda part: part.split(") ---", 1)[1].removeprefix("\n")
    return head.splitlines()[0], body(out), body(errpart)


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


# QEMU qcodes on V2's German console layout ('-' where the US slash is, '/' = shift+7); no y/z (swapped).
KEYS = {c: [c] for c in "abcdefghijklmnopqrstuvwx0123456789"}
KEYS.update({" ": ["spc"], "-": ["slash"], "\n": ["ret"], "/": ["shift", "7"]})


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


HEAD = f"""#!/bin/sh
P={D}
o() {{ n=$1; shift; "$@" > {W}/$OUT/$n.txt 2>&1; echo "exit $?" >> {W}/$OUT/$n.txt; }}
pid() {{ sed -n 's/.*"plan_id":"\\([^"]*\\)".*/\\1/p' {W}/$OUT/$1.txt; }}
mkdir -p {W}/$OUT
"""

# Root: the sources in settingsd (bitspkgd registers each source's keys when it reads the order). Nothing is
# loaded by this: from here on the only source is the fixture mirror (R9), so Claude's refresh can be the first.
GO0 = "OUT=out0\n" + HEAD + f"""mkdir -p /etc/bitspkg/keyrings
cp {W}/keyrings/* /etc/bitspkg/keyrings/
o order settings set pkg.sources.order fixture
o src0 $P call '{{"op":"sources"}}'
o srv settings set pkg.sources.fixture.servers 'http://10.0.2.2:{HTTP}/main/$repo'
o sig settings set pkg.sources.fixture.sig_db optional
o kr settings set pkg.sources.fixture.keyring archlinux
o sources $P call '{{"op":"sources"}}'
chmod -R a+rX {W}/$OUT
echo done > {W}/$OUT/done
"""

# Root: the ops over the socket (the list is the one Claude's refresh loaded).
GO = "OUT=out\n" + HEAD + f"""o refresh $P call '{{"op":"refresh"}}'
o search $P call '{{"op":"search","query":"jq","limit":5}}'
o plan $P call '{{"op":"plan","action":"install","name":"jq"}}'
id=$(pid plan)
o do $P call "{{\\"op\\":\\"do\\",\\"plan_id\\":\\"$id\\"}}"
o again $P call "{{\\"op\\":\\"do\\",\\"plan_id\\":\\"$id\\"}}"
o list $P call '{{"op":"list"}}'
o space $P call '{{"op":"space","name":"jq"}}'
o rplan $P call '{{"op":"plan","action":"remove","name":"jq"}}'
rid=$(pid rplan)
o rdo $P call "{{\\"op\\":\\"do\\",\\"plan_id\\":\\"$rid\\"}}"
o list2 $P call '{{"op":"list"}}'
o apps {BB} ls -a /apps
o appdata {BB} ls -a /data/appdata
o trash {BB} ls -a /apps/.trash
o space2 $P call '{{"op":"space","name":"jq"}}'
o keys settings list
id > {W}/$OUT/id.txt
chmod -R a+rX {W}/$OUT
echo done > {W}/$OUT/done
"""

# Root: a second source on the slow mirror; its refresh runs on in the background while uid 1000 reads.
GO2 = "OUT=out2\n" + HEAD + f"""o order settings set pkg.sources.order 'fixture slow'
o src0 $P call '{{"op":"sources"}}'
o srv settings set pkg.sources.slow.servers 'http://10.0.2.2:{SLOW}/$repo'
o sig settings set pkg.sources.slow.sig_db optional
o kr settings set pkg.sources.slow.keyring archlinux
( o slowrefresh $P call '{{"op":"refresh"}}'; chmod -R a+rX {W}/$OUT; echo done > {W}/$OUT/slowdone ) &
chmod -R a+rX {W}/$OUT
echo done > {W}/$OUT/done
"""


def root_phase(q, c, script, out, timeout=900):
    q.type(f"sh {W}/{script}\n")
    end = time.time() + timeout
    while time.time() < end and read(c, f"{W}/{out}/done") is None:
        time.sleep(3)
    need(read(c, f"{W}/{out}/done") is not None, f"the root run {script} finished", "\n".join(serial().splitlines()[-40:]))
    return lambda n: read(c, f"{W}/{out}/{n}.txt") or ""


def lines(t):
    out = []
    for l in t.splitlines():
        try:
            out.append(json.loads(l))
        except ValueError:
            pass
    return out


def final(t):
    j = lines(t)
    return j[-1] if j else {}


def as_user(c, req):
    st, out, err = run(c, [D, "call", req])
    return final(out), out + err


def http_log():
    with open(os.path.join(os.path.dirname(LOG), "http.log"), "rb") as f:
        return f.read().decode("utf-8", "replace")


def first_refresh(k):
    """Claude asks for the first refresh (the Mini-PC on 12.11.: no list loaded, no tool to load one). The tool is
    class change: the kernel hands it to the installer as kind program, and nothing is fetched before the yes."""
    s = Client(SPORT, 120)
    want = {"programs_search", "programs_info", "program_install", "program_remove", "programs_refresh"}
    end = time.time() + 60
    while time.time() < end and not want <= {t["name"] for t in s.rpc("tools/list")["tools"]}:
        time.sleep(1)
    tools = {t["name"]: t for t in s.rpc("tools/list")["tools"]}
    need(want <= set(tools), "bitspkgd's five tools are listed by bitsmcp", sorted(tools))
    need(tools["programs_refresh"].get("inputSchema", {}).get("required") is None and "timeout_ms" in str(tools["programs_refresh"]), "programs_refresh takes no argument besides timeout_ms", tools["programs_refresh"])
    e, t = s.call("programs_search", {"query": "jq"})
    need(e and "keine Quelle geladen" in t and "programs_refresh" in t, "with no list loaded programs_search names the tool programs_refresh", t)
    mark = len(serial())
    e, t = s.call("programs_refresh")
    need(e and "programs_refresh was not done:" in t, "programs_refresh without a confirmer is refused", t)
    wait_serial(r"mcp svc\.install_asked id=\d+ tool=programs_refresh kind=program", "the kernel handed programs_refresh to the installer as kind program", 30, mark)
    mark = len(serial())
    err, shown = k.call("run", {"path": "/bin/bitsinstall", "args": ["test-confirmer", "--answers", "no,yes"], "timeout_ms": 1500})
    job = re.search(r"\(call (\d+)\)", shown)
    need(job is not None, "the test confirmer keeps running", shown)
    wait_serial(r"bitsinstall: confirmer connected: /bin/bitsinstall", "the test confirmer holds the place", 30, mark)
    e, t = s.call("programs_refresh", {}, timeout=300)
    need(e and "programs_refresh was not done:" in t, "programs_refresh after Georg's no is refused", t)
    # The confirmer's own lines are its stdout (`output` gives what is new since the last look).
    end = time.time() + 30
    title = r'test-confirmer: ask \d+ \(program, "Paketlisten laden"\): no'
    while time.time() < end and not re.search(title, shown):
        time.sleep(1)
        shown += k.call("output", {"call": int(job.group(1))})[1]
    need(re.search(title, shown), "the dialog for a refresh is titled Paketlisten laden", shown)
    wait_serial(r"bitsinstall: request \d+ programs_refresh \(program\) from \S+: no by dialog", "bitsinstall's journal line: no by the dialog", 30, mark)
    src, raw = as_user(k, '{"op":"sources"}')
    need(src.get("ok") and src["sources"][0].get("packages") is None and "GET" not in http_log(),
         "R9: before the yes nothing was fetched from the mirror and no list is loaded", raw + http_log())
    mark = len(serial())
    e, t = s.call("programs_refresh", {}, timeout=600)
    need(not e and re.search(r"^fixture: aktualisiert, \d+ Pakete, von http://10\.0\.2\.2:%d/main/fixture" % HTTP, t, re.M),
         "with the test confirmer's yes programs_refresh loads the lists and answers with one line per source", t)
    wait_serial(r"bitsinstall: request \d+ programs_refresh \(program\) from \S+: yes by dialog", "bitsinstall's journal line: yes by the dialog", 10, mark)
    wait_serial(r"mcp svc\.install_decided id=\d+ tool=programs_refresh decision=yes by=dialog", "the kernel's event: yes by the dialog", 10, mark)
    need("GET /main/fixture/fixture.db" in http_log(), "the list came from the fixture mirror after the yes", http_log()[-2000:])
    src, raw = as_user(k, '{"op":"sources"}')
    need((src["sources"][0].get("packages") or 0) > 0, "the source now has its package list", raw)
    e, t = s.call("programs_search", {"query": "jq"})
    need(not e and "fixture/jq 1.8.2-1.1" in t, "after Claude's refresh programs_search finds jq", t)
    # This confirmer leaves: the checks further down start without one.
    mark = len(serial())
    k.call("stop", {"call": int(job.group(1))})
    wait_serial(r"bitsinstall: confirmer gone: ", "the test confirmer left", 30, mark)
    return s


def main():
    k = Client(KPORT, 300)
    wait_serial(r"init: started the program service /bin/bitspkgd \(pid \d+\)", "init started bitspkgd after settingsd", 120)
    wait_serial(r"event=pkg\.daemon\.start socket=/run/bitsos/pkg\.sock", "bitspkgd's events reach the journal (/dev/kmsg)", 60)
    end = time.time() + 120
    while time.time() < end and (read(k, "/run/bitsos/apps-state") or "waiting").startswith("waiting"):
        time.sleep(2)
    need(not (read(k, "/run/bitsos/apps-state") or "waiting").startswith("waiting"), "init settled /apps", read(k, "/run/bitsos/apps-state"))
    st, out, err = run(k, [BB, "mkdir", "-p", W + "/keyrings"])
    need(st and st.endswith("exit 0"), "scratch folder", f"{st}{out}{err}")
    for f in sorted(os.listdir("/usr/share/pacman/keyrings")):
        put(k, f"{W}/keyrings/{f}", open(f"/usr/share/pacman/keyrings/{f}", "rb").read())
    put(k, W + "/go0", GO0.encode())
    put(k, W + "/go", GO.encode())
    put(k, W + "/go2", GO2.encode())
    s0, _ = as_user(k, '{"op":"status"}')
    need(s0.get("ok") is True and s0.get("busy") is None, "bitspkgd answers uid 1000's status", s0)

    wait_serial(r"init: started the console shell", "the console shell", 120)
    q = Qmp(QMP_PATH)
    q.type("\n")
    o0 = root_phase(q, k, "go0", "out0")
    need(o0("srv").rstrip().endswith("exit 0") and o0("kr").rstrip().endswith("exit 0"), "the fixture source's keys are bitspkgd's registered keys", o0("src0") + o0("srv") + o0("kr"))
    src = final(o0("sources"))
    need(src.get("ok") and [x["name"] for x in src["sources"]] == ["fixture"] and src["origin"] == "settingsd", "sources: only the fixture, from settingsd's keys", o0("sources"))
    need(src["sources"][0].get("packages") is None, "no package list is loaded yet (a fresh system)", o0("sources"))
    s = first_refresh(k)
    o = root_phase(q, k, "go", "out")
    need("uid=0" in o("id"), "the run was root's", o("id"))
    rf = final(o("refresh"))
    need(rf.get("done") and rf.get("refused") == 0 and rf.get("loaded") == 1 and any("fixture: unverändert" in l for l in rf.get("report", [])),
         "refresh as root over the socket: the list Claude's refresh loaded is unchanged", o("refresh"))
    hits = final(o("search")).get("hits", [])
    need(any(h["repo"] == "fixture" and h["name"] == "jq" for h in hits), "search finds jq", o("search"))
    plan = final(o("plan"))
    need(plan.get("installable") and plan.get("plan_id") and plan.get("program") == "jq" and len(plan.get("packages", [])) == 7,
         "plan install jq: 7 packages, a plan id", o("plan"))
    need(re.match(r"^jq 1\.8\.2-1\.1, 7 Teile, ", plan.get("summary", "")) and plan.get("download", 0) > 0 and plan.get("permissions"),
         "the plan names version, parts, sizes (the dialog's facts) and the permissions wanted", plan)
    do = lines(o("do"))
    last = do[-1] if do else {}
    need(last.get("ok") is True and last.get("done") is True and any("Installiert: jq 1.8.2-1.1+1" in l for l in last.get("report", [])), "do: jq installed, the run ends with done", o("do")[-3000:])
    steps = [(j["progress"], j.get("step")) for j in do[:-1] if j.get("step")]
    order = ["download", "verify", "unpack", "triggers", "switch"]
    ranks = [order.index(p) for p, _ in steps if p in order]
    need(ranks == sorted(ranks) and {"download", "verify", "unpack", "triggers", "switch"} <= {p for p, _ in steps}, "progress lines arrive in order: download, verify, unpack, triggers, switch", steps)
    dl = [j for j in do if j.get("event") == "pkg.download.done"]
    need(dl and dl[-1].get("done_bytes", 0) > 0 and dl[-1].get("total_bytes") == plan["download"], "download progress counts bytes against the plan's total", dl)
    need(final(o("again")).get("error") == "unknown_plan", "a plan runs once", o("again"))
    progs = final(o("list")).get("programs", [])
    need(any(p["name"] == "jq" and p["current"] == "1.8.2-1.1+1" for p in progs), "list shows jq", o("list"))
    need(final(o("space")).get("programs", [{}])[0].get("package", 0) > 0, "space for jq", o("space"))
    rp = final(o("rplan"))
    need(rp.get("installable") and rp.get("plan_id") and rp.get("versions") == ["1.8.2-1.1+1"], "plan remove jq", o("rplan"))
    need("Datenordner" in rp.get("summary", "") and "data_removed" in rp, "the remove plan says what happens to the data folder", rp)
    need(final(o("rdo")).get("done") is True and final(o("list2")).get("programs") == [], "do remove: jq gone", o("rdo") + o("list2"))
    # B2: no leftovers: the program folder, its data folder, the trash, the space report and its keys.
    names = lambda t: [l.strip() for l in t.splitlines() if l.strip() and not l.startswith("exit ")]
    need("jq" not in names(o("apps")), "B2: /apps/jq is gone", o("apps"))
    need("jq" not in names(o("appdata")), "B2: no data folder of jq is left", o("appdata"))
    need(not any(n.endswith("-jq") for n in names(o("trash"))), "B2: nothing of jq stays in the trash", o("trash"))
    sp2 = final(o("space2"))
    need(sp2.get("ok") is not True or not any(p.get("name") == "jq" for p in sp2.get("programs", [])), "B2: space no longer names jq", o("space2"))
    need(not re.search(r"(^|[\s\"/])(app\.)?jq\.", o("keys"), re.M), "B2: no settings key of jq is left", o("keys")[-2000:])

    # uid 1000, not the compositor: reads and plans, never changes.
    up, t = as_user(k, '{"op":"plan","action":"install","name":"jq"}')
    need(up.get("installable") is True and up.get("plan_id") is None and "nur lesen" in up.get("note", ""), "uid 1000 may plan, but its plan gets no id (a reader cannot fill the table)", t)
    for req in ['{"op":"do","plan_id":"%s"}' % plan["plan_id"], '{"op":"switch","name":"jq","version":"1"}', '{"op":"rollback","name":"jq"}', '{"op":"refresh"}']:
        r, t = as_user(k, req)
        need(r.get("error") == "read_only" and "Compositor" in r.get("message", ""), f"uid 1000 refused: {req[:30]}", t)
    r, t = as_user(k, '{"op":"list"}')
    need(r.get("ok") is True and r.get("programs") == [], "nothing was installed by uid 1000", t)

    # Claude over bitsmcp: read tools without a dialog, change tools only after a yes.
    mark = len(serial())
    e, t = s.call("programs_search", {"query": "jq"})
    need(not e and "fixture/jq 1.8.2-1.1" in t, "programs_search answers (class read, no dialog)", t)
    need(not re.search(r"svc\.install_asked id=\d+ tool=programs_search", serial()[mark:]), "the read tool asked nobody")
    e, t = s.call("program_install", {"name": "jq"})
    need(e and "program_install was not done:" in t, "program_install without a confirmer is refused", t)
    wait_serial(r"mcp svc\.install_asked id=\d+ tool=program_install kind=program", "the kernel handed program_install to the installer as kind program", 30, mark)
    r, t = as_user(k, '{"op":"list"}')
    need(r.get("programs") == [], "the refused install changed nothing", t)
    mark = len(serial())
    k.call("run", {"path": "/bin/bitsinstall", "args": ["test-confirmer", "--answers", "no,yes,yes"], "timeout_ms": 1500})
    wait_serial(r"bitsinstall: confirmer connected: /bin/bitsinstall", "the test confirmer holds the place", 30, mark)
    e, t = s.call("program_install", {"name": "jq"}, timeout=300)
    need(e and "program_install was not done:" in t, "program_install after Georg's no is refused", t)
    wait_serial(r"bitsinstall: request \d+ program_install \(program\) from \S+: no by dialog", "bitsinstall's journal line: no by the dialog", 30, mark)
    r, t = as_user(k, '{"op":"list"}')
    need(r.get("programs") == [], "the no changed nothing", t)
    mark = len(serial())
    e, t = s.call("program_install", {"name": "jq"}, timeout=1500)
    need(not e and "Installiert: jq 1.8.2-1.1+1" in t, "with the test confirmer's yes program_install installs jq", t[-3000:])
    wait_serial(r"bitsinstall: request \d+ program_install \(program\) from \S+: asking at the screen", "bitsinstall asked at the screen for kind program", 10, mark)
    wait_serial(r"bitsinstall: request \d+ program_install \(program\) from \S+: yes by dialog", "bitsinstall's journal line: yes by the dialog", 10, mark)
    wait_serial(r"mcp svc\.install_decided id=\d+ tool=program_install decision=yes by=dialog", "the kernel's event: yes by the dialog", 10, mark)
    r, t = as_user(k, '{"op":"list"}')
    need(any(p["name"] == "jq" for p in r.get("programs", [])), "jq is installed again after its removal (B2: a clean reinstall)", t)
    mark = len(serial())
    e, t = s.call("program_remove", {"name": "jq"}, timeout=600)
    need(not e and "jq entfernt" in t, "with the test confirmer's yes program_remove removes jq", t[-3000:])
    wait_serial(r"mcp svc\.install_asked id=\d+ tool=program_remove kind=program", "the kernel handed program_remove to the installer as kind program", 10, mark)
    wait_serial(r"bitsinstall: request \d+ program_remove \(program\) from \S+: yes by dialog", "bitsinstall's journal line: program_remove yes by the dialog", 10, mark)
    r, t = as_user(k, '{"op":"list"}')
    need(r.get("programs") == [], "jq is gone after program_remove", t)

    # A refresh hanging on the slow mirror: list still answers while it runs.
    o2 = root_phase(q, k, "go2", "out2", 300)
    need(o2("kr").rstrip().endswith("exit 0"), "the slow source's keys set", o2("src0") + o2("srv") + o2("kr"))
    end = time.time() + 30
    st_ = {}
    while time.time() < end:
        st_, _ = as_user(k, '{"op":"status"}')
        if st_.get("busy") == "refresh":
            break
        time.sleep(0.5)
    need(st_.get("busy") == "refresh", "the refresh on the slow mirror runs", st_)
    r, t = as_user(k, '{"op":"list"}')
    after, _ = as_user(k, '{"op":"status"}')
    need(r.get("ok") is True and after.get("busy") == "refresh" and read(k, f"{W}/out2/slowdone") is None,
         "list answered while the refresh still waited for the slow mirror", f"{t}\n{after}")
    end = time.time() + 180
    while time.time() < end and read(k, f"{W}/out2/slowdone") is None:
        time.sleep(3)
    sr = final(o2("slowrefresh"))
    need(sr.get("done") and sr.get("refused") == 1 and any("slow" in l for l in sr.get("report", [])), "the slow source was refused, the refresh ended", o2("slowrefresh"))


main()
