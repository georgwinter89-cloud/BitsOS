#!/usr/bin/env python3
"""CL-1 on V2 through MCP: settingsd (static musl, sent with write_file) serves on V2, the client sets, applies
and undoes every key, R8 switches are refused, kernel keys land in \\BITSOS2\\CONFIG.TXT with the guarded lines
untouched, values survive a kill -9 and init's restart, and without a stick it runs on the fallback. Since INT-W3
settingsd is in the image and init (pid 1, uid 0) starts it; the client runs as uid 1000 (MCP run).

Usage: settings_qemu_check.py <mcp host port> <token> <settingsd binary> <run dir>
"""
import base64
import http.client
import json
import re
import sys
import time

PORT, TOKEN, BIN, RUN_DIR = int(sys.argv[1]), sys.argv[2], sys.argv[3], sys.argv[4]
next_id = 0
SD = "/bin/settingsd"  # in the image since INT-W3; init (pid 1) starts and restarts it
SOCK = "/run/bitsos/settings.sock"


def need(cond, what, detail=""):
    if not cond:
        raise SystemExit(f"RED: {what}{': ' + str(detail)[:600] if detail else ''}")
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
            c = http.client.HTTPConnection("127.0.0.1", PORT, timeout=120)
            rpc(c, "initialize", {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "cl1", "version": "1"}})
            return c
        except (OSError, http.client.HTTPException, ValueError) as e:
            if time.time() > end:
                raise SystemExit(f"RED: no MCP answer on 127.0.0.1:{PORT}: {e}")
            time.sleep(1)


def call(c, name, args=None):
    j = rpc(c, "tools/call", {"name": name, "arguments": args or {}})
    if "error" in j:
        return True, f"rpc error {j['error']}"
    r = j["result"]
    return r["isError"], "\n".join(i.get("text", "") for i in r["content"] if i["type"] == "text")


def run(c, args, timeout_ms=20000):
    err, text = call(c, "run", {"path": SD, "args": args, "timeout_ms": timeout_ms})
    return err, text


def stdout_of(text):
    """The program's stdout from a run answer (the JSON line settings --json printed)."""
    for line in text.splitlines():
        line = line.strip()
        if line.startswith("{") and '"ok"' in line:
            return json.loads(line)
    return None


def ctl(c, *args, sock=SOCK):
    err, text = run(c, ["--socket", sock, "--json", "--source", "qemu-check", *args])
    j = stdout_of(text)
    if j is None:
        raise SystemExit(f"RED: no answer from settings {' '.join(args)}: {text[:800]}")
    return j


def sample(d):
    s, default = d["schema"], d["default"]
    t = s["type"]
    if t == "bool":
        return not default
    if t == "int":
        return s["min"] if default == s["max"] else s["max"]
    if t == "choice":
        return next(x for x in s["choices"] if x != default)
    if t == "color":
        return "#123abc"
    if t == "text":
        return "Test Font"
    if t == "logspec":
        return "warn,net=debug"
    raise SystemExit(f"RED: no sample for {d['key']}")


def read_file(c, path):
    err, text = call(c, "read_file", {"path": path})
    if err:
        return None
    j = json.loads(text)
    return j.get("text") if "text" in j else base64.b64decode(j.get("data", "")).decode("utf-8", "replace")


def guarded(text):
    return [l for l in (text or "").splitlines() if re.match(r"\s*(nvme_write|stick_write|usb_msd_write)\s*=", l, re.I)]


def start(c, extra=()):
    err, text = run(c, ["serve", *extra], timeout_ms=1500)
    m = re.search(r"\(call (\d+)\)", text)
    need(m is not None, f"settingsd {' '.join(extra) or ''} keeps running", text)
    return int(m.group(1))


def daemon_pid(c):
    err, text = call(c, "ps", {})
    for line in text.splitlines():
        if f"{SD} serve" in line and "--socket" not in line:
            m = re.search(r"\d+", line)
            if m:
                return int(m.group(0))
    raise SystemExit(f"RED: no settingsd in ps: {text[:1500]}")


def wait_daemon(c):
    end = time.time() + 60
    while True:
        err, text = run(c, ["--socket", SOCK, "--json", "status"])
        j = stdout_of(text)
        if j is not None:
            return j
        if time.time() > end:
            raise SystemExit(f"RED: settingsd does not answer on {SOCK}: {text[:1500]}")
        time.sleep(1)


def wait_stick(c):
    """init's settingsd may start while the stick is still read-only (FS-7's verdict): its resync moves it there."""
    end = time.time() + 90
    while True:
        st = wait_daemon(c)
        if st.get("place") == "stick" or time.time() > end:
            return st
        time.sleep(2)


def main():
    c = connect(240)
    config_before = read_file(c, "/boot/BITSOS2/CONFIG.TXT")
    need(config_before is not None, "CONFIG.TXT readable")

    first = daemon_pid(c)
    print(f"ok: init started settingsd (pid {first})")
    st = wait_stick(c)
    need(st.get("place") == "stick", "settingsd stores on the stick (\\BITSOS2\\settings)", st)

    keys = ctl(c, "list")["keys"]
    need(len(keys) >= 25, f"{len(keys)} keys listed")
    for k in keys:
        d = ctl(c, "describe", k["key"])["key"]
        v = sample(d)
        a = ctl(c, "set", d["key"], v if isinstance(v, str) else json.dumps(v))
        want = v if not isinstance(v, bool) else v
        if not (a.get("ok") and a.get("new") == want and a.get("stored") == "stick"):
            raise SystemExit(f"RED: set {d['key']}={v}: {a}")
        g = ctl(c, "get", d["key"])
        if g.get("value") != want:
            raise SystemExit(f"RED: get {d['key']} after set: {g}")
        if d["applier"]["by"] == "kernel":
            cfg = read_file(c, "/boot/BITSOS2/CONFIG.TXT")
            line = f"{d['applier']['config']}={v}"
            if line not in (cfg or "").splitlines():
                raise SystemExit(f"RED: {line} not in CONFIG.TXT: {cfg}")
        u = ctl(c, "undo")
        g = ctl(c, "get", d["key"])
        if not u.get("ok") or g.get("value") != d["default"]:
            raise SystemExit(f"RED: undo {d['key']}: {u} / {g}")
    print(f"ok: set/apply/undo for all {len(keys)} keys")
    config_after = read_file(c, "/boot/BITSOS2/CONFIG.TXT")
    need(guarded(config_after) == guarded(config_before), "guarded CONFIG.TXT lines byte-identical", (config_before, config_after))

    for k in ["nvme_write", "stick_write", "usb_msd_write", "kernel.nvme_write"]:
        a = ctl(c, "set", k, "on")
        if a.get("error") != "refused_r8":
            raise SystemExit(f"RED: {k} not refused: {a}")
    print("ok: R8 switches refused")

    a = ctl(c, "set", "theme.accent", "#ff00aa")
    need(a.get("ok"), "theme.accent set for the restart", a)
    pid = daemon_pid(c)
    err, text = call(c, "kill", {"pid": pid, "signal": "SIGKILL"})
    need(not err, f"settingsd (pid {pid}) killed with SIGKILL", text)
    end = time.time() + 60
    while True:
        try:
            again = daemon_pid(c)
        except SystemExit:
            again = pid
        if again != pid:
            break
        if time.time() > end:
            raise SystemExit("RED: init did not restart settingsd within 60 s")
        time.sleep(1)
    print(f"ok: init restarted settingsd (pid {again})")
    wait_daemon(c)
    need(ctl(c, "get", "theme.accent").get("value") == "#ff00aa", "value survives a restart")
    h = ctl(c, "history", "500")
    need(len(h["entries"]) >= 2 * len(keys) + 1, f"journal kept {len(h['entries'])} entries")
    theme = read_file(c, "/boot/BITSOS2/settings/theme.toml")
    need(theme is not None and 'theme.accent = "#ff00aa"' in theme, "theme.toml on the stick", theme)

    fb = start(c, ["--stick", "/nonexistent", "--fallback", "/tmp/clfb/fb", "--socket", "/tmp/clfb/fb.sock"])
    st = ctl(c, "status", sock="/tmp/clfb/fb.sock")
    need(st.get("place") == "fallback", "without a stick: fallback", st)
    a = ctl(c, "set", "theme.radius", "3", sock="/tmp/clfb/fb.sock")
    need(a.get("stored") == "fallback", "a change on the fallback is kept there", a)
    need("theme.radius" in (read_file(c, "/tmp/clfb/fb/PENDING") or ""), "PENDING lists it for the resync")
    call(c, "run", {"stop": {"call": fb}})
    print("settingsd on V2: green")


if __name__ == "__main__":
    main()
