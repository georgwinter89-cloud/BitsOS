#!/usr/bin/env python3
"""RB-5a/RB-5b scenario driver: Reload over MCP under QEMU with an igb card and <cpus> CPUs, ten times in a row,
then once under memory pressure and once refused for lack of memory.

Usage: reload_scenario.py <mcp host port> <token> <serial log> <bundle dir> <http port as the guest sees it> [cpus]

The bundle dir holds base/KERNEL and base/INITRD (the image's own files). For reload k this script writes
<k>/KERNEL (the same kernel) and <k>/INITRD (the base initramfs with k extra files, so every kernel gets a
different one) and serves them through the shell wrapper's HTTP server. Every step prints one line; exit 0 =
green. Checks per reload, from the serial log (the mirror of RB-4 does not exist yet) unless said otherwise:
  old kernel: kexec reload.jump reload=k devices=<igc...> cpus_stopped=<cpus-1> by_irq=<cpus-1> by_nmi=0
              halted=<cpus-1> init_sent=<cpus-1> init_failed=0
              init_refused=false init_unused_failed=0 bus_master_swept=0 bus_master_refused=0 lint0_extint=true
  new kernel: kexec handover reload=k tsc_from_reload=true initrd_bytes/initrd_fnv of <k>/INITRD,
              time clock.ready source=reload with the first kernel's rate, kernel start reload=k,
              smp done cpus=<cpus> up=<cpus-1> failed=0 (every AP came back from INIT),
              vfs root.ready files=<base + k>; MCP answers again within 20 s; then from the new kernel's log
              over MCP (`log`, after its own `kernel start reload=k`): kernel config with this CONFIG,
              time tsc.recheck hypervisor=true outcome=kept|hypervisor over >= 1 s (INT-A1e: log only), no
              tsc.rehandover. These three come late, when init and settingsd write to the console too: a
              program's console output reaches the serial port unlocked and can land inside a kernel line there
              (RB-fl), the log ring holds each record whole."""
import http.client
import json
import os
import re
import shutil
import sys
import time

# --selftest checks the MCP log parsing offline against canned answers (15-reload-parser.sh, plain gate).
SELFTEST = sys.argv[1:2] == ["--selftest"]
if not SELFTEST:
    PORT, TOKEN, LOG, BUNDLES, GUEST_HTTP = int(sys.argv[1]), sys.argv[2], sys.argv[3], sys.argv[4], sys.argv[5]
    CPUS = int(sys.argv[6]) if len(sys.argv) > 6 else 1
else:
    LOG = os.devnull
MCP_BACK_S = 20
next_id = 0


def need(cond, what, detail=""):
    if not cond:
        tail = ""
        try:
            with open(LOG, "rb") as f:
                tail = f.read()[-3000:].decode("utf-8", "replace")
        except OSError:
            pass
        raise SystemExit(f"RED: {what}\n{detail}\n--- serial tail ---\n{tail}")
    print(f"ok: {what}", flush=True)


def connect(wait_s):
    end = time.time() + wait_s
    while True:
        try:
            c = http.client.HTTPConnection("127.0.0.1", PORT, timeout=60)
            rpc(c, "initialize", {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "rb5a-scenario", "version": "1"}})
            return c
        except (OSError, http.client.HTTPException, ValueError) as e:
            if time.time() > end:
                need(False, f"MCP answers on 127.0.0.1:{PORT} within {wait_s} s", str(e))
            time.sleep(0.5)


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


def call(c, name, args=None):
    r = rpc(c, "tools/call", {"name": name, "arguments": args or {}})
    return r["isError"], "\n".join(i.get("text", "") for i in r["content"] if i["type"] == "text")


def serial():
    with open(LOG, "rb") as f:
        return f.read().decode("utf-8", "replace")


def wait_for(pattern, start, timeout_s, what):
    end = time.time() + timeout_s
    rx = re.compile(pattern)
    while True:
        text = serial()[start:]
        # Only whole lines: QEMU may be in the middle of writing the one we look for.
        text = text[:text.rfind("\n") + 1]
        m = rx.search(text)
        if m:
            return m, start + m.end()
        if time.time() > end:
            need(False, f"{what} within {timeout_s} s", f"pattern: {pattern}")
        time.sleep(0.2)


LOG_LINE = re.compile(r"#(\d+) \[\s*[\d.]+\] \w (\S+) (\S+) ?(.*)")


def parse_log(t, sub, name):
    """(seq, text) of the records exactly `sub name` in an MCP `log` answer, in answer order. The tool's filters
    are a subsystem with its dotted children and a name prefix, so both are compared exactly here."""
    return [(int(m.group(1)), m.group(4)) for m in map(LOG_LINE.fullmatch, t.splitlines()[1:])
            if m and m.group(2) == sub and m.group(3) == name]


def newest_after(rs, seq):
    """Text of the newest record after `seq`, or None."""
    rs = [r for r in rs if r[0] > seq]
    return rs[-1][1] if rs else None


def config_values(text):
    """The values of a `kernel config` record if they carry the handed-on MCP port and the masked token."""
    m = re.fullmatch(r'values="([^"]*)"', text or "")
    return m.group(1) if m and "mcp_port=8765" in m.group(1) and "token=***" in m.group(1) else None


def logged(c, sub, name):
    """(seq, text) of the newest records `sub name` in the running kernel's log, oldest first (MCP `log`)."""
    err, t = call(c, "log", {"subsystem": sub, "name": name, "max": 50})
    need(not err and t.startswith("log next="), f"MCP log answers for {sub} {name}", t)
    return parse_log(t, sub, name)


def logged_after(c, sub, name, seq, timeout_s, what):
    """The newest `sub name` record after `seq`, waited for up to `timeout_s`."""
    end = time.time() + timeout_s
    while True:
        r = newest_after(logged(c, sub, name), seq)
        if r is not None:
            return r
        if time.time() > end:
            need(False, f"{what} within {timeout_s} s", f"no {sub} {name} after #{seq}")
        time.sleep(0.5)


def fnv1a64(b):
    h = 0xcbf29ce484222325
    for x in b:
        h = ((h ^ x) * 0x100000001b3) & 0xFFFFFFFFFFFFFFFF
    return h


def newc_entries(data):
    """(name, mode, payload) of every entry before TRAILER!!! in a cpio newc archive."""
    out, off = [], 0
    while off + 110 <= len(data):
        h = data[off:off + 110]
        if h[:6] != b"070701":
            need(False, "the base initramfs is cpio newc", repr(h[:6]))
        mode, size, nlen = int(h[14:22], 16), int(h[54:62], 16), int(h[94:102], 16)
        name = data[off + 110:off + 110 + nlen - 1].decode()
        doff = (off + 110 + nlen + 3) & ~3
        if name == "TRAILER!!!":
            break
        out.append((name, mode, data[doff:doff + size]))
        off = (doff + size + 3) & ~3
    return out


def newc(entries):
    out = bytearray()
    for ino, (name, mode, payload) in enumerate(entries + [("TRAILER!!!", 0, b"")], 1):
        nb = name.encode() + b"\0"
        fields = [ino if name != "TRAILER!!!" else 0, mode, 0, 0, 1, 0, len(payload), 0, 0, 0, 0, len(nb), 0]
        out += b"070701" + b"".join(b"%08X" % f for f in fields) + nb
        out += b"\0" * (-len(out) % 4) + payload
        out += b"\0" * (-len(out) % 4)
    return bytes(out)


def make_bundle(k):
    d = os.path.join(BUNDLES, str(k))
    os.makedirs(d, exist_ok=True)
    dst = os.path.join(d, "KERNEL")
    if not os.path.exists(dst):
        try:
            os.link(os.path.join(BUNDLES, "base", "KERNEL"), dst)
        except OSError:
            shutil.copyfile(os.path.join(BUNDLES, "base", "KERNEL"), dst)
    with open(os.path.join(BUNDLES, "base", "INITRD"), "rb") as f:
        base = newc_entries(f.read())
    extra = [(f"reload-tag-{i}", 0o100644, f"reload {k} file {i}\n".encode()) for i in range(k)]
    data = newc(base + extra)
    with open(os.path.join(d, "INITRD"), "wb") as f:
        f.write(data)
    return len(data), fnv1a64(data)


def reload_once(c, k, base_files, hz0, expect_counter, keys=(), source="unchanged"):
    """One Reload; `keys`: `key=value` the new kernel's CONFIG must hold, `source`: what RB-10's re-read of the
    stick's CONFIG.TXT at the jump logs (`unchanged`, or `stick` after a key was written since the last start)."""
    size, fnv = make_bundle(k)
    start = len(serial())
    t0 = time.time()
    err, t = call(c, "reload", {"url": f"http://10.0.2.2:{GUEST_HTTP}/{k}/", "timeout_ms": 60000})
    need(not err and "reloading once this answer is out" in t and f"Reload counter -> {expect_counter}" in t, f"reload {k}: answered first", t)
    m, pos = wait_for(r"kexec reload\.jump reload=(\d+) devices=(\S*) .*?cpus_stopped=(\d+) by_irq=(\d+) by_nmi=(\d+) "
                      r"stop_us=\d+ halted=(\d+) init_sent=(\d+) init_failed=(\d+)(.*)", start, 30, f"reload {k}: old kernel quiesced and jumped")
    rc = re.search(rf"kexec reload\.config reload={expect_counter} source=(\w+)", serial()[start:pos])
    need(rc and rc.group(1) == source, f"reload {k}: CONFIG.TXT read from the stick again before the jump (source={rc.group(1) if rc else '?'}, want {source})")
    need(int(m.group(1)) == expect_counter and "igc" in m.group(2) and "xhci" in m.group(2),
         f"reload {k}: jump with counter {expect_counter}, igc and xhci quiesced ({m.group(2)})")
    o = CPUS - 1
    stopped = tuple(int(m.group(i)) for i in range(3, 9))
    need(stopped == (o, o, 0, o, o, 0), f"reload {k}: {o} APs parked by IPI, halted and sent INIT "
         f"(cpus_stopped/by_irq/by_nmi/halted/init_sent/init_failed = {stopped})")
    rest = m.group(9)
    for want in ("init_refused=false", "init_unused_failed=0", "bus_master_swept=0", "bus_master_refused=0", "lint0_extint=true"):
        need(f" {want} " in rest, f"reload {k}: {want} (INIT from the boot CPU, no function left with bus master, LINT0 back to virtual wire)", rest)
    _, pos = wait_for(r"BitsOS V2 kernel: entry", pos, 30, f"reload {k}: new kernel entered")
    # RB-11: the old kernel hands the fetched KERNEL file's id to the new one in the crash page's marker.
    fk, _ = wait_for(r"kexec reload\.fetched what=kernel .*?fnv=(0x[0-9a-f]+)", start, 5, f"reload {k}: the old kernel logged the kernel id it fetched")
    hk, _ = wait_for(rf"crash kernel_id\.handed kernel_id=(0x[0-9a-f]+) reload={expect_counter}\b", pos, 60, f"reload {k}: the new kernel knows its kernel id")
    need(fk.group(1) == hk.group(1), f"reload {k}: the new kernel's id {hk.group(1)} is the fetched KERNEL file's ({fk.group(1)})")
    m, pos = wait_for(r"time clock\.ready tsc_hz=(\d+) source=(\w+)", pos, 30, f"reload {k}: clock ready")
    need(int(m.group(1)) == hz0 and m.group(2) == "reload", f"reload {k}: TSC rate handed on ({m.group(1)} Hz, source={m.group(2)})")
    m, pos = wait_for(r"kernel start version=\S+ generation=(\d+) reload=(\d+)", pos, 30, f"reload {k}: kernel start line")
    need(int(m.group(2)) == expect_counter, f"reload {k}: kernel start reload={m.group(2)}")
    m, pos = wait_for(r"smp (?:done|not_started) (\S.*)", pos, 60, f"reload {k}: SMP bring-up ran")
    need(CPUS == 1 or (f"cpus={CPUS} " in m.group(1) and f"up={CPUS - 1} failed=0" in m.group(1)),
         f"reload {k}: the new kernel sees all {CPUS} CPUs ({m.group(1)})")
    m, pos = wait_for(r"kexec handover reload=(\d+) tsc_hz=(\d+) tsc_from_reload=(\w+) initrd_bytes=(\d+) initrd_fnv=0x([0-9a-f]+)", pos, 30, f"reload {k}: handover event")
    need(int(m.group(1)) == expect_counter and m.group(3) == "true" and int(m.group(4)) == size and int(m.group(5), 16) == fnv,
         f"reload {k}: the new kernel got bundle {k}'s initramfs ({size} bytes, fnv {fnv:#x})", m.group(0))
    m, pos = wait_for(r"vfs root\.ready fs=\S+ initramfs_bytes=\d+ files=(\d+)", pos, 30, f"reload {k}: VFS unpacked the initramfs")
    need(int(m.group(1)) == base_files + k, f"reload {k}: VFS has {m.group(1)} initramfs files (base {base_files} + {k})")
    c = connect(60)
    back = time.time() - t0
    err, t = call(c, "reload_info")
    need(not err and f"reload counter {expect_counter} " in t, f"reload {k}: MCP back after {back:.1f} s, reload_info counter {expect_counter}", t)
    need(back < MCP_BACK_S, f"reload {k}: MCP back within {MCP_BACK_S} s ({back:.1f} s)")
    # The rest from the new kernel's own log over MCP (RB-fl: on serial a program's console output can split
    # these late lines). Anchored at this kernel's start record, so no older record can answer.
    starts = [r for r in logged(c, "kernel", "start") if re.search(rf"\breload={expect_counter}\b", r[1])]
    need(starts, f"reload {k}: MCP log holds this kernel's start (reload={expect_counter})")
    s0 = starts[-1][0]
    t = logged_after(c, "kernel", "config", s0, 30, f"reload {k}: CONFIG read")
    cfg = config_values(t)
    need(cfg, f"reload {k}: CONFIG handed on ({cfg})", t)
    for kv in keys:
        need(kv in cfg.split(), f"reload {k}: the new kernel's CONFIG has {kv}, written to the stick since the last start", cfg)
    # INT-A1e: the handed-on rate is checked against the HPET one second after the calibration; under QEMU that
    # only logs (hypervisor=true), and the rate stays the first kernel's.
    t = logged_after(c, "time", "tsc.recheck", s0, 30, f"reload {k}: handed-on TSC rate re-checked")
    m = re.match(r"outcome=(\w+) handed_hz=(\d+) measured_hz=(\d+) ppm=(-?\d+) span_ms=(\d+) hypervisor=(\w+)", t)
    need(m and m.group(6) == "true" and m.group(1) in ("kept", "hypervisor") and int(m.group(2)) == hz0 and int(m.group(5)) >= 1000,
         f"reload {k}: TSC re-check against the HPET logged only ({m.group(1)}, {m.group(4)} ppm over {m.group(5)} ms)" if m else
         f"reload {k}: TSC re-check line", t)
    need(newest_after(logged(c, "time", "tsc.rehandover"), s0) is None, f"reload {k}: no rate change under QEMU")
    return c, pos


def free_mb(text):
    return int(re.findall(r"mm frames total_mb=\d+ free_mb=(\d+)", text)[-1])


def main():
    c = connect(240)
    boot = serial()
    base_files = int(re.search(r"vfs root\.ready fs=\S+ initramfs_bytes=\d+ files=(\d+)", boot).group(1))
    # CP-5: the rate handed on is the HPET calibration's (clock.calibrated), not the first 10 ms PIT rate.
    hz0 = int((re.search(r"time clock\.calibrated tsc_hz=(\d+)", boot) or re.search(r"time clock\.ready tsc_hz=(\d+) source=pit", boot)).group(1))
    free0 = free_mb(boot)
    err, t = call(c, "reload_info")
    need(not err and "reload counter 0 " in t and re.search(r"stub page 0x[0-9a-f]+: ready", t), "first boot: reload_info counter 0, stub page ready", t)
    m = re.search(r"smp (?:done|not_started) (\S.*)", boot)
    need(m and (CPUS == 1 or f"cpus={CPUS} " in m.group(1)), f"first boot: {CPUS} CPUs online", m.group(0) if m else "no smp line")
    print(f"first boot: {base_files} initramfs files, TSC {hz0} Hz, {free0} MiB free, {CPUS} CPUs", flush=True)

    # Refusals over MCP: wrong scheme, not LAN, a dry run that fetches and checks but stays.
    err, t = call(c, "reload", {"url": f"ftp://10.0.2.2:{GUEST_HTTP}/1/"})
    need(err and "wrong URL scheme" in t, "reload refuses ftp://", t)
    err, t = call(c, "reload", {"url": "http://93.184.216.34/KERNEL"})
    need(err and "not on a private network" in t, "reload refuses a public address (R9)", t)
    make_bundle(0)
    err, t = call(c, "reload", {"url": f"http://10.0.2.2:{GUEST_HTTP}/0/", "dry_run": True, "timeout_ms": 60000})
    need(not err and "dry run" in t and "Reload counter -> 1" in t, "dry run fetches and checks, nothing happens", t)
    err, t = call(c, "reload_info")
    need(not err and "reload counter 0 " in t, "still the first kernel after the dry run", t)

    # RB-10: keys written to the stick's CONFIG.TXT between two Reloads (as settingsd does) reach the next kernel
    # and stay for the ones after it; a later line with the same key wins.
    # The file tools never change CONFIG.TXT; settingsd's kernel keys do (kernel.console_font: Restart::Reboot, so
    # settingsd applies nothing now, it only writes the line; kernel.console_log is applied now and written too).
    writes = {2: ("kernel.console_font", "vga"), 6: ("kernel.console_log", "info")}
    times = []
    want = ()
    for k in range(1, 11):
        source = "unchanged"
        if k in writes:
            key, value = writes[k]
            end = time.time() + 60
            while True:  # settingsd registers its tools a moment after MCP is back
                err, t = call(c, "settings_set", {"key": key, "value": value})
                if not err or time.time() > end:
                    break
                time.sleep(1)
            need(not err, f"before reload {k}: settings_set {key}={value} (settingsd writes the stick's CONFIG.TXT)", t)
            err, t = call(c, "read_file", {"path": "/boot/BITSOS2/CONFIG.TXT", "encoding": "text"})
            line = f"{key.split('.', 1)[1]}={value}"
            need(not err and line in t, f"before reload {k}: CONFIG.TXT holds {line}", t)
            make_bundle(k)
            err, t = call(c, "reload", {"url": f"http://10.0.2.2:{GUEST_HTTP}/{k}/", "dry_run": True, "timeout_ms": 60000})
            need(not err and "CONFIG: the stick's CONFIG.TXT (" in t, f"before reload {k}: the dry run names the stick's newer CONFIG.TXT", t)
            want, source = want + (line,), "stick"
        t0 = time.time()
        c, _ = reload_once(c, k, base_files, hz0, k, want, source)
        times.append(time.time() - t0)
    err, t = call(c, "reload", {"url": f"http://10.0.2.2:{GUEST_HTTP}/0/", "dry_run": True, "timeout_ms": 60000})
    need(not err and "CONFIG: unchanged since boot (" in t, "dry run after the ten: CONFIG.TXT unchanged since this kernel's start", t)
    print("ten reloads: MCP back after " + " ".join(f"{x:.1f}" for x in times) + " s (QEMU, not judged)", flush=True)

    # Memory pressure: the running kernel keeps only rb_squeeze_mb free, Reload still fits and the new kernel gets
    # the whole memory back.
    err, t = call(c, "ktest", {"filter": "rb.reload.squeeze", "timeout_ms": 60000})
    need(not err and "passed=1" in t, "squeezed the running kernel's free memory", t)
    err, t = call(c, "reload_info")
    m = re.search(r"free for a Reload now: (\d+) MiB", t)
    # 20-reload.sh squeezes to rb_squeeze_mb=256 (40 until wave 2, 128 until wave 4, 192 until INT-W15: the test
    # bundle's initramfs grew to ~26 MB, then ~46 MB with WY-1b's foot, then ~77 MB with WY-11's Adwaita cursor
    # theme, and it must fit in one piece among the squeeze's leftovers).
    need(m and int(m.group(1)) < 264, f"reload_info: only {m.group(1) if m else '?'} MiB to spare", t)
    c, pos = reload_once(c, 11, base_files, hz0, 11)
    after = free_mb(serial()[serial().rfind("BitsOS V2 kernel: entry"):])
    need(abs(after - free0) <= 16, f"under pressure: the new kernel has {after} MiB free again (first boot {free0})")

    # Too little memory: refused, the running kernel goes on.
    err, t = call(c, "ktest", {"filter": "rb.reload.choke", "timeout_ms": 60000})
    need(not err and "passed=1" in t, "choked the running kernel's free memory", t)
    make_bundle(12)
    start = len(serial())
    err, t = call(c, "reload", {"url": f"http://10.0.2.2:{GUEST_HTTP}/12/", "timeout_ms": 60000})
    need(err and "does not fit" in t, "reload refused: does not fit", t)
    wait_for(r"kexec reload\.refused", start, 10, "refusal logged")
    err, t = call(c, "reload_info")
    need(not err and "reload counter 11 " in t, "the running kernel goes on after the refusal", t)
    err, t = call(c, "version")
    need(not err and "BitsOS V2" in t, "MCP still answers", t)
    need(TOKEN not in serial(), "the token never appears in the log (R8i)")
    print(f"rb reload: green (refusals, dry run, 10 reloads (settingsd's CONFIG.TXT keys written before 2 and 6 handed on) with igb + xhci and {CPUS} CPUs, one under memory pressure, one refused for memory)")


def selftest():
    """The parsing of the late checks against canned `log` answers in the format of klogd::tool::render."""
    t = "\n".join([
        "log next=31 oldest=2 shown=6 missed=3",
        "#4 [    0.010000] I kernel start version=0.1 generation=7 reload=0",
        "[... 3 records lost]",
        '#9 [    0.120000] I kernel config values="loglevel=info mcp_port=8765 panic=halt token=***"',
        "#12 [    0.900000] I kernel.boot config.extra values=x",
        "#20 [    2.000000] I kernel start version=0.1 generation=7 reload=1",
        '#21 [    2.100000] I kernel config values="loglevel=info mcp_port=8765 panic=halt rb_choke_mb=10 token=***"',
        "#22 [    2.200000] I kernel configs.other n=1",
        "#30 [    3.000000] I kernel config",
    ])
    need(parse_log(t, "kernel", "start") == [(4, "version=0.1 generation=7 reload=0"), (20, "version=0.1 generation=7 reload=1")],
         "parse_log: both start records, gap line skipped")
    cfgs = parse_log(t, "kernel", "config")
    need([r[0] for r in cfgs] == [9, 21, 30], "parse_log: exact subsystem and name (no kernel.boot, no configs.other)", repr(cfgs))
    need(cfgs[-1] == (30, ""), "parse_log: a record without fields has empty text", repr(cfgs[-1]))
    need(newest_after(cfgs[:2], 20) == cfgs[1][1], "newest_after: only a record after this kernel's start answers")
    need(newest_after(cfgs[:1], 20) is None, "newest_after: an older kernel's config does not answer")
    need(config_values(cfgs[1][1]) == "loglevel=info mcp_port=8765 panic=halt rb_choke_mb=10 token=***", "config_values: accepts the handed-on config")
    for bad in ("", 'values="loglevel=info panic=halt token=***"', 'values="mcp_port=8765 token=rb5a-scenario-token-91c2"',
                'values="mcp_port=8765 tok init: Bit en=***', 'values="mcp_port=8765 token=***" extra=1', None):
        need(config_values(bad) is None, f"config_values: rejects {bad!r}")
    need(parse_log("log next=0 oldest=0 shown=0 missed=0", "kernel", "config") == [], "parse_log: an empty answer has no records")
    print("rb reload parser: green")


selftest() if SELFTEST else main()
