#!/usr/bin/env python3
"""RB-9 scenario driver: system_list / system_pin / install_system over MCP under QEMU, resets through QMP.
Its own QEMU only (started by 55-system.sh).

Usage: system_scenario.py <mcp port> <token> <serial log> <qmp socket> <guest http base>

The stick starts with generations 1 and 2 (both OK, the same files), CONFIG boot_confirm_s=20, sys_keep=3.
  1. bitsboot starts 2 (NEWEST_OK). system_list: 2 RUNNING + NEXT START, 1 OK, kernel ids (asked again until
     both are read: equal, the same file), no pin. Refusals change nothing: n missing, n not a number, n=0,
     a generation that does not exist.
  2. system_pin n=1: PIN written; system_list: next start 1 (PINNED). QMP system_reset: bitsboot starts 1
     (PINNED); system_list: 1 RUNNING (PINNED).
  3. system_pin n=off now=true: PIN removed and a Reload into 2 read from the stick (kexec
     reload.generation_switched generation=2 reason=NEWEST_OK; the new kernel starts as generation 2, Reload 1,
     NEWEST_OK). system_list: 2 RUNNING after Reload 1.
  4. system_pin n=1 now=true: Reload into 1 (generation 1, Reload 2, PINNED). system_pin n=1 now=true again:
     1 runs already, no Reload. system_pin n=off: next start 2. QMP system_reset: bitsboot starts 2.
  5. install_system with an initramfs without settingsd and bitssnap: refused, nothing changed. install_system of
     the broken kernel (jmp $ at its entry): system generation 3 (NEW); system_pin 3 refused (not confirmed);
     system_pin off now=true refused (3's one try needs a firmware start). QMP system_reset: 3's one try
     (NEWEST_TRY), MCP silent; QMP system_reset: bitsboot falls back to 2 (FALLBACK_AFTER_TRY skipped=3);
     system_list: 3 NEW+TRY, 2 RUNNING.
  6. install_system of the good bundle with its own KERNEL (four/): generation 4 (3 replaced). QMP system_reset: 4's one try comes up and
     confirms itself after MCP answered over boot_confirm_s (OK). system_list: 4 OK RUNNING NEXT START,
     running "newest, confirmed (OK) in this boot" (RB-11, the words of reboot and hw_info).
  7. system_pin n=1 now=true: back to 1 (PINNED) at once; RB-11: reload of url/ (a kernel no generation has)
     from the pinned 1: system_list names its kernel id, no RUNNING tag, not "generation 1"; reload of four/
     (4's KERNEL): system_list names generation 4's kernel RUNNING by its kernel id while the record says 1;
     system_pin n=off now=true: 4 runs already (no Reload), PIN removed.
  After every Reload system_list is asked until the kernel ids are read (the running kernel is named by its id).
Each step prints one line; exit 0 = green."""
import http.client
import json
import re
import socket
import sys
import time

PORT, TOKEN, LOG, SOCK, BASE = int(sys.argv[1]), sys.argv[2], sys.argv[3], sys.argv[4], sys.argv[5]
next_id = 0
BANNER = "bitsboot: BitsOS V2 loader"


def serial():
    try:
        with open(LOG, "rb") as f:
            return f.read().decode("utf-8", "replace")
    except FileNotFoundError:
        return ""


def need(cond, what, detail=""):
    if not cond:
        raise SystemExit(f"RED: {what}\n{detail}\n--- serial tail ---\n{serial()[-4000:]}")
    print(f"ok: {what}", flush=True)


def rpc(c, method, params=None):
    global next_id
    next_id += 1
    c.request("POST", "/mcp", json.dumps({"jsonrpc": "2.0", "id": next_id, "method": method, "params": params or {}}),
              {"Authorization": f"Bearer {TOKEN}", "Content-Type": "application/json"})
    r = c.getresponse()
    data = r.read()
    j = json.loads(data) if data else None
    if r.status != 200 or not j or "result" not in j:
        raise ValueError(f"{method}: HTTP {r.status} {j}")
    return j["result"]


def hello(c):
    rpc(c, "initialize", {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "rb9-scenario", "version": "1"}})


def connect(wait_s):
    end = time.time() + wait_s
    while True:
        try:
            c = http.client.HTTPConnection("127.0.0.1", PORT, timeout=30)
            hello(c)
            return c
        except (OSError, http.client.HTTPException, ValueError) as e:
            if time.time() > end:
                need(False, f"MCP answers on 127.0.0.1:{PORT} within {wait_s} s", str(e))
            time.sleep(0.5)


def silent(seconds):
    end = time.time() + seconds
    while time.time() < end:
        try:
            hello(http.client.HTTPConnection("127.0.0.1", PORT, timeout=2))
            return False
        except (OSError, http.client.HTTPException, ValueError):
            time.sleep(0.5)
    return True


def call(c, name, args=None, timeout=300):
    """(is error, text); a JSON-RPC error (a bad argument) comes back as (True, its message)."""
    c.timeout = timeout
    if c.sock:
        c.sock.settimeout(timeout)
    try:
        r = rpc(c, "tools/call", {"name": name, "arguments": args or {}})
    except ValueError as e:
        return True, f"rpc error: {e}"
    return r.get("isError", False), "\n".join(i.get("text", "") for i in r["content"] if i["type"] == "text")


def wait_for(pattern, start, timeout_s, what):
    end = time.time() + timeout_s
    rx = re.compile(pattern)
    while True:
        text = serial()[start:]
        text = text[:text.rfind("\n") + 1]
        m = rx.search(text)
        if m:
            return m, start + m.end()
        if time.time() > end:
            need(False, f"{what} within {timeout_s} s", f"pattern: {pattern}")
        time.sleep(0.2)


def qmp(cmd):
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    s.settimeout(10)
    s.connect(SOCK)
    f = s.makefile("rw")
    f.readline()
    for c in ("qmp_capabilities", cmd):
        f.write(json.dumps({"execute": c}) + "\n")
        f.flush()
        while True:
            msg = json.loads(f.readline())
            if "return" in msg or "error" in msg:
                break
        if "error" in msg:
            need(False, f"QMP {c}", str(msg))
    s.close()


def reset(chosen, what):
    """QMP system_reset, then bitsboot's choice (a stall before bitsboot is reset again: CONTRACTS §1 flake)."""
    for attempt in range(1, 4):
        start = len(serial())
        qmp("system_reset")
        end = time.time() + 30
        while time.time() < end and BANNER not in serial()[start:]:
            time.sleep(0.2)
        if BANNER in serial()[start:]:
            _, pos = wait_for(chosen, start, 30, what)
            need(True, what)
            return pos
        print(f"note: reset {attempt}: firmware never reached bitsboot, resetting again", flush=True)
    need(False, f"{what}: bitsboot after a reset")


def gen_line(t, n):
    for line in t.splitlines():
        if line.strip().startswith(f"{n} "):
            return line
    return ""


def system_list(c):
    err, t = call(c, "system_list", timeout=60)
    need(not err, "system_list answers", t)
    return t


def list_until_ids(c, wait_s=120):
    """system_list once the kernel ids are read (after a Reload the running kernel is named by its id)."""
    end = time.time() + wait_s
    while True:
        t = system_list(c)
        if "(not read yet)" not in t or time.time() > end:
            return t
        time.sleep(2)


def pin(c, args, what, ok=True, want=()):
    err, t = call(c, "system_pin", args, timeout=180)
    need(err != ok and all(w in t for w in want), what, t)
    return t


def reload_into(c, args, number, reload, reason, what):
    pos = len(serial())
    t = pin(c, {**args, "now": True, "timeout_ms": 170000}, f"{what}: the answer", want=(f"Reloading into generation {number}",))
    c.close()
    wait_for(rf"kexec reload\.generation_switched reload={reload} generation={number} reason={reason}", pos, 120,
             f"{what}: the jump tells the new kernel it is generation {number} ({reason})")
    m, _ = wait_for(r"kernel start version=\S+ generation=(\d+) reload=(\d+) gen_reason=(\d+)", pos, 120, f"{what}: the new kernel starts")
    need((m.group(1), m.group(2)) == (str(number), str(reload)), f"{what}: it runs as generation {number} after Reload {reload}", m.group(0))
    c = connect(120)
    t = list_until_ids(c)
    need("RUNNING" in gen_line(t, number) and f"running: generation {number} (" in t and f"bitsboot: {reason}" in t and f"after Reload {reload}" in t,
         f"{what}: system_list names {number} running", t)
    return c, t


def main():
    wait_for(r"generation\.chosen number=2 reason=NEWEST_OK", 0, 120, "bitsboot starts generation 2 (NEWEST_OK)")
    c = connect(120)
    # 1. The listing; kernel ids come once read (the first listing may answer before).
    end = time.time() + 120
    while True:
        t = system_list(c)
        if "(not read yet)" not in t or time.time() > end:
            break
        time.sleep(2)
    l1, l2 = gen_line(t, 1), gen_line(t, 2)
    need("OK" in l1 and "OK" in l2 and "RUNNING, NEXT START" in l2 and "RUNNING" not in l1, "system_list: 2 running and next, 1 OK", t)
    need("installed 20" in l1 and "confirmed 20" in l1, "system_list shows when 1 was installed and confirmed", t)
    ids = [re.search(r"kernel id ([0-9a-f]{16})", x) for x in (l1, l2)]
    need(all(ids) and ids[0].group(1) == ids[1].group(1), "both kernel ids read and equal (the same KERNEL file)", t)
    need("pin: none" in t and "running: generation 2 (newest, confirmed (OK); bitsboot: NEWEST_OK)" in t, "system_list: no pin, running 2 (NEWEST_OK)", t)
    for args, want in (({}, "argument: n"), ({"n": "x"}, "generation number"), ({"n": 0}, "generation number"),
                       ({"n": 7}, "no generation 7"), ({"n": 1, "now": "yes"}, "now")):
        pin(c, args, f"system_pin {args} is refused", ok=False, want=(want,))
    need("pin: none" in system_list(c), "refusals wrote no PIN")

    # 2. Pin 1, reset: bitsboot starts 1.
    pin(c, {"n": 1}, "system_pin 1 writes the PIN", want=("PIN set to generation 1", "next start is generation 1 (PINNED", "next start (or add now=true"))
    t = system_list(c)
    need("NEXT START, PINNED" in gen_line(t, 1) and "pin: generation 1" in t, "system_list: next start 1 (PINNED)", t)
    c.close()
    reset(r"generation\.chosen number=1 reason=PINNED", "reset: bitsboot starts the pinned generation 1")
    c = connect(120)
    t = system_list(c)
    need("RUNNING" in gen_line(t, 1) and "running: generation 1 (pinned (SYS\\PIN); bitsboot: PINNED" in t, "system_list: 1 running (PINNED)", t)

    # 3./4. Reloads into a generation read from the stick.
    c, t = reload_into(c, {"n": "off"}, 2, 1, "NEWEST_OK", "system_pin off now")
    need("pin: none" in t, "the PIN is gone", t)
    c, t = reload_into(c, {"n": 1}, 1, 2, "PINNED", "system_pin 1 now")
    pin(c, {"n": 1, "now": True}, "system_pin 1 now while 1 runs: no Reload", want=("pinned already", "runs already: no Reload needed"))
    pin(c, {"n": "off"}, "system_pin off", want=("PIN removed (it named generation 1)", "next start is generation 2 (NEWEST_OK"))
    c.close()
    reset(r"generation\.chosen number=2 reason=NEWEST_OK", "reset: bitsboot starts 2 again")
    c = connect(120)

    # 5. install_system: refused without the services; the broken kernel falls back by itself.
    err, t = call(c, "install_system", {"url": f"{BASE}/noservices/"})
    need(err and "/bin/settingsd (missing)" in t and "nothing on the stick changed" in t, "install_system refuses an initramfs without the early services", t)
    err, t = call(c, "install_system", {"url": f"{BASE}/broken/"})
    need(not err and "installed system generation 3 (NEW)" in t and "init and the early services in the initramfs: /bin/init" in t,
         "install_system installs the broken kernel as system generation 3", t)
    pin(c, {"n": 3}, "system_pin 3 (NEW) is refused", ok=False, want=("not confirmed",))
    pin(c, {"n": "off", "now": True}, "system_pin off now is refused before 3's one try", ok=False, want=("one try", "nothing on the stick changed"))
    t = system_list(c)
    need("NEW" in gen_line(t, 3) and "NEXT START" in gen_line(t, 3), "system_list: 3 NEW is the next start", t)
    c.close()
    reset(r"generation\.chosen number=3 reason=NEWEST_TRY", "reset: bitsboot gives 3 its one try")
    need(silent(25), "the broken kernel never answers MCP (25 s)")
    reset(r"generation\.chosen number=2 reason=FALLBACK_AFTER_TRY skipped=3", "reset: bitsboot falls back to 2 by itself")
    c = connect(120)
    t = system_list(c)
    need("NEW+TRY" in gen_line(t, 3) and "RUNNING, NEXT START" in gen_line(t, 2) and "bitsboot: FALLBACK_AFTER_TRY, skipped 3)" in t and "running: generation 2 (fallback: the newest one was never confirmed;" in t,
         "system_list: 3 NEW+TRY, 2 running as the fallback", t)

    # 6. The good bundle as 4: comes up and confirms itself.
    err, t = call(c, "install_system", {"url": f"{BASE}/four/"})
    need(not err and "installed system generation 4 (NEW)" in t and "unconfirmed generation 3 was removed" in t, "install_system installs the good bundle as 4", t)
    c.close()
    pos = reset(r"generation\.chosen number=4 reason=NEWEST_TRY", "reset: 4's one try")
    c = connect(120)
    end = time.time() + 120
    while "kexec boot.confirmed generation=4" not in serial()[pos:]:
        if time.time() > end:
            need(False, "generation 4 confirms itself within 120 s of MCP answers")
        call(c, "reload_info")
        time.sleep(2)
    need(True, "generation 4 came up and confirmed itself (OK)")
    t = system_list(c)
    need(gen_line(t, 4).split()[1] == "OK" and "RUNNING, NEXT START" in gen_line(t, 4) and gen_line(t, 3) == "", "system_list: 4 OK running and next, 3 gone", t)
    # RB-11 (desk-minipc-w13 FAIL 5): named as reboot and hw_info name it.
    need("running: generation 4 (newest, confirmed (OK) in this boot; bitsboot: NEWEST_TRY)" in t, "system_list: 4 confirmed in this boot", t)

    # 7. Back to 1 and to the newest at once.
    c, _ = reload_into(c, {"n": 1}, 1, 1, "PINNED", "system_pin 1 now from 4")
    # RB-11: a URL Reload from the pinned 1 into a kernel no generation has: the record still says 1, the listing
    # does not call it 1.
    pos = len(serial())
    err, t = call(c, "reload", {"url": f"{BASE}/url/", "timeout_ms": 120000})
    need(not err and "Reload counter -> 2" in t, "reload of another kernel from a URL: answered", t)
    c.close()
    m, _ = wait_for(r"crash kernel_id\.handed kernel_id=0x([0-9a-f]+) reload=2\b", pos, 120, "the URL kernel knows its kernel id")
    c = connect(120)
    t = list_until_ids(c)
    want = f"running: a kernel from a URL (kernel id {int(m.group(1), 16):016x}, after Reload 2): no generation on the stick has this KERNEL; the GENERATION record says generation 1 (pinned"
    need("RUNNING" not in t and want in t, "system_list: the URL kernel is not called generation 1", t)
    # RB-11 (fixer): a URL Reload of 4's KERNEL from there: named generation 4's kernel by its id, the record says 1.
    pos = len(serial())
    err, t = call(c, "reload", {"url": f"{BASE}/four/", "timeout_ms": 120000})
    need(not err and "Reload counter -> 3" in t, "reload of 4's kernel from a URL: answered", t)
    c.close()
    m, _ = wait_for(r"crash kernel_id\.handed kernel_id=0x([0-9a-f]+) reload=3\b", pos, 120, "the URL kernel knows its kernel id")
    c = connect(120)
    t = list_until_ids(c)
    k4 = re.search(r"kernel id ([0-9a-f]{16})", gen_line(t, 4))
    need(k4 is not None and int(k4.group(1), 16) == int(m.group(1), 16), "the handed id is 4's kernel id", t)
    want = (f"running: generation 4's kernel (from a Reload: kernel id {k4.group(1)} = generation 4's KERNEL, after Reload 3); "
            "the GENERATION record says generation 1 (pinned")
    need("RUNNING" in gen_line(t, 4) and "RUNNING" not in gen_line(t, 1) and want in t, "system_list: 4's kernel runs, not generation 1", t)
    # system_pin now follows the same identity: 4 runs already, so off needs no Reload.
    pin(c, {"n": "off", "now": True}, "system_pin off now while 4's kernel runs: no Reload",
        want=("PIN removed (it named generation 1)", "Generation 4 runs already: no Reload needed"))
    t = system_list(c)
    need("pin: none" in t and "NEXT START" in gen_line(t, 4) and "RUNNING" in gen_line(t, 4), "no pin, 4 runs and is the next start", t)
    c.close()
    print("system: green", flush=True)


main()
