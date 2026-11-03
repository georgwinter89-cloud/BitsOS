#!/usr/bin/env python3
"""AC-3 scenario client (10-actions.sh): brave-act on V2 (QEMU), driven over MCP.

Usage: v2_act.py <mcp host port> <token> <run dir>
bitscomp (headless backend, 1280x800) and BusyBox httpd serving host/tests/ac/site from inside the guest on
127.0.0.1 (the name ac3.test; every other name is NOTFOUND: no internet), brave-act as uid 1000 (MCP run); its
seven tools in tools/list; every action once through MCP; file: URLs refused; a page that never answers against
timeout_ms (a sleeping CGI), a page larger than max_chars cut by read_page; the change actions refused while their
settings keys are deny (settings_set without confirmed refused), still refused after an MCP client set allow itself
(settings_set confirmed: source mcp), then allowed by the shell's `settings` command (as Georg would); the
download compared byte for byte; Brave killed and started again by the next call; brave-act's stop ends its tools
and its Brave (the pipe closes). Then a Brave started without brave-act: the tools say so, brave_open_url hands its
URL over. Times are printed, never judged (QEMU). Exit 0 = green."""
import base64
import hashlib
import json
import os
import re
import sys
import time

sys.dont_write_bytecode = True
HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.join(HERE, "..", "wy"))
RUN = sys.argv[3]
sys.argv = sys.argv[:3] + ["-", RUN, "-"]
import v2_check as v  # noqa: E402
from v2_check import Job, call, ctl, need  # noqa: E402

v.ENV = dict(v.ENV, BRAVE_THEME="light")
SITE_DIR = "/tmp/ac-site"
PORT = 8090
BASE = f"http://ac3.test:{PORT}"
RULES = f"--host-resolver-rules=MAP ac3.test 127.0.0.1, MAP * ~NOTFOUND"
TOOLS = ["brave_open_url", "brave_tabs", "brave_read_page", "brave_screenshot_tab", "brave_click_link",
         "brave_fill_form", "brave_download"]
CHANGE = {"brave_fill_form", "brave_download"}


def act(c, name, args, timeout_ms=60000):
    t0 = time.time()
    err, text, images = call(c, name, dict(args, timeout_ms=timeout_ms))
    print(f"  {name} {json.dumps(args)[:120]} -> {'ERROR ' if err else ''}{text[:240]} ({time.time() - t0:.1f} s)", flush=True)
    return err, text, images


def ok(c, name, args, timeout_ms=60000):
    err, text, images = act(c, name, args, timeout_ms)
    need(not err, f"{name} answers", text)
    return json.loads(text), images


def listed(c):
    j = v.rpc(c, "tools/list")
    return {t["name"]: t for t in j["result"]["tools"]}


def brave_pids(c):
    _, text, _ = call(c, "ps", {})
    return [l for l in text.splitlines() if re.search(r"\bbrave\b", l) and "brave-act" not in l and "--type=" not in l]


def start_act(c):
    act_job = Job(c, "/bin/brave-act", ["--brave-arg", "--test-type", "--brave-arg", RULES])
    act_job.wait_for(r"registered 7", "brave-act registered its seven tools", 30)
    return act_job


def main():
    c = v.connect(300)
    comp = Job(c, "/bin/bitscomp", ["--backend", "headless", "--size", "1280x800", "--socket", v.SOCK])
    comp.wait_for(r"backend=headless size=1280x800", "bitscomp started headless at 1280x800")

    # The test site, from inside the guest.
    for f in sorted(os.listdir(os.path.join(HERE, "site"))):
        data = open(os.path.join(HERE, "site", f), "rb").read()
        err, text, _ = call(c, "write_file", {"path": f"{SITE_DIR}/{f}", "data": base64.b64encode(data).decode(), "parents": True})
        need(not err, f"site file {f} written into the guest", text)
    # BusyBox httpd sends no Content-Type for a suffix it does not know (Brave would sniff data.bin as text): the
    # config names .bin as a download (its `.ext:type` lines, networking/httpd.c).
    err, text, _ = call(c, "write_file", {"path": "/tmp/ac-httpd.conf", "text": ".bin:application/octet-stream\n"})
    need(not err, "httpd's config written", text)
    httpd = Job(c, "/bin/sh", ["-c", f"exec httpd -f -p 127.0.0.1:{PORT} -h {SITE_DIR} -c /tmp/ac-httpd.conf"])
    _, text, _ = call(c, "run", {"path": "/bin/sh", "args": ["-c", f"sleep 1; wget -q -O - http://127.0.0.1:{PORT}/index.html | head -c 200"], "timeout_ms": 20000})
    need("AC3 index" in text, "BusyBox httpd serves the site on 127.0.0.1 in the guest", text + httpd.poll())

    act_job = start_act(c)
    tools = listed(c)
    for n in TOOLS:
        need(n in tools, f"tools/list has {n}")
        a = tools[n].get("annotations", {})
        print(f"  {n}: owner={a.get('owner')} safety={a.get('safety')} service_pid={a.get('service_pid')} "
              f"readOnlyHint={a.get('readOnlyHint')} destructiveHint={a.get('destructiveHint')}", flush=True)
        need(a.get("owner") == "brave" and a.get("safety") == ("change" if n in CHANGE else "read"),
             f"{n}: owner brave, safety {'change' if n in CHANGE else 'read'}", json.dumps(tools[n])[:600])

    # 1. open_url starts Brave (through the launcher, uid 1000, the DevTools pipe) and loads the page.
    t0 = time.time()
    o, _ = ok(c, "brave_open_url", {"url": f"{BASE}/index.html"}, 300000)
    print(f"Brave started and showed index.html {time.time() - t0:.1f} s after the call (QEMU, not judged)", flush=True)
    need(o["title"] == "AC3 index", "brave_open_url: the page loaded (title AC3 index)", json.dumps(o))
    tab = o["tab"]
    need(any("app_id=brave-browser" in l for l in ctl(c, "list").splitlines()), "Brave's window is in the desktop")
    ctl(c, "max")
    o2, _ = ok(c, "brave_open_url", {"url": f"{BASE}/second.html"})
    need(o2["title"] == "AC3 second" and o2["tab"] != tab, "brave_open_url: a second tab", json.dumps(o2))
    err, text, _ = act(c, "brave_open_url", {"url": "chrome://settings"})
    need(err and "not an http" in text, "brave_open_url refuses chrome:// URLs", text)
    err, text, _ = act(c, "brave_open_url", {"url": "file:///etc/passwd"})
    need(err and "not an http" in text, "brave_open_url refuses file: URLs (read_page would read any file)", text)

    # A page that never answers (a CGI that only sleeps, on a second httpd in a folder of run's own user; the site's
    # folder belongs to write_file's): timeout_ms ends the call; brave-act goes on.
    hang_dir = "/tmp/ac-hang"
    _, text, _ = call(c, "run", {"path": "/bin/sh", "args": ["-c", f"mkdir -p {hang_dir}/cgi-bin && printf '#!/bin/sh\\nsleep 600\\n' > {hang_dir}/cgi-bin/hang && chmod 755 {hang_dir}/cgi-bin/hang && echo made"], "timeout_ms": 20000})
    need("made" in text, "the hanging CGI is made", text)
    hang = Job(c, "/bin/sh", ["-c", f"exec httpd -f -p 127.0.0.1:{PORT + 1} -h {hang_dir}"])
    t1 = time.time()
    err, text, _ = act(c, "brave_open_url", {"url": f"http://ac3.test:{PORT + 1}/cgi-bin/hang"}, 5000)
    need(err and ("in time" in text or "timeout_ms" in text), "a page that never answers ends at timeout_ms", text)
    print(f"  the hanging page's call ended after {time.time() - t1:.1f} s (not judged)", flush=True)
    _, text, _ = act(c, "brave_tabs", {})
    need("tabs" in text, "brave-act answers after the timed-out call", text)

    # A page larger than an answer (1.2 million characters, 3.6 MB, in a <pre> of short lines: one long line takes
    # minutes to lay out in QEMU): read_page cuts it at max_chars and says so; the answer fits 4 MiB.
    _, text, _ = call(c, "run", {"path": "/bin/sh", "args": ["-c",
        f"{{ echo '<html><head><meta charset=utf-8><title>AC3 big</title></head><body><pre>'; yes '{'語' * 100}' | head -n 12000; "
        f"echo '</pre></body></html>'; }} > {hang_dir}/big.html; wc -c < {hang_dir}/big.html"], "timeout_ms": 60000})
    print(f"  big.html: {text.strip()[-60:]} bytes", flush=True)
    bo, _ = ok(c, "brave_open_url", {"url": f"http://ac3.test:{PORT + 1}/big.html?run={int(time.time())}"}, 240000)
    err, text, _ = act(c, "brave_read_page", {"tab": bo["tab"], "max_chars": 1000000, "links": False}, 60000)
    need(not err and len(text.encode()) < 4 << 20, f"brave_read_page of a 3.6 MB page answers ({len(text.encode())} bytes)", text[:300])
    bj = json.loads(text)
    need(bj["truncated"] is True and bj["text"].startswith("語語語"), "brave_read_page says the text was cut", text[:300])
    call(c, "stop", {"call": hang.n})

    # 2. tabs, read_page, screenshot_tab, click_link.
    t, _ = ok(c, "brave_tabs", {})
    ids = [x["tab"] for x in t["tabs"]]
    need(tab in ids and o2["tab"] in ids, "brave_tabs lists both tabs", json.dumps(t))
    r, _ = ok(c, "brave_read_page", {"tab": tab})
    need("Hello from the AC-3 test site: Grüße" in r["text"], "brave_read_page: the page's text", json.dumps(r)[:800])
    need(any(l[1].endswith("/second.html") for l in r["links"]), "brave_read_page: its links", json.dumps(r["links"]))
    s, images = ok(c, "brave_screenshot_tab", {"tab": tab})
    need(len(images) == 1 and images[0][:8] == b"\x89PNG\r\n\x1a\n", "brave_screenshot_tab: a PNG", json.dumps(s))
    open(os.path.join(RUN, "tab.png"), "wb").write(images[0])
    w, h, px = v.decode_png(images[0])
    green = v.count(px, (0x33, 0xaa, 0x66))
    print(f"  screenshot {w}x{h}, {green} pixels of the green box (300x120 CSS px)", flush=True)
    # The box is 300x120 CSS pixels, more device pixels at a scale over 1 (V2's bitscomp: 1.25).
    need(w >= 600 and h >= 300 and green >= 300 * 120 * 9 // 10, "the screenshot shows the page (its green box)")
    k, _ = ok(c, "brave_click_link", {"tab": tab, "text": "zweite"})
    need(k["title"] == "AC3 second", "brave_click_link follows a link by its text", json.dumps(k))
    k, _ = ok(c, "brave_click_link", {"tab": tab, "href": "index.html"})
    need(k["title"] == "AC3 index", "brave_click_link follows a link by its href", json.dumps(k))
    err, text, _ = act(c, "brave_click_link", {"tab": tab, "text": "Skript"})
    need(err and "only http" in text, "brave_click_link does not follow javascript: links", text)

    # 3. The change actions: refused without permission.
    fill = {"tab": tab, "fields": {"who": "Claude", "agree": True, "color": "blau"}, "submit": True}
    dl = {"url": f"{BASE}/data.bin"}
    for n, args in (("brave_fill_form", fill), ("brave_download", dl)):
        err, text, _ = act(c, n, args)
        need(err and f"claude.actions.{n}" in text and "not permitted" in text, f"{n} is refused without permission", text)
        _, text, _ = act(c, "settings_get", {"key": f"claude.actions.{n}"})
        need("deny" in text, f"its key claude.actions.{n} is deny", text)
        err, text, _ = act(c, "settings_set", {"key": f"claude.actions.{n}", "value": "allow"})
        need(err and "needs_confirm" in text, f"settings_set of claude.actions.{n} without confirmed is refused", text)
    _, text, _ = call(c, "list_dir", {"path": "/tmp/Downloads"})
    need("data.bin" not in text, "nothing was downloaded while refused", text)

    # An MCP client that allows itself (settings_set confirmed, without Georg): settingsd takes the value, the gate
    # does not (the journal's source is mcp).
    for n, args in (("brave_fill_form", fill), ("brave_download", dl)):
        _, text, _ = act(c, "settings_set", {"key": f"claude.actions.{n}", "value": "allow", "confirmed": True})
        need("allow" in text, f"settings_set wrote claude.actions.{n} = allow", text)
        err, text, _ = act(c, n, args)
        need(err and "over MCP" in text, f"{n} stays refused after an allow set over MCP", text)

    # 4. Allowed by the shell's settings command (as Georg would, source cli): both work.
    # settingsd journals changes only: the MCP-set allow is first put back to deny, then allowed.
    for n in CHANGE:
        call(c, "run", {"path": "/bin/settings", "args": ["set", f"claude.actions.{n}", "deny", "--confirm"], "timeout_ms": 20000})
        _, text, _ = call(c, "run", {"path": "/bin/settings", "args": ["set", f"claude.actions.{n}", "allow", "--confirm"], "timeout_ms": 20000})
        print(f"  settings set claude.actions.{n} allow: {text.strip()[:160]}", flush=True)
        _, text, _ = act(c, "settings_history", {"n": 1, "prefix": f"claude.actions.{n}"})
        need('"cli"' in text or "cli" in text, f"claude.actions.{n} = allow from the shell (source cli)", text)
    err, text, _ = act(c, "brave_fill_form", {"tab": tab, "fields": {"pw": "geheim"}})
    need(err and "password" in text, "brave_fill_form refuses password fields", text)
    f, _ = ok(c, "brave_fill_form", fill)
    need(f["title"] == "AC3 result who=Claude agree=yes color=b" and f["submitted"], "brave_fill_form filled and submitted the form", json.dumps(f))
    d, _ = ok(c, "brave_download", dl, 120000)
    err, text, _ = call(c, "read_file", {"path": d["path"]})
    got = base64.b64decode(json.loads(text)["data"]) if not err else b""
    want = open(os.path.join(HERE, "site", "data.bin"), "rb").read()
    need(hashlib.sha256(got).hexdigest() == hashlib.sha256(want).hexdigest() and d["path"].endswith("/data.bin"),
         f"brave_download: {d['path']} has the file's {len(want)} bytes", json.dumps(d))
    err, text, _ = act(c, "brave_download", {"url": f"{BASE}/second.html"})
    need(err and "did not start a download" in text, "brave_download of a page is an error", text)
    for n in CHANGE:
        act(c, "settings_set", {"key": f"claude.actions.{n}", "value": "deny", "confirmed": True})

    # Brave killed: the next call sees its launcher end and starts Brave again.
    def browser_pids():
        # ps: 13 columns, then the command; Brave's browser process is the tree's brave (not bin/brave, the launcher).
        out = []
        for l in brave_pids(c):
            f = l.split(None, 13)
            if len(f) == 14 and f[0].isdigit() and f[13].split()[0].endswith("/brave") and "/bin/" not in f[13].split()[0]:
                out.append(f[0])
        return out
    pids = browser_pids()
    need(pids, "Brave's browser process is in ps", "\n".join(brave_pids(c)))
    pid = pids[0]
    call(c, "run", {"path": "/bin/sh", "args": ["-c", f"kill -KILL {pid}"], "timeout_ms": 10000})
    end = time.time() + 60
    while pid in browser_pids():
        need(time.time() < end, f"Brave {pid} ended on SIGKILL", "\n".join(brave_pids(c)))
        time.sleep(1)
    time.sleep(2)
    t0 = time.time()
    o3, _ = ok(c, "brave_open_url", {"url": f"{BASE}/second.html"}, 300000)
    need(o3["title"] == "AC3 second", "after Brave was killed, the next call started it again", json.dumps(o3))
    print(f"  Brave started again in {time.time() - t0:.1f} s (QEMU, not judged)", flush=True)

    # brave-act's B6 events: on its stderr (MCP output); /dev/kmsg is root's to write (0644 as on Linux), so a uid
    # 1000 service's lines do not reach the kernel log.
    evtext = act_job.poll()
    for e in ["registered tools=7", "brave.start", "brave.up", "perm.refused", "perm.self_granted", "perm.allowed", "download.done", "brave.gone"]:
        need(e in evtext, f"brave-act's event {e} is in its output")

    # 5. brave-act's end: its tools vanish, its Brave ends with the pipe.
    _, text, _ = call(c, "stop", {"call": act_job.n})
    print("  stop brave-act: " + text[:200], flush=True)
    end = time.time() + 60
    while any(n in listed(c) for n in TOOLS) or brave_pids(c):
        need(time.time() < end, "brave-act's stop removes its tools and ends its Brave", "\n".join(brave_pids(c)))
        time.sleep(1)
    print("ok: brave-act's stop removed its tools and ended its Brave", flush=True)

    # 6. A Brave started without brave-act (the tile, the shell): no pipe. The actions say so; open_url hands over.
    b = Job(c, "/bin/sh", ["/boot/BITSOS2/bin/brave", f"{BASE}/second.html", "--", "--test-type", RULES])
    end = time.time() + 300
    while "AC3 second" not in ctl(c, "list"):
        need(time.time() < end, "a Brave started by the launcher shows second.html", b.poll()[-1500:])
        time.sleep(2)
    act_job = start_act(c)
    err, text, _ = act(c, "brave_tabs", {})
    need(err and "without brave-act" in text, "brave_tabs on a Brave without the pipe says why it cannot", text)
    err, text, _ = act(c, "brave_open_url", {"url": f"{BASE}/index.html"})
    need(not err and "handed_to" in text, "brave_open_url hands its URL to that Brave", text)
    end = time.time() + 120
    while "AC3 index" not in ctl(c, "list"):
        need(time.time() < end, "the handed-over URL shows in that Brave", ctl(c, "list"))
        time.sleep(2)
    print("ok: the handed-over URL shows in that Brave", flush=True)
    call(c, "stop", {"call": act_job.n})
    call(c, "stop", {"call": b.n})
    klog = open(os.path.join(RUN, "serial.log"), "rb").read().decode("utf-8", "replace")
    need("settingsd registered owner=brave keys=2" in klog, "settingsd took brave-act's two permission keys")
    ctl(c, "quit")


if __name__ == "__main__":
    main()
