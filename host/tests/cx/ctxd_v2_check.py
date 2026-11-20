#!/usr/bin/env python3
"""CX-5 on V2 through MCP (`run` is uid 1000, Georg's user): the context service as init starts it.

Usage: ctxd_v2_check.py <mcp host port> <token> <run dir> <qmp socket>
The stick carries BITSOS2/CXTEST (docs/, mehr/kopie-1..4/, topics.json, e5.gguf); 20-ctxd-v2.sh puts it there.
"""
import http.client
import json
import os
import re
import subprocess
import sys
import time

PORT, TOKEN, RUN_DIR, QMP = int(sys.argv[1]), sys.argv[2], sys.argv[3], sys.argv[4]
SRC = "/boot/BITSOS2/CXTEST"
HOME = "/data/home/1000/CX-Test"
DOCS, PRIV, MORE = f"{HOME}/Dokumente", f"{HOME}/Privat", f"{HOME}/Mehr"
SECRET = "Kontonummer-Geheimwort-Quastenflosser"
next_id = 0
numbers = {}


def need(cond, what, detail=""):
    if not cond:
        raise SystemExit(f"RED: {what}{': ' + str(detail)[-2500:] if detail else ''}")
    print(f"ok: {what}")


def rpc(c, method, params=None):
    global next_id
    next_id += 1
    body = {"jsonrpc": "2.0", "id": next_id, "method": method, "params": params or {}}
    c.request("POST", "/mcp", json.dumps(body), {"Authorization": f"Bearer {TOKEN}", "Content-Type": "application/json"})
    r = c.getresponse()
    data = r.read()
    if r.status != 200:
        raise ValueError(f"{method}: HTTP {r.status} {data[:200]}")
    return json.loads(data)


def connect(wait_s):
    end = time.time() + wait_s
    while True:
        try:
            c = http.client.HTTPConnection("127.0.0.1", PORT, timeout=300)
            rpc(c, "initialize", {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "cx5", "version": "1"}})
            return c
        except (OSError, http.client.HTTPException, ValueError) as e:
            if time.time() > end:
                raise SystemExit(f"RED: no MCP answer on 127.0.0.1:{PORT}: {e}")
            time.sleep(1)


def run(c, path, args, stdin=None, timeout_ms=240000):
    """A program in the guest as uid 1000: (exit code, stdout, stderr)."""
    a = {"path": path, "args": args, "timeout_ms": timeout_ms}
    if stdin is not None:
        a["stdin"] = stdin
    j = rpc(c, "tools/call", {"name": "run", "arguments": a})
    r = j.get("result", {})
    text = "\n".join(i.get("text", "") for i in r.get("content", []) if i.get("type") == "text")
    m = re.match(r"pid \d+ ended: exit (\d+)\n--- stdout \(\d+ bytes\) ---\n(.*)\n?--- stderr \(\d+ bytes\) ---\n(.*)", text, re.S)
    if r.get("isError") or not m:
        raise SystemExit(f"RED: run {path} {args}: {text[:1500]}")
    return int(m.group(1)), m.group(2), m.group(3)


def sh(c, script, timeout_ms=240000):
    return run(c, "/bin/sh", ["-c", script], timeout_ms=timeout_ms)


def ctxd(c, *args):
    code, out, err = run(c, "/bin/ctxd", ["--system", *args])
    try:
        return code, json.loads(out) if out.strip() else None, err
    except json.JSONDecodeError:
        return code, None, out + err


def status(c):
    code, v, err = ctxd(c, "status")
    return v if code == 0 else None


def wait(c, what, secs, test):
    end = time.time() + secs
    last = None
    while time.time() < end:
        last = status(c)
        if last is not None and test(last):
            return last
        time.sleep(2)
    raise SystemExit(f"RED: timed out ({secs} s): {what}: {json.dumps(last)[:1500] if last else 'no answer'}")


def setting(c, key, value):
    code, out, err = run(c, "/bin/settings", ["--json", "--source", "cx5-check", "set", key, value])
    need(code == 0 and '"ok":true' in out, f"settings set {key}", out + err)


ROOT_CALLS = []


def as_root(c, command):
    """`command` in the root console shell (MCP's run is uid 1000): the script goes to /tmp over MCP, its start
    is typed on the USB keyboard through QMP (as cl/assist_qemu_check.py). What it printed, its exit code last."""
    ROOT_CALLS.append(command)
    result = f"/tmp/cxroot.{len(ROOT_CALLS)}.out"
    script = f"( {command} ) > {result} 2>&1\necho code=$? >> {result}\n"
    code, out, errt = run(c, "/bin/sh", ["-c", f"/bin/busybox rm -f {result}; /bin/busybox cat > /tmp/cxroot"], stdin=script)
    need(code == 0, "the script for the root console is in /tmp", out + errt)
    end = time.time() + 120
    while "init: started the console shell" not in open(os.path.join(RUN_DIR, "serial.log"), errors="replace").read():
        need(time.time() < end, "the console shell started")
        time.sleep(1)
    here = os.path.dirname(os.path.abspath(__file__))
    typed = subprocess.run([sys.executable, "-I", os.path.join(here, "..", "mcp", "qmp_type.py"), QMP, "\\nsh /tmp/cxroot\\n"], capture_output=True, text=True)
    need(typed.returncode == 0, "the command was typed at the console", typed.stdout + typed.stderr)
    end = time.time() + 60
    while True:
        code, out, errt = run(c, "/bin/busybox", ["cat", result])
        if "code=" in out:
            return out
        need(time.time() < end, "the root console ran the command", out + errt)
        time.sleep(1)


def georg_sets(c, key, value):
    """A change that shows an AI more of Georg's files (settingsd leash.rs): uid 1000 is refused, root at the
    console (the scenario's stand-in for his own yes) makes it."""
    code, out, err = run(c, "/bin/settings", ["--json", "--source", "cx5-check", "set", key, value])
    need(code == 1 and '"error":"not_confirmer"' in out, f"settings set {key} as uid 1000 is refused", out + err)
    out = as_root(c, f"/bin/settings --json --confirm set {key} '{value}'")
    need('"ok":true' in out and out.rstrip().endswith("code=0"), f"root at the console sets {key}", out)


def search(c, q, limit=10, mcp=False):
    args = ["call", "context_search", json.dumps({"query": q, "limit": limit})]
    if mcp:
        args.append("--mcp")
    code, v, err = ctxd(c, *args)
    need(code == 0 and v is not None, f"search {q!r}{' (MCP form)' if mcp else ''} answers", err)
    return v


def pid_of(c, exe):
    code, out, _ = sh(c, f'for d in /proc/[0-9]*; do [ "$(readlink $d/exe 2>/dev/null)" = {exe} ] && echo ${{d#/proc/}}; done; true')
    return [int(p) for p in out.split()]


def events(c):
    code, out, _ = sh(c, "cat /data/appdata/ctxd/log/events.jsonl")
    return [json.loads(l) for l in out.splitlines() if l.startswith("{")]


def finished_after(c, t0_count):
    ev = [e for e in events(c) if e.get("name") == "index.finished"]
    return ev[t0_count:] if len(ev) > t0_count else []


def hog(c, n=4, loops=2000000):
    """n busy shells at once (normal class); seconds until the last one ends (/proc/uptime: BusyBox's date has no %N)."""
    script = f"read t0 _ < /proc/uptime; for i in $(seq {n}); do (i=0; while [ $i -lt {loops} ]; do i=$((i+1)); done) & done; wait; read t1 _ < /proc/uptime; echo $t0 $t1"
    code, out, err = sh(c, script, timeout_ms=600000)
    parts = out.split()
    need(code == 0 and len(parts) == 2, "the CPU hog ran", out + err)
    took = float(parts[1]) - float(parts[0])
    need(took > 1.5, f"the CPU hog took measurable time ({took:.2f} s)", out)
    return took


def leaks(items):
    """What of the private folder an answer shows (the query's own words in `why` are the asker's, not a leak)."""
    out = []
    for i in items:
        shown = json.dumps({k: v for k, v in i.items() if k != "why"}, ensure_ascii=False)
        if SECRET in shown or PRIV in shown or "vertraulich" in shown:
            out.append(shown)
    return out


def recall(c, topics, key):
    rs = []
    for t in topics:
        v = search(c, t[key], limit=10)
        got = [i["path"].rsplit("/", 1)[-1].rsplit(".", 1)[0] for i in v["items"]]
        rel = set(t["relevant"])
        rs.append(len(rel & set(got)) / min(10, len(rel)))
    return sum(rs) / len(rs)


def main():
    c = connect(400)
    print("MCP is up")

    # 1. init started it as Georg's user, it waits for nothing and indexes nothing by itself.
    st = wait(c, "ctxd answers on its socket", 240, lambda s: s.get("generation") is not None)
    pids = pid_of(c, "/bin/ctxd")
    need(len(pids) == 1, "one ctxd runs", pids)
    code, out, _ = sh(c, f"grep '^Uid:' /proc/{pids[0]}/status")
    need(out.split()[1:2] == ["1000"], "ctxd runs as uid 1000, not root", out)
    need(st["docs"] == 0 and st["roots"] == [], "no folder named: nothing is indexed", st)
    need("model file missing" in (st["embedder"]["error"] or ""), "without the model the status says why it searches by keywords", st["embedder"])
    code, out, _ = sh(c, "stat -c '%a %u' /data/appdata/ctxd /run/user/1000/ctxd.sock")
    need(out.split() == ["700", "1000", "600", "1000"], "data folder 0700 and socket 0600, both uid 1000's", out)
    code, out, err = run(c, "/bin/assist", ["--mcp"], stdin=json.dumps({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-06-18"}}) + "\n" + json.dumps({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}) + "\n")
    names = [t["name"] for l in out.splitlines() if l.strip() for t in json.loads(l).get("result", {}).get("tools", [])]
    need(code == 0 and "context_search" not in names, "no folder: the assistant is offered no context tools", out + err)

    # 2. C1: the shipped build has no test knobs and no fake embedder.
    code, out, err = sh(c, "mkdir -p /tmp/cx-fake && echo 'Hallo Steuer' > /tmp/cx-fake/a.txt && /bin/ctxd --data /tmp/cx-fake-data --root /tmp/cx-fake --embedder fake index")
    need(code == 0 and json.loads(out)["embedded_chunks"] == 0, "the shipped build refuses the fake embedder (keywords only)", out + err)
    code, out, err = sh(c, "CTXD_TEST_DATA=/tmp/cx-knob /bin/ctxd --system status")
    need(code == 0 and json.loads(out)["data"] == "/data/appdata/ctxd", "CTXD_TEST_DATA means nothing to the shipped build", out + err)

    # 3. The desk's way: uid 1000 copies test folders (never Georg's own) and names them in the settings.
    code, out, err = sh(c, f"mkdir -p {HOME} {PRIV} && cp -r {SRC}/docs {DOCS} && "
                           f"printf 'Brief an den Steuerberater zur Steuererklärung 2025. {SECRET} bitte nie weitergeben.\\n' > {PRIV}/vertraulich.txt && ls {DOCS} | wc -l")
    need(code == 0 and out.strip() == "151", "151 test documents copied by uid 1000", out + err)
    end = time.time() + 30
    while run(c, "/bin/settings", ["--json", "get", "context.roots"])[0] != 0 and time.time() < end:
        time.sleep(0.5)
    need(run(c, "/bin/settings", ["--json", "get", "context.roots"])[0] == 0, "ctxd registered its keys with settingsd")
    t = time.time()
    # A private folder more is anyone's to add; a folder an AI may see takes Georg's own yes.
    setting(c, "context.private_roots", PRIV)
    georg_sets(c, "context.roots", DOCS)
    st = wait(c, "both folders indexed (keywords)", 300, lambda s: s["docs"] == 152 and not s["indexing"].get("running"))
    numbers["keyword_index_s"] = round(time.time() - t, 1)
    need(st["embedder"]["id"] is None, "still keywords only (no model yet)", st["embedder"])
    v = search(c, "Steuererklärung 2025")
    need(v.get("partial") is True and any("steuer-0" in i["path"] for i in v["items"]), "a keyword search finds the tax documents and says it ran on keywords", v)

    # 4. The model: one file, checked and copied in by uid 1000; ctxd starts anew and embeds.
    code, out, err = run(c, "/bin/ctxd", ["model-install", f"{SRC}/e5.gguf"], timeout_ms=300000)
    need(code == 0 and "Modell eingesetzt" in out, "model-install checks and takes the model file", out + err)
    code, out, err = run(c, "/bin/ctxd", ["model-install", f"{SRC}/topics.json"])
    need(code != 0 and "Modell" in err, "a wrong file is refused", out + err)
    n0 = len([e for e in events(c) if e.get("name") == "index.finished"])
    st = wait(c, "the model loaded and every document embedded", 600,
              lambda s: (s["embedder"]["id"] or "").startswith("llama/") and s["embedder"]["index_embedder"] == s["embedder"]["id"] and s["docs"] == 152)
    fin = [e for e in finished_after(c, n0) if e.get("embedded", 0) > 0]
    need(fin, "an index run embedded the documents", finished_after(c, n0))
    numbers["embed_152_docs_s"] = fin[-1]["seconds"]
    numbers["docs_per_s_152"] = round(fin[-1]["embedded"] / max(fin[-1]["seconds"], 0.001), 1)
    print(f"indexing speed (152 docs, model): {numbers['docs_per_s_152']} docs/s")
    topics = json.loads(sh(c, f"cat {SRC}/topics.json")[1])["topics"]
    rq, ra = recall(c, topics, "query"), recall(c, topics, "query_alt")
    numbers["recall_query"], numbers["recall_alt"] = round(rq, 2), round(ra, 2)
    need(rq >= 0.62 and ra >= 0.47, f"recall@10 with the model on V2 as on CachyOS (query {rq:.2f}, alt {ra:.2f}; host 0.66 / 0.51)")

    # 5. Privacy: the desktop sees the private folder; what goes to an AI never.
    v = search(c, f"Steuerberater {SECRET}", limit=20)
    need(any("vertraulich" in i["path"] for i in v["items"]), "the desktop's search finds the private document", v)
    for q in [f"Steuerberater {SECRET}", "Steuererklärung 2025"]:
        v = search(c, q, limit=500, mcp=True)
        text = json.dumps(v, ensure_ascii=False)
        need(not leaks(v["items"]) and PRIV not in text and "vertraulich" not in text, f"the MCP form of {q!r} carries nothing of the private folder", leaks(v["items"]))
        need(all(len(i.get("snippet") or "") <= 200 for i in v["items"]), "MCP snippets are at most 200 characters")
    lines = [{"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-06-18"}},
             {"jsonrpc": "2.0", "id": 2, "method": "tools/list"},
             {"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {"name": "context_search", "arguments": {"query": f"Steuerberater {SECRET}", "limit": 50}}},
             {"jsonrpc": "2.0", "id": 4, "method": "tools/call", "params": {"name": "workspace_open", "arguments": {"query": "Steuererklärung 2025"}}}]
    code, out, err = run(c, "/bin/assist", ["--mcp"], stdin="".join(json.dumps(l) + "\n" for l in lines))
    answers = {a.get("id"): a for a in (json.loads(l) for l in out.splitlines() if l.strip())}
    tools = {t["name"]: t for t in answers.get(2, {}).get("result", {}).get("tools", [])}
    need({"context_search", "workspace_open", "workspace_list"} <= set(tools) and "context_forget" not in tools, "the assistant offers ctxd's tools (forgetting is not offered)", sorted(tools))
    need(tools["context_search"]["annotations"]["readOnlyHint"] is True and tools["workspace_open"]["annotations"]["readOnlyHint"] is False, "searching reads, a workspace change asks", tools["workspace_open"])
    found = answers.get(3, {}).get("result", {})
    ftext = json.dumps(found, ensure_ascii=False)
    need(found.get("isError") is False and "steuer-0" in ftext, "context_search through the assistant finds the tax documents", ftext)
    got = json.loads(found["content"][0]["text"]) if found.get("content") else {}
    need(not leaks(got.get("items", [])) and "vertraulich" not in ftext and PRIV not in ftext, "... and nothing of the private folder", leaks(got.get("items", [])))
    need(answers.get(4, {}).get("result", {}).get("isError") is True, "workspace_open is not run unasked (nobody at this bell)", answers.get(4))

    # 6. File events: a new, a renamed and a deleted document are seen without a rescan. Each step must come
    # through a file event (watch.wake) with no restart in between (a restart would index anew by itself).
    def named(n):
        return len([e for e in events(c) if e.get("name") == n])
    def through_event(what, wakes, restarts):
        need(named("watch.wake") > wakes and named("service.restart") == restarts, f"{what}: through a file event, not a rescan or a restart",
             f"watch.wake {wakes} -> {named('watch.wake')}, service.restart {restarts} -> {named('service.restart')}")
    need(named("watch.started") >= 1 and named("watch.unavailable") == 0 and named("watch.failed") == 0, "the file watch runs")
    word = "Zebrastreifenverwaltungsamt"
    wakes, restarts = named("watch.wake"), named("service.restart")
    sh(c, f"printf 'Antrag beim {word} für den Zebrastreifen vor dem Haus.\\n' > {DOCS}/neu-antrag.txt")
    end, hit = time.time() + 60, None
    while time.time() < end and not hit:
        hit = [i for i in search(c, word)["items"] if i["path"].endswith("neu-antrag.txt")]
        time.sleep(1)
    need(hit, "a new file is found within a minute (file events)")
    through_event("the new file", wakes, restarts)
    doc = hit[0]["doc"]
    wakes = named("watch.wake")
    sh(c, f"mv {DOCS}/neu-antrag.txt {DOCS}/antrag-zebrastreifen.txt")
    end, ok = time.time() + 60, False
    while time.time() < end and not ok:
        items = search(c, word)["items"]
        ok = bool(items) and items[0]["path"].endswith("antrag-zebrastreifen.txt") and items[0]["doc"] == doc
        time.sleep(1)
    need(ok, "a renamed file keeps its document id and shows its new name")
    through_event("the rename", wakes, restarts)
    wakes = named("watch.wake")
    sh(c, f"rm {DOCS}/antrag-zebrastreifen.txt")
    end, gone = time.time() + 60, False
    while time.time() < end and not gone:
        # Other documents can share 4-grams with the word; the deleted one must be the one that is gone.
        gone = not any(i["doc"] == doc for i in search(c, word, limit=50)["items"])
        time.sleep(1)
    need(gone, "a deleted file leaves the index")
    through_event("the deletion", wakes, restarts)
    need(not any(word in json.dumps(e, ensure_ascii=False) for e in events(c)), "the event log holds no query text")

    # 7. B4/B5: indexing gives way. Four busy shells (Normal class) on the four CPUs, alone and while 604 more
    # documents are read in; the median of three each. SCHED_IDLE has weight 3 against Normal's 1024
    # (crates/sched attr.rs), but tonight two llama threads still took a whole CPU from the four shells (the
    # balancing places threads by count), so ctxd also pauses its embedding while
    # less than half the machine is idle (prio.rs Quiet). Measured: the CPU ctxd and llama-server take while the
    # hogs run, and the hogs' wall time.
    # Best of three each: the shared host's load only ever adds time (alone ranged 2.9 to 6.8 s between runs).
    alone = min(hog(c) for _ in range(3))
    sh(c, f"cp -r {SRC}/mehr {MORE}")
    n0 = len([e for e in events(c) if e.get("name") == "index.finished"])
    georg_sets(c, "context.roots", f"{DOCS};{MORE}")
    wait(c, "indexing of the copies started", 120, lambda s: bool(s["indexing"].get("running")) or s["docs"] > 152)
    llama = pid_of(c, "/bin/ctxd-llama")
    code, out, _ = sh(c, f"cat /proc/{llama[0]}/task/*/stat" if llama else "true")
    pol = [l[l.rfind(")") + 2:].split()[38] for l in out.splitlines() if ")" in l]
    need(llama and pol and all(p == "5" for p in pol), "llama-server runs in the Background class (SCHED_IDLE) in every thread", pol)
    def ticks(pid):
        code, out, _ = sh(c, f"cat /proc/{pid}/stat 2>/dev/null")
        f = out[out.rfind(")") + 2:].split() if ")" in out else []
        return int(f[11]) + int(f[12]) if len(f) > 12 else 0
    code, out, _ = sh(c, "cut -d' ' -f19,41 /proc/self/stat")
    print(f"the hog's own nice and policy: {out.strip()}")
    t_llama, t_ctxd, w0 = ticks(llama[0]), ticks(pid_of(c, "/bin/ctxd")[0]), time.time()
    busy = min(hog(c) for _ in range(3))
    w1 = time.time()
    numbers["llama_cpu_s_during_hogs"] = (ticks(llama[0]) - t_llama) / 100
    numbers["ctxd_cpu_s_during_hogs"] = (ticks(pid_of(c, "/bin/ctxd")[0]) - t_ctxd) / 100
    numbers["hogs_wall_s"] = round(w1 - w0, 1)
    print(f"during the hogs ({w1 - w0:.1f} s wall): llama-server {numbers['llama_cpu_s_during_hogs']} s CPU, ctxd {numbers['ctxd_cpu_s_during_hogs']} s CPU")
    need(any(e.get("name") == "index.paused" for e in events(c)), "ctxd paused its indexing while the machine was busy")
    after = status(c)
    need(after and (after["indexing"].get("running") or after["docs"] < 756), "the hog ran while ctxd was still indexing", after and after["indexing"])
    numbers["hog_alone_s"], numbers["hog_while_indexing_s"] = alone, busy
    st = wait(c, "the copies indexed", 900, lambda s: s["docs"] == 756 and not s["indexing"].get("running") and s["embedder"]["index_embedder"] == s["embedder"]["id"])
    fin = [e for e in finished_after(c, n0) if e.get("embedded", 0) >= 600]
    need(fin, "the copies were embedded in one run", finished_after(c, n0))
    numbers["docs_per_s_604"] = round(fin[-1]["embedded"] / max(fin[-1]["seconds"], 0.001), 1)
    print(f"hog alone {alone:.2f} s, while indexing {busy:.2f} s; indexing speed (604 docs) {numbers['docs_per_s_604']} docs/s")
    taken = numbers["llama_cpu_s_during_hogs"] + numbers["ctxd_cpu_s_during_hogs"]
    need(taken <= 0.05 * 4 * numbers["hogs_wall_s"], f"ctxd and llama-server take at most 5 % of the four CPUs while the hogs run ({taken:.2f} s CPU in {numbers['hogs_wall_s']} s)")
    # The hard check is the CPU share above; the wall time (best of three each) moves with the host's load.
    need(busy <= alone * 1.3, f"a busy program keeps its pace while ctxd indexes ({busy:.2f} s vs {alone:.2f} s alone)")

    # 8. CX-3: the desktop reports opens on the socket; learning ties them to the query and forgets on request.
    a_path, b_path = f"{DOCS}/steuer-01.txt", f"{DOCS}/steuer-07.txt"
    search(c, "Steuererklärung 2025")
    for p in (a_path, b_path):
        code, v, err = ctxd(c, "call", "open", json.dumps({"path": p}))
        need(code == 0 and v and v.get("doc"), f"an open of {p.rsplit('/', 1)[1]} is recorded", err)
    items = search(c, "Steuererklärung 2025", limit=20)["items"]
    b = [i for i in items if i["path"] == b_path]
    need(b and "Oft zusammen geöffnet mit" in json.dumps(b[0]["why"], ensure_ascii=False), "the why names the document it was opened with", b)
    code, v, err = ctxd(c, "forget", "usage")
    need(code == 0 and v["events"] >= 3, "forget usage empties the usage log", v or err)
    code, out, _ = sh(c, "cat /data/appdata/ctxd/usage/events-*.jsonl 2>/dev/null | wc -c")
    need(out.strip() == "0", "no usage left on /data", out)

    # 9. Restarted when it dies.
    old = pid_of(c, "/bin/ctxd")
    sh(c, f"kill -9 {old[0]}")
    end, new = time.time() + 60, []
    while time.time() < end:
        new = [p for p in pid_of(c, "/bin/ctxd") if p not in old]
        if new and status(c):
            break
        time.sleep(1)
    need(new and status(c) is not None, "after kill -9 init starts ctxd again and it answers", new)
    need(status(c)["docs"] == 756, "the index survived (no rebuild from nothing)")
    with open(f"{RUN_DIR}/numbers.json", "w") as f:
        json.dump(numbers, f, indent=1)
    print("numbers:", json.dumps(numbers))


main()
