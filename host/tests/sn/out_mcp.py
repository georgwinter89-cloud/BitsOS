#!/usr/bin/env python3
"""SN-10 scenario client: over MCP (QEMU hostfwd) the tool `snd` and /sys/bitsos/snd_output through busybox sh
(uid 1000: reads; a write is EACCES).
QEMU has no display engine: hdmi and hdmi-off must be refused with the target unchanged.

Usage: out_mcp.py <host port> <token>. Exit status 0 = green."""
import http.client
import json
import sys
import time

PORT, TOKEN = int(sys.argv[1]), sys.argv[2]
next_id = 0
fail = False


def rpc(c, method, params=None):
    global next_id
    next_id += 1
    c.request("POST", "/mcp", json.dumps({"jsonrpc": "2.0", "id": next_id, "method": method, "params": params or {}}),
              {"Authorization": f"Bearer {TOKEN}", "Content-Type": "application/json"})
    r = c.getresponse()
    j = json.loads(r.read() or b"null")
    if r.status != 200 or not j:
        raise ValueError(f"{method}: HTTP {r.status} {j}")
    return j


def connect(wait_s):
    end = time.time() + wait_s
    while True:
        try:
            c = http.client.HTTPConnection("127.0.0.1", PORT, timeout=120)
            rpc(c, "initialize", {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "sn10-output", "version": "1"}})
            return c
        except (OSError, http.client.HTTPException, ValueError) as e:
            if time.time() > end:
                raise SystemExit(f"RED: no MCP answer on 127.0.0.1:{PORT}: {e}")
            time.sleep(1)


def call(c, name, args):
    """(kind, text): kind is "ok", "error" (isError) or "rpc" (a JSON-RPC error such as -32602)."""
    j = rpc(c, "tools/call", {"name": name, "arguments": args})
    if "error" in j:
        return "rpc", f"{j['error'].get('code')} {j['error'].get('message', '')}"
    r = j["result"]
    return ("error" if r.get("isError") else "ok"), "\n".join(i.get("text", "") for i in r["content"] if i["type"] == "text")


def need(cond, what, detail=""):
    global fail
    if cond:
        print(f"ok   {what}")
    else:
        print(f"FAIL {what}\n{detail}")
        fail = True


def sh(c, script, timeout_ms=20000):
    """(status line, stdout, stderr) of /bin/sh -c script."""
    kind, t = call(c, "run", {"path": "/bin/sh", "args": ["-c", script], "timeout_ms": timeout_ms})
    if kind != "ok" or "\n--- stdout (" not in t:
        return t, "", ""
    head, rest = t.split("\n--- stdout (", 1)
    out, err = rest.split("\n--- stderr (", 1)
    body = lambda part: part.split(") ---", 1)[1].removeprefix("\n")
    return head.splitlines()[0], body(out), body(err)


def status(c):
    kind, t = call(c, "snd", {})
    if kind != "ok":
        raise SystemExit(f"RED: snd status: {kind} {t}")
    return t


def target(c):
    return status(c).splitlines()[0].split()[0]


def main():
    c = connect(150)
    t = status(c)
    print(f"--- snd status at boot\n{t}")
    need(t.startswith("target=auto "), "boot: target auto", t)
    need("\nhdmi display=absent" in t, "boot: no HDMI in QEMU", t)
    need("\noutput hda0/jack/0:0x3 intel " in t, "boot: the hda jack listed", t)
    need(any(l.startswith("output usb:xhci") and "/usb/0:0x1 usb channels=2" in l for l in t.splitlines()), "boot: the USB headset listed", t)

    st, out, err = sh(c, "cat /sys/bitsos/snd_output")
    need(st.endswith("exit 0") and out.splitlines() == ["auto"], "cat /sys/bitsos/snd_output: auto (one value)", f"{st}\n{out}{err}")
    st, out, err = sh(c, "cat /sys/bitsos/snd_output_status")
    need(st.endswith("exit 0") and out.startswith("mixer=") and " hdmi=absent" in out, "cat /sys/bitsos/snd_output_status", f"{st}\n{out}{err}")
    # MCP's run programs are uid 1000: the file is root's to write (Georg's root console; sn.out.sysfs covers
    # root's writes through the same file calls), so over MCP the tool switches.
    st, out, err = sh(c, "id -u; echo jack > /sys/bitsos/snd_output")
    need(out.strip() == "1000" and not st.endswith("exit 0") and "Permission denied" in err, "echo jack as uid 1000: EACCES", f"{st}\n{out}{err}")
    kind, t = call(c, "snd", {"output": "jack"})
    need(kind == "ok" and t.startswith("snd output jack\ntarget=jack "), "snd output jack", f"{kind} {t}")
    st, out, err = sh(c, "head -1 /sys/bitsos/snd_output")
    need(st.endswith("exit 0") and out.strip() == "jack", "the file says jack", f"{st}\n{out}{err}")
    for v in ("hdmi", "hdmi-off", "hdmi off"):
        kind, t = call(c, "snd", {"output": v})
        need(kind == "error" and "HDMI audio: " in t, f"snd output {v}: refused", f"{kind} {t}")
    kind, t = call(c, "snd", {"output": "nope"})
    need(kind == "rpc" and t.startswith("-32602"), "snd output nope: bad argument", f"{kind} {t}")
    need(target(c) == "target=jack", "the target stays jack after the MCP refusals", status(c))

    # A client playing on the jack, then moved to the headset while it plays.
    kind, t = call(c, "run", {"path": "/bin/alsa-hw", "args": ["cobalt", "8"], "timeout_ms": 2000})
    need(kind == "error" and "Still running" in t and "(call " in t, "alsa-hw cobalt 8 runs on", f"{kind} {t}")
    t = status(c)
    need("playing=hda0/jack/0:0x3 " in t and "\nclient " in t and " running " in t, "snd: a running client plays on the jack", t)
    st, out, err = sh(c, "cat /sys/bitsos/snd_output_status")
    need("playing=hda0/jack/0:0x3 clients=1 " in out, "the status file: playing on the jack", f"{st}\n{out}{err}")
    kind, t = call(c, "snd", {"output": "usb"})
    need(kind == "ok" and t.startswith("snd output usb\ntarget=usb "), "snd output usb", f"{kind} {t}")
    end = time.time() + 20
    while time.time() < end and "playing=usb:" not in status(c):
        time.sleep(0.5)
    t = status(c)
    print(f"--- snd status while playing on usb\n{t}")
    need("playing=usb:xhci" in t and any(l.startswith("output usb:") and l.endswith(" *") for l in t.splitlines()), "snd: now on the headset (marked *)", t)
    end = time.time() + 40
    while time.time() < end and "\nclient " in status(c):
        time.sleep(1)
    need("\nclient " not in status(c), "alsa-hw ended", status(c))
    kind, t = call(c, "snd", {"output": "auto"})
    need(kind == "ok" and "\ntarget=auto " in t, "snd output auto (back)", f"{kind} {t}")
    st, out, err = sh(c, "head -1 /sys/bitsos/snd_output")
    need(out.strip() == "auto", "the file says auto again", f"{st}\n{out}{err}")
    return 1 if fail else 0


sys.exit(main())
