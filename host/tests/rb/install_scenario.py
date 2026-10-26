#!/usr/bin/env python3
"""RB-6 scenario driver: install_kernel over MCP under QEMU, machine resets through QMP, bitsboot's fallback and
the kernel's own boot confirmation. Its own QEMU only (started by 50-install.sh).

Usage:
  install_scenario.py cycle <mcp port> <token> <serial log> <qmp socket> <guest http base> <bundle dir>
  install_scenario.py refused <mcp port> <token> <serial log> <guest http base>

cycle (the stick starts with generation 1 = OK, CONFIG boot_confirm_s=20, sys_keep=1, panic=reboot, stick_write=off):
  1. MCP up; bitsboot started generation 1 (NEWEST_OK); reload_info: nothing to confirm.
  2. Refusals that leave the stick alone (SYS lists only 1, CONFIG.TXT byte for byte as before): CONFIG.TXT
     texts that flip stick_write, evade with a later stick_write=on line, add nvme_write=on or drop the token; a junk kernel;
     an initramfs without its trailer; downloads that fail (404, a body cut short against its Content-Length, a
     Content-Length over every limit, an initramfs 404 after a good kernel; install.fetch_failed logged); bad
     URLs and arguments (public address, https, a kernel file without initrd, wrong types, no url); a second
     install_kernel and a Reload while one install waits for its (slow) download: refused as busy. A dry run.
  3. install_kernel <broken>/ (the image's kernel with `jmp $` at its entry: valid for every check, hangs before
     MCP): generation 2 (NEW), read back equal. SYS/2 holds KERNEL INITRD NEW.
  4. QMP system_reset: bitsboot starts 2 as its one try (NEWEST_TRY, TRY written); MCP stays silent (the kernel
     hangs). QMP system_reset again: bitsboot falls back to 1 by itself (FALLBACK_AFTER_TRY skipped=2), MCP back.
  5. install_kernel <panic>/ (the good kernel, its initramfs with /etc/bitsos/test-panic-before-mcp): generation
     3, the unconfirmed 2 removed. QMP system_reset: 3 starts as its one try, its kernel panics before MCP
     (kexec boot.test_panic), panic=reboot resets the machine by itself, bitsboot falls back to 1
     (FALLBACK_AFTER_TRY skipped=3) without any help.
  6. install_kernel <good>/ (the image's own kernel): generation 4 (NEW), the unconfirmed 3 removed.
  7. QMP system_reset: 4 starts as its one try and waits for its proof; a Reload (the answer warns that 4 stays
     unconfirmed) brings a kernel that has nothing to confirm (Reload 1): MCP answers for the proof time and 15 s
     more, nothing is confirmed, SYS/4 keeps NEW and TRY. QMP system_reset: bitsboot falls back to 1
     (FALLBACK_AFTER_TRY skipped=4).
  8. install_kernel <good>/ with a CONFIG.TXT that adds a comment line: generation 5, 4 removed, CONFIG.TXT
     written.
  9. QMP system_reset: 5 starts as its one try; MCP answers (every 2 s); the kernel logs boot.confirm_pending,
     and after MCP has answered over boot_confirm_s it writes OK (kexec boot.confirmed generation=5 pruned=[1]:
     sys_keep=1). reload_info says confirmed. SYS lists only 5, with KERNEL INITRD OK. RB-10: a Reload then hands
    5 on as NEWEST_OK (its OK marker read at the jump): reboot and hw_info of the new kernel say confirmed.
  10. QMP system_reset: bitsboot starts 5 as NEWEST_OK; reload_info: nothing to confirm.
refused (a clone of the boot stick is attached: the stick is read-only): install_kernel is refused before it
fetches anything, naming the read-only stick.
Each step prints one line; exit 0 = green."""
import base64
import http.client
import json
import threading
import re
import socket
import sys
import time

MODE = sys.argv[1]
PORT, TOKEN, LOG = int(sys.argv[2]), sys.argv[3], sys.argv[4]
next_id = 0


def serial():
    try:
        with open(LOG, "rb") as f:
            return f.read().decode("utf-8", "replace")
    except FileNotFoundError:
        return ""  # QEMU has not opened it yet


def need(cond, what, detail=""):
    if not cond:
        tail = serial()[-4000:]
        raise SystemExit(f"RED: {what}\n{detail}\n--- serial tail ---\n{tail}")
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


def connect(wait_s):
    end = time.time() + wait_s
    while True:
        try:
            c = http.client.HTTPConnection("127.0.0.1", PORT, timeout=30)
            rpc(c, "initialize", {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "rb6-scenario", "version": "1"}})
            return c
        except (OSError, http.client.HTTPException, ValueError) as e:
            if time.time() > end:
                need(False, f"MCP answers on 127.0.0.1:{PORT} within {wait_s} s", str(e))
            time.sleep(0.5)


def silent(seconds):
    """True when MCP does not answer for `seconds`."""
    end = time.time() + seconds
    while time.time() < end:
        try:
            c = http.client.HTTPConnection("127.0.0.1", PORT, timeout=2)
            rpc(c, "initialize", {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "rb6-scenario", "version": "1"}})
            return False
        except (OSError, http.client.HTTPException, ValueError):
            time.sleep(0.5)
    return True


def call(c, name, args=None, timeout=300):
    c.timeout = timeout
    if c.sock:
        c.sock.settimeout(timeout)
    r = rpc(c, "tools/call", {"name": name, "arguments": args or {}})
    return r.get("isError", False), "\n".join(i.get("text", "") for i in r["content"] if i["type"] == "text")


def call_any(c, name, args=None, timeout=300):
    """Like call, but a JSON-RPC error (a bad argument) comes back as (True, its message) too."""
    try:
        return call(c, name, args, timeout)
    except ValueError as e:
        return True, f"rpc error: {e}"


def read_text(c, path):
    err, t = call(c, "read_file", {"path": path})
    need(not err, f"read_file {path}", t)
    return base64.b64decode(json.loads(t)["data"]).decode("utf-8", "replace")


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


def qmp(sock_path, cmd):
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    s.settimeout(10)
    s.connect(sock_path)
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


BANNER = "bitsboot: BitsOS V2 loader"


def reset(sock_path, chosen, what):
    """QMP system_reset, then bitsboot's choice. OVMF sometimes stalls before bitsboot (CONTRACTS §1: the
    usb-storage flake): no banner within 30 s -> reset again (bitsboot never ran, so nothing on the stick
    changed)."""
    for attempt in range(1, 4):
        start = len(serial())
        qmp(sock_path, "system_reset")
        end = time.time() + 30
        while time.time() < end and BANNER not in serial()[start:]:
            time.sleep(0.2)
        if BANNER in serial()[start:]:
            m, pos = wait_for(chosen, start, 30, what)
            need(True, what)
            reset.start = start
            return pos
        print(f"note: reset {attempt}: firmware never reached bitsboot, resetting again", flush=True)
    need(False, f"{what}: bitsboot after a reset")


def listing(c, path):
    err, t = call(c, "list_dir", {"path": path})
    need(not err, f"list_dir {path}", t)
    names = []
    for line in t.splitlines()[1:]:
        parts = line.split()
        if parts:
            names.append(parts[-1].rstrip("/"))
    return sorted(names)


def cycle():
    sock_path, base, bundles = sys.argv[5], sys.argv[6], sys.argv[7]
    m, pos = wait_for(r"generation\.chosen number=1 reason=NEWEST_OK", 0, 120, "bitsboot starts generation 1 (OK)")
    need(True, "bitsboot starts generation 1 (NEWEST_OK)")
    c = connect(120)
    err, t = call(c, "reload_info")
    need(not err and "nothing to confirm" in t, "reload_info: nothing to confirm on a confirmed generation", t)
    need("boot stick: read-write; generations 1 (OK); next boot starts generation 1 (NEWEST_OK" in t, "reload_info names the stick's generations and the next boot", t)
    need(listing(c, "/boot/BITSOS2/SYS") == ["1"], "SYS holds only generation 1")

    # 2. Refusals and a dry run: nothing changes.
    cfg0 = read_text(c, "/boot/BITSOS2/CONFIG.TXT")
    # INT-W9 (WL-4 CR 4): read_file shows the token as ***; install_kernel puts the stick's value back for it (step 8).
    need("stick_write=off" in cfg0 and "token=***" in cfg0 and TOKEN not in cfg0,
         "the stick's CONFIG.TXT has stick_write=off and the token, shown as ***")
    keep = "".join(l + "\n" for l in cfg0.splitlines() if not l.startswith("token="))
    for what, cfg, key in (
        ("flips stick_write=off to stick_write=on", f"token={TOKEN}\nstick_write=on\n", "stick_write"),
        ("evades with a later stick_write=on line (last wins)", f"token={TOKEN}\nstick_write=off\nstick_write=on\n", "stick_write"),
        ("adds nvme_write=on", f"token={TOKEN}\nstick_write=off\nnvme_write=on\n", "nvme_write"),
        ("drops the token", keep, "token"),
    ):
        err, t = call(c, "install_kernel", {"url": f"{base}/good/", "config": cfg})
        need(err and key in t and "nothing on the stick changed" in t, f"a CONFIG.TXT that {what} is refused", t)
    err, t = call(c, "install_kernel", {"url": f"{base}/junk/"})
    need(err and "KERNEL" in t and "nothing on the stick changed" in t, "a junk kernel is refused", t)
    err, t = call(c, "install_kernel", {"url": f"{base}/good/KERNEL", "initrd": f"{base}/cut/INITRD"})
    need(err and "INITRD" in t, "an initramfs cut short is refused", t)
    # Bad downloads and bad arguments: an error naming the cause, nothing on the stick changed.
    for what, args, want in (
        ("a 404", {"url": f"{base}/missing/"}, "404"),
        ("a body cut short against its Content-Length", {"url": f"{base}/cutbody/"}, "kernel"),
        ("a Content-Length over every limit", {"url": f"{base}/huge/"}, "too big"),
        ("an initramfs URL that answers 404", {"url": f"{base}/good/KERNEL", "initrd": f"{base}/missing/INITRD"}, "404"),
    ):
        err, t = call(c, "install_kernel", args)
        need(err and want in t and "nothing on the stick changed" in t, f"install_kernel with {what} fails cleanly", t)
    for what, args, want in (
        ("a public address", {"url": "http://8.8.8.8/x/"}, "not on a private network"),
        ("https", {"url": "https://10.0.2.2/x/"}, "https"),
        ("a kernel URL without initrd", {"url": f"{base}/good/KERNEL"}, "both files"),
        ("dry_run not a boolean", {"url": f"{base}/good/", "dry_run": "yes"}, "dry_run"),
        ("config not a string", {"url": f"{base}/good/", "config": 5}, "config"),
        ("no url", {}, "url"),
    ):
        err, t = call_any(c, "install_kernel", args)
        need(err and want in t, f"install_kernel with {what} is refused", t)
    # Busy: while one install waits for its download, a second install and a Reload are refused.
    slow = {}

    def slow_install():
        c2 = connect(30)
        slow["r"] = call(c2, "install_kernel", {"url": f"{base}/slow/", "dry_run": True})
        c2.close()

    th = threading.Thread(target=slow_install)
    th.start()
    time.sleep(3)
    err, t = call(c, "install_kernel", {"url": f"{base}/good/"})
    need(err and "in progress" in t and "nothing on the stick changed" in t, "a second install_kernel while one runs is refused", t)
    err, t = call(c, "reload", {"url": f"{base}/good/", "dry_run": True})
    need(err and "in progress" in t, "a Reload while install_kernel runs is refused", t)
    th.join(60)
    need(not th.is_alive() and not slow["r"][0] and "dry run" in slow["r"][1], "the slow install (dry run) finished", str(slow.get("r")))
    err, t = call(c, "install_kernel", {"url": f"{base}/broken/", "dry_run": True})
    need(not err and "dry run" in t and "generation 2" in t, "a dry run plans generation 2", t)
    need(listing(c, "/boot/BITSOS2/SYS") == ["1"], "after refusals and the dry run SYS still holds only 1")
    need(read_text(c, "/boot/BITSOS2/CONFIG.TXT") == cfg0, "after the refusals CONFIG.TXT is unchanged")
    wait_for(r"kexec install\.fetch_failed", 0, 5, "a failed download is logged (install.fetch_failed)")

    # 3. The broken kernel.
    err, t = call(c, "install_kernel", {"url": f"{base}/broken/"})
    need(not err and "installed generation 2 (NEW)" in t and "read back from the stick and equal" in t
         and "fallback (KERNEL.OLD): generation 1" in t and "generation 2 (NEWEST_TRY" in t, "the broken kernel installed as generation 2", t)
    need(listing(c, "/boot/BITSOS2/SYS/2") == ["INITRD", "KERNEL", "NEW"], "SYS/2 holds KERNEL INITRD NEW")
    err, t = call(c, "reload_info")
    need("generations 1 (OK), 2 (NEW); next boot starts generation 2 (NEWEST_TRY" in t, "reload_info: the next boot gives 2 its one try", t)
    c.close()

    # 4. Its one try hangs; the next boot falls back by itself.
    pos = reset(sock_path, r"generation\.chosen number=2 reason=NEWEST_TRY", "bitsboot starts generation 2 as its one try")
    wait_for(r"marker\.written[^\n]*SYS\\2\\TRY", reset.start, 5, "bitsboot writes SYS\\2\\TRY")
    need(True, "bitsboot wrote SYS\\2\\TRY")
    need(silent(25), "the broken kernel never answers MCP (25 s)")
    pos = reset(sock_path, r"generation\.chosen number=1 reason=FALLBACK_AFTER_TRY skipped=2", "bitsboot falls back to generation 1 by itself")
    c = connect(120)
    wait_for(r"kexec boot\.confirm_not_needed", pos, 60, "the fallback kernel has nothing to confirm")
    need(listing(c, "/boot/BITSOS2/SYS/2") == ["INITRD", "KERNEL", "NEW", "TRY"], "SYS/2 now holds NEW and TRY")

    # 5. A generation whose kernel panics before MCP (its initramfs asks for it): with panic=reboot the machine
    #    resets by itself and bitsboot falls back without any help.
    err, t = call(c, "install_kernel", {"url": f"{base}/panic/"})
    need(not err and "installed generation 3 (NEW)" in t and "unconfirmed generation 2 was removed" in t, "the panicking bundle installed as 3, 2 replaced", t)
    c.close()
    pos = reset(sock_path, r"generation\.chosen number=3 reason=NEWEST_TRY", "bitsboot starts generation 3 as its one try")
    # The panic handler's own line (always printed); the event before it may be cut by the panic's serial budget.
    m, pos = wait_for(r"(?m)^RB-6 fallback test: the initramfs holds", pos, 60, "the kernel of 3 panics before MCP")
    need(True, "the kernel of 3 panics before MCP")
    panic_at = m.start() + (pos - m.end())
    end = time.time() + 90
    while not re.search(r"generation\.chosen number=1 reason=FALLBACK_AFTER_TRY skipped=3", serial()[panic_at:]):
        if time.time() > end:
            if BANNER not in serial()[panic_at:]:
                print("note: no new boot after the panic within 90 s (firmware stall?): resetting through QMP", flush=True)
                pos = reset(sock_path, r"generation\.chosen number=1 reason=FALLBACK_AFTER_TRY skipped=3", "bitsboot falls back to 1")
                break
            need(False, "bitsboot falls back to generation 1 after the panic")
        time.sleep(0.5)
    else:
        need(True, "the machine reset itself after the panic and bitsboot fell back to generation 1 (FALLBACK_AFTER_TRY skipped=3)")
    c = connect(120)

    # 6. The good kernel replaces the unconfirmed 3 as 4.
    err, t = call(c, "install_kernel", {"url": f"{base}/good/"})
    need(not err and "installed generation 4 (NEW)" in t and "unconfirmed generation 3 was removed" in t, "the good kernel installed as 4, 3 replaced", t)
    need(listing(c, "/boot/BITSOS2/SYS") == ["1", "4"], "SYS holds 1 and 4")
    c.close()

    # 7. 4's one try comes up, but a Reload replaces its kernel before the proof: a kernel brought by a Reload
    #    never confirms a generation, so TRY and NEW stay and the next boot falls back.
    pos = reset(sock_path, r"generation\.chosen number=4 reason=NEWEST_TRY", "bitsboot starts generation 4 as its one try")
    c = connect(120)
    wait_for(r"kexec boot\.confirm_pending generation=4", pos, 60, "the kernel of 4 waits for its confirmation")
    err, t = call(c, "reload", {"url": f"{base}/good/", "timeout_ms": 60000})
    need(not err and "not confirmed yet" in t and "generation 4" in t, "the reload answer warns that 4 stays unconfirmed", t)
    c.close()
    m, pos = wait_for(r"kernel start version=\S+ generation=\d+ reload=1", pos, 60, "the reloaded kernel starts (Reload 1)")
    c = connect(120)
    err, t = call(c, "reload_info")
    need("nothing to confirm: this kernel came by Reload 1" in t, "reload_info: a reloaded kernel has nothing to confirm", t)
    # RB-10: 4 is still on trial (no OK on the stick), so the reloaded kernel says so.
    err, t = call(c, "reboot", {"dry_run": True})
    need(not err and re.search(r"running generation 4 \(newest, on trial \(TRY\)(; kernel id [0-9a-f]{16})?\)", t), "reboot after the Reload: 4 still on trial", t)
    end = time.time() + 20 + 15
    while time.time() < end:  # MCP answers all along: more than the proof time
        call(c, "reload_info")
        time.sleep(2)
    need("boot.confirmed" not in serial()[pos:], "the reloaded kernel never confirms 4 (boot_confirm_s=20 and 15 s more of MCP answers)")
    need(listing(c, "/boot/BITSOS2/SYS/4") == ["INITRD", "KERNEL", "NEW", "TRY"], "SYS/4 still holds NEW and TRY, no OK")
    c.close()
    pos = reset(sock_path, r"generation\.chosen number=1 reason=FALLBACK_AFTER_TRY skipped=4", "bitsboot falls back to generation 1 (4 never confirmed)")
    c = connect(120)

    # 8. The good kernel again as 5 (4 replaced), with a CONFIG.TXT that only adds a comment line.
    cfg5 = cfg0 + "# rb6 scenario: written by install_kernel\n"
    err, t = call(c, "install_kernel", {"url": f"{base}/good/", "config": cfg5})
    need(not err and "installed generation 5 (NEW)" in t and "unconfirmed generation 4 was removed" in t and "CONFIG.TXT written" in t,
         "the good kernel installed as 5 with a new CONFIG.TXT, 4 replaced", t)
    need(read_text(c, "/boot/BITSOS2/CONFIG.TXT") == cfg5, "CONFIG.TXT holds the new text")
    need(listing(c, "/boot/BITSOS2/SYS") == ["1", "5"], "SYS holds 1 and 5")
    c.close()

    # 9. 5's one try comes up and confirms itself.
    pos = reset(sock_path, r"generation\.chosen number=5 reason=NEWEST_TRY", "bitsboot starts generation 5 as its one try")
    c = connect(120)
    wait_for(r"kexec boot\.confirm_pending generation=5", pos, 60, "the kernel of 5 waits for its confirmation")
    err, t = call(c, "reload_info")
    need("generation 5 waits for its confirmation" in t, "reload_info shows the confirmation pending", t)
    end = time.time() + 120
    while "kexec boot.confirmed generation=5" not in serial()[pos:]:
        if time.time() > end:
            need(False, "generation 5 confirms itself within 120 s of MCP answers")
        call(c, "reload_info")
        time.sleep(2)
    wait_for(r"kexec boot\.confirmed generation=5 pruned=\"?\[1\]", pos, 5, "generation 5 confirmed (OK), 1 pruned (sys_keep=1)")
    need(True, "generation 5 confirmed itself (OK), 1 pruned by sys_keep=1")
    need("boot.confirm_gap" not in serial()[pos:], "MCP answered without a pause over the bound")
    err, t = call(c, "reload_info")
    need("generation 5 confirmed (OK) in this boot" in t, "reload_info shows the confirmation", t)
    need(listing(c, "/boot/BITSOS2/SYS") == ["5"], "SYS holds only 5")
    need(listing(c, "/boot/BITSOS2/SYS/5") == ["INITRD", "KERNEL", "OK"], "SYS/5 holds KERNEL INITRD OK")
    # RB-10 (desk-minipc-w12 follow-up 5): a Reload after the confirmation hands 5 on as confirmed, read from the
    # stick's OK marker at the jump, so `reboot` and `hw_info` of the new kernel no longer say "on trial".
    pos = len(serial())
    err, t = call(c, "reload", {"url": f"{base}/good/", "timeout_ms": 60000})
    need(not err and "reloading once this answer is out" in t and "not confirmed yet" not in t, "Reload after 5 confirmed itself", t)
    c.close()
    wait_for(r"kexec reload\.generation_confirmed reload=1 generation=5", pos, 60, "the Reload read 5's OK marker and hands 5 on as confirmed")
    m, pos = wait_for(r"kernel start version=\S+ generation=5 reload=1 gen_reason=(\d+)", pos, 60, "the reloaded kernel starts (Reload 1)")
    need(m.group(1) == "1", f"the reloaded kernel's GENERATION record says NEWEST_OK (gen_reason={m.group(1)})")
    c = connect(120)
    err, t = call(c, "reboot", {"dry_run": True})
    need(not err and re.search(r"running generation 5 \(newest, confirmed \(OK\)(; kernel id [0-9a-f]{16})?\)", t) and "on trial" not in t, "reboot after the Reload: 5 confirmed, not on trial", t)
    err, t = call(c, "hw_info")
    need(not err and re.search(r"generation 5 reason=1 \(newest, confirmed \(OK\)(; kernel id [0-9a-f]{16})?\)", t), "hw_info after the Reload: 5 confirmed", t)
    c.close()

    # 10. The next boot starts the confirmed 5.
    pos = reset(sock_path, r"generation\.chosen number=5 reason=NEWEST_OK", "bitsboot starts generation 5 as NEWEST_OK")
    c = connect(120)
    err, t = call(c, "reload_info")
    need("nothing to confirm" in t and "NEWEST_OK" in t, "reload_info: nothing to confirm on 5", t)
    print("cycle: green", flush=True)


def refused():
    base = sys.argv[5]
    wait_for(r"fat stick\.decided [^\n]*rw=false", 0, 120, "two sticks: the boot stick stays read-only")
    c = connect(120)
    err, t = call(c, "install_kernel", {"url": f"{base}/good/"})
    need(err and "read-only" in t, "install_kernel refuses a read-only stick", t)
    err, t = call(c, "install_kernel", {"url": f"{base}/good/", "dry_run": True})
    print(f"note: dry run on the read-only stick: {'refused' if err else 'planned'}: {t.splitlines()[0] if t else ''}", flush=True)
    print("refused: green", flush=True)


if MODE == "cycle":
    cycle()
elif MODE == "refused":
    refused()
else:
    raise SystemExit(f"unknown mode {MODE}")
