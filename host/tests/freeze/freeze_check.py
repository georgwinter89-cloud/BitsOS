#!/usr/bin/env python3
"""SC-7c freeze suite: B4 "the desktop never freezes", driven over MCP on a QEMU with 16 CPUs.

Usage: freeze_check.py <host port> <token> <result dir>

The program in front (`abi-freeze probe`, made Interactive with sched_focus) must meet every deadline and see every
injected key event within a generous limit while 64 busy threads (`stress`), a hung program holding a PI futex
(`abi-freeze hold`) and FS-12's in-kernel disk hog (`block.ram.hog`, looped) run; `abi-freeze hog` adds write +
fsync load on the page cache (/tmp: MCP programs are uid 1000 and may not write a FAT disk). Functional only:
QEMU's numbers go to <result dir>/numbers.txt and are never judged beyond the limit; that B4 is really at work is
checked from the scheduler (the probe's threads are in class interactive, the Interactive wait counter grew by
its wake-ups). FREEZE_EXPECT_B4=0 is the run before the desk applied the SC-7a/7b integration: a program that cannot
be made Interactive or a LOCK_PI that is ENOSYS is then noted and the run goes on, but it never ends green.
Every program the run started is stopped and the focus cleared on every way out. Exit status 0 = green."""
import http.client
import json
import os
import re
import sys
import threading
import time

PORT, TOKEN, OUT = int(sys.argv[1]), sys.argv[2], sys.argv[3]
LIMIT_MS = 500          # as sched.interactive.deadline64 under a hypervisor; the Mini-PC list uses 20 ms
PROBE_S, STRESS_S, HOG_S, KEYS_S = 50, 42, 38, 28
BUSY = 64
HOG_PATHS = ("/boot/BITSOS2/FRZHOG.BIN", "/tmp/frzhog.bin")  # the stick's FAT first (root's 0755), then tmpfs
EXPECT_B4 = os.environ.get("FREEZE_EXPECT_B4", "1") != "0"
next_id = 0
numbers = []
# Integration the suite needs but the kernel lacks (an SC patch the desk has not applied). The run goes on, so
# every other check still says what it found, and ends red naming what is missing.
missing = []
started = []  # pids of programs this run started with `run`, stopped on every way out


def lack(what, detail):
    """The kernel lacks B4 integration: red at once, unless this is the pre-integration run (then red at the end)."""
    if EXPECT_B4:
        raise SystemExit(f"RED: {what}\n{detail}")
    missing.append(what)
    print(f"MISSING: {what}\n{detail.strip()[:400]}", flush=True)


def need(cond, what, detail=""):
    if not cond:
        raise SystemExit(f"RED: {what}\n{detail}")
    print(f"ok: {what}", flush=True)


def note(line):
    numbers.append(line)
    print(f"  {line}", flush=True)


def post(c, body):
    c.request("POST", "/mcp", json.dumps(body), {"Authorization": f"Bearer {TOKEN}", "Content-Type": "application/json"})
    r = c.getresponse()
    data = r.read()
    return r.status, (json.loads(data) if data else None)


def rpc(c, method, params=None):
    global next_id
    next_id += 1
    st, j = post(c, {"jsonrpc": "2.0", "id": next_id, "method": method, "params": params or {}})
    if st != 200 or j is None or "result" not in j:
        raise ValueError(f"{method}: HTTP {st} {j}")
    return j["result"]


def connect(wait_s):
    end = time.time() + wait_s
    while True:
        try:
            c = http.client.HTTPConnection("127.0.0.1", PORT, timeout=120)
            rpc(c, "initialize", {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "sc7c", "version": "1"}})
            return c
        except (OSError, http.client.HTTPException, ValueError) as e:
            if time.time() > end:
                raise SystemExit(f"RED: no MCP answer on 127.0.0.1:{PORT}: {e}")
            time.sleep(1)


def call(c, name, args=None):
    """(text, is_error, seconds the call took)."""
    t = time.time()
    r = rpc(c, "tools/call", {"name": name, "arguments": args or {}})
    text = "\n".join(i.get("text", "") for i in r.get("content", []) if i.get("type") == "text")
    return text, bool(r.get("isError")), time.time() - t


def start(c, args, ready, timeout_ms=4000):
    """Starts `args` with run; it must still run after timeout_ms and have printed `ready`. Returns (pid, text)."""
    text, err, _ = call(c, "run", {"path": args[0], "args": args[1:], "timeout_ms": timeout_ms})
    if err and "not found" in text.lower() and args[0] == "abi-freeze":
        raise SystemExit(f"RED: abi-freeze is not in the image (the ABI programs need the musl sysroot: cargo xtask abi-diff once)\n{text}")
    m = re.search(r"pid (\d+)", text)
    # A program still running at the deadline answers "Still running ... (call n)" as an error result.
    running = (not err or text.startswith("Still running")) and "ended:" not in text
    if m:
        started.append(int(m[1]))
    need(running and m is not None, f"{' '.join(args)} runs", text)
    pid = int(m[1])
    seen = text
    end = time.time() + 60
    while ready not in seen and time.time() < end:
        time.sleep(0.5)
        t, _, _ = call(c, "output", {"pid": pid})
        seen += t
        if "ended:" in t:
            break
    need(ready in seen, f"{args[0]} {args[1] if len(args) > 1 else ''} said '{ready}' (pid {pid})", seen)
    return pid, seen


def wait_end(c, pid, seen, within_s):
    end = time.time() + within_s
    while "ended:" not in seen and time.time() < end:
        time.sleep(1)
        t, _, _ = call(c, "output", {"pid": pid})
        seen += t
    return seen


def interactive(c):
    """The `sched` tool's interactive line as a dict (focus, waits, max_us, late, ...)."""
    text, _, _ = call(c, "sched", {})
    m = re.search(r"^interactive (.*)$", text, re.M)
    if m is None:
        raise SystemExit(f"RED: sched has no interactive line\n{text[-2000:]}")
    return dict(kv.split("=", 1) for kv in m[1].split()), m[0]


def group_rows(c, group):
    """(class, wait_max_us) of every thread of `group` in the sched thread table (columns counted from the end:
    a thread name may hold spaces or be empty), and how many thread rows the table had."""
    text, _, _ = call(c, "sched", {"threads": True, "max": 10000})
    rows, total = [], 0
    # Only the thread table: a CPU row has as many columns, and its user_ms (second to last) can equal the group.
    in_threads = False
    for line in text.splitlines():
        if line.startswith("id name class state "):
            in_threads = True
            continue
        p = line.split()
        if in_threads and len(p) >= 18 and p[0].isdigit() and p[-2].isdigit():
            total += 1
            if p[-2] == str(group):
                rows.append((p[-17], int(p[-1]) if p[-1].isdigit() else -1))
    return rows, total


def kernel_hog(stop, runs):
    """FS-12's block.ram.hog through the MCP tool ktest until `stop`; one result line per run."""
    try:
        c = connect(60)
        while not stop.is_set():
            text, err, dt = call(c, "ktest", {"filter": "block.ram.hog"})
            ok = not err and "passed=1 failed=0" in text
            runs.append(f"{'ok' if ok else 'FAIL'} {dt:.1f}s {text.strip().splitlines()[-1] if text.strip() else ''}")
    except (Exception, SystemExit) as e:  # the main check reports a missing or failed run
        runs.append(f"FAIL {e}")


def run_once(c, hog_stop):
    _, line = interactive(c)
    need(line.startswith("interactive focus="), "sched reports the Interactive class", line)
    waits0 = int(interactive(c)[0]["waits"])

    # 1. The program in front.
    probe, probe_out = start(c, ["abi-freeze", "probe", str(PROBE_S), str(LIMIT_MS)], "freeze probe ready")
    text, err, _ = call(c, "sched_focus", {"pid": probe})
    focused = not err
    if focused:
        need(f"focus={probe}\n" in text and re.search(r"threads ([3-9]|\d\d+)", text), f"the probe (pid {probe}) is in front with all its threads", text)
    else:
        lack("sched_focus cannot make a program Interactive (user threads carry no tgid in the scheduler: "
             "plan/notes/sc-SC-7a-integration.patch, proc::task spawns with spawn_in_group)", text)
    t0 = time.time()

    # 2. The load: busy threads, a hung program, disk hogs.
    busy, busy_out = start(c, ["stress", "-t", str(BUSY), "-s", str(STRESS_S)], "", timeout_ms=1500)
    try:
        hold, hold_out = start(c, ["abi-freeze", "hold"] + ([] if EXPECT_B4 else ["nopi-ok"]), "freeze hold ready")
    except SystemExit as e:
        if "lock_pi ENOSYS" in str(e):
            raise SystemExit("RED: FUTEX_LOCK_PI answers ENOSYS (plan/notes/sc-SC-7b-integration.patch: the PI "
                             f"operations in syscall::futex)\n{e}")
        raise
    if "pi=no" in hold_out:
        lack("FUTEX_LOCK_PI answers ENOSYS (plan/notes/sc-SC-7b-integration.patch: the PI operations in syscall::futex)", hold_out)
        need(re.search(r"freeze hold ready pid=\d+ pi=no", hold_out), "the hung program blocks in plain futex waits", hold_out)
    else:
        need(re.search(r"freeze hold ready pid=\d+ pi=yes owner=me waiters=yes", hold_out), "the hung program holds a PI futex with a waiter blocked on it", hold_out)
    # FS-12's in-kernel hog (a Background thread floods a RamDisk that takes 1 ms per call; an Interactive read
    # must pass it) over and over on a second connection: the device load.
    hog_runs = []
    hog_thread = threading.Thread(target=kernel_hog, args=(hog_stop, hog_runs), daemon=True)
    hog_thread.start()
    # The user-space write + fsync hog: the stick's FAT when MCP programs may write there (they are uid 1000 and
    # \BITSOS2\ is root's 0755, so today it is /tmp: page cache and the fsync path, no device I/O).
    hog, hog_out, hog_where = None, "", ""
    for path in HOG_PATHS:
        text, err, _ = call(c, "run", {"path": "abi-freeze", "args": ["hog", path, str(HOG_S)], "timeout_ms": 4000})
        m = re.search(r"pid (\d+)", text)
        if m and "ended:" not in text:
            started.append(int(m[1]))
        if "freeze hog: open" in text and ("EACCES" in text or "ENOENT" in text or "EROFS" in text):
            print(f"note: the user-space disk hog cannot write {path} (uid 1000): {text.strip().splitlines()[0]}", flush=True)
            continue
        need(m is not None and "freeze hog ready" in text and "ended:" not in text, f"the user-space hog runs on {path}", text)
        hog, hog_out, hog_where = int(m[1]), text, path
        break
    need(hog is not None, "the user-space hog found a writable place", " ".join(HOG_PATHS))

    # 3. Input while loaded: shift alone (harmless on the console), 10 presses per call.
    sent, calls, slowest, dropped = 0, 0, 0.0, 0
    mid_rows, mid_total, mid_line = [], 0, ""
    while time.time() - t0 < KEYS_S:
        text, err, dt = call(c, "send_keys", {"keys": ["shift"] * 10})
        if err:
            raise SystemExit(f"RED: send_keys failed under load\n{text}")
        m = re.search(r"ring dropped (\d+) events", text)
        dropped += int(m[1]) if m else 0
        sent += 10
        calls += 1
        slowest = max(slowest, dt)
        if calls == 8:
            mid_rows, mid_total = group_rows(c, probe)
            mid_line = interactive(c)[1]
        time.sleep(0.7)
    need(sent >= 100, f"{sent} key presses sent in {calls} send_keys calls under load")
    note(f"qemu send_keys calls={calls} presses={sent} slowest_call_s={slowest:.2f} dropped={dropped}")
    need(mid_line != "" and mid_total >= BUSY, f"sched read under load ({mid_total} thread rows)", mid_line)
    note("qemu sched " + mid_line)
    classes = sorted({k for k, _ in mid_rows})
    note(f"qemu probe threads under load={len(mid_rows)} classes={','.join(classes) or '-'}")
    if focused:
        # B4 at work, not only timing that a free CPU would give any class: the probe's group is in front and
        # every one of its threads runs in class interactive under the load.
        need(re.search(rf"^interactive focus={probe} ", mid_line), "sched shows the probe's group in front under load", mid_line)
        need(len(mid_rows) >= 3 and classes == ["interactive"], f"all {len(mid_rows)} threads of the probe run in class interactive under load", f"{mid_rows}")

    # 4. The probe's verdict.
    probe_out = wait_end(c, probe, probe_out, PROBE_S + 120)
    need("ended:" in probe_out, "the probe ended", probe_out[-3000:])
    # Output can come twice (the progress of a still running call, then the result), so lines are taken once.
    for line in dict.fromkeys(re.findall(r"^freeze probe (?:deadlines|input).*$", probe_out, re.M)):
        note("qemu " + line)
    d = re.search(r"^freeze probe deadlines=(\d+) late=(\d+) ", probe_out, re.M)
    m = re.search(r"^freeze probe input presses=(\d+) key_events=(\d+) late=(\d+) .* dropped=(\d+) errors=(\d+)$", probe_out, re.M)
    need(m is not None and int(m[1]) == sent, f"every injected press reached the reader in front ({m[1] if m else '?'} of {sent})", probe_out[-3000:])
    need(int(m[2]) >= int(m[1]), f"{m[2]} key events (presses, releases) timed", m[0])
    need(d is not None and int(d[1]) > 0 and d[2] == "0", f"every deadline of the probe's clock thread met within {LIMIT_MS} ms", probe_out[-3000:])
    need(m[3] == "0" and m[4] == "0" and m[5] == "0", f"every key event read within {LIMIT_MS} ms, no SYN_DROPPED, no read error", m[0])
    need("freeze probe green" in probe_out and f"pid {probe} ended: exit 0" in probe_out, "the probe is green", probe_out[-3000:])
    waits1 = int(interactive(c)[0]["waits"])
    note(f"qemu interactive waits during the run={waits1 - waits0} probe deadlines={d[1]}")
    if focused:
        # Every sleep-to-deadline wake of an Interactive thread is one measured wait (sched::interactive::switched_in);
        # half leaves room for deadlines already past when the thread asked to sleep.
        need(waits1 - waits0 >= int(d[1]) // 2, f"the Interactive wait counter grew by the probe's wake-ups ({waits1 - waits0} for {d[1]} deadlines)")

    # 5. No starvation: every busy thread counted, the hogs made progress.
    busy_out = wait_end(c, busy, busy_out, 120)
    need(f"pid {busy} ended: exit 0" in busy_out and f"stress: {BUSY} workers (threads)" in busy_out, f"all {BUSY} busy threads counted (none starved)", busy_out[-3000:])
    counts = list({w: int(n) for w, n in re.findall(r"^stress: worker (\d+) count=(\d+)", busy_out, re.M)}.values())
    need(len(counts) == BUSY and min(counts) > 0, f"{len(counts)} worker counts, the smallest {min(counts) if counts else 0}")
    note(f"qemu stress min_count={min(counts)} max_count={max(counts)} ratio={max(counts) / max(min(counts), 1):.1f}")
    hog_stop.set()
    hog_thread.join(300)
    need(not hog_thread.is_alive(), "the kernel disk hog loop ended")
    bad = [r for r in hog_runs if not r.startswith("ok")]
    need(len(hog_runs) >= 3 and not bad, f"FS-12's disk hog ran {len(hog_runs)} times under the load, every run green", "\n".join(bad[:5] or hog_runs))
    hog_out = wait_end(c, hog, hog_out, 120)
    m = re.search(r"^freeze hog writes=\d+ mib=(\d+) fsyncs=(\d+) .* bad_reads=0 error=none$", hog_out, re.M)
    need(f"pid {hog} ended: exit 0" in hog_out and m is not None and int(m[1]) > 0, f"the user-space hog wrote, synced and read back on {hog_where}", hog_out[-3000:])
    note(f"qemu hog_path={hog_where} " + m[0])

    # 6. The hung program is still hung, and ends when stopped (SIGINT reaches threads blocked in futexes).
    t, _, _ = call(c, "output", {"pid": hold})
    hold_out += t
    need("ended:" not in hold_out, "the hung program still holds its futex after the run", hold_out[-2000:])
    text, err, _ = call(c, "stop", {"pid": hold, "grace_ms": 2000})
    hold_out = wait_end(c, hold, hold_out + text, 30)
    need(re.search(rf"pid {hold} ended: signal (2 \(SIGINT\)|9 \(SIGKILL\))", hold_out), "stop ends the hung program", hold_out[-2000:])

    # 7. Focus cleared; the numbers for the record.
    text, _, _ = call(c, "sched_focus", {"pid": 0})
    need("focus=none" in text, "the focus is cleared", text)
    for name, args in (("sched", {}), ("metrics", {"name": "sched.interactive", "last": 120}),
                       ("events", {"subsystem": "sched", "name": "interactive.late"}),
                       ("events", {"subsystem": "block", "name": "hog"})):
        text, err, _ = call(c, name, args)
        with open(f"{OUT}/{name}-{args.get('name', 'all')}.txt", "w") as f:
            f.write(text)
        m = re.search(r"^interactive .*$", text, re.M) if name == "sched" else None
        if m:
            note("qemu sched after " + m[0])
        if args.get("name") == "hog":
            lat = sorted(int(x) for x in re.findall(r"latency_us=(\d+)", text))
            if lat:
                note(f"qemu block hog.read events={len(lat)} p50_us={lat[len(lat) // 2]} max_us={lat[-1]}")


def cleanup():
    """Every way out: stop what this run started, clear the focus (also what a manual Mini-PC run needs)."""
    try:
        c = connect(10)
        for pid in dict.fromkeys(started):
            text, _, _ = call(c, "output", {"pid": pid})
            if "ended:" not in text:
                call(c, "stop", {"pid": pid, "grace_ms": 1000})
        call(c, "sched_focus", {"pid": 0})
    except (Exception, SystemExit) as e:
        print(f"note: cleanup incomplete: {e}", flush=True)


def main():
    c = connect(300)
    print(f"ok: MCP answers (B4 integration expected: {'yes' if EXPECT_B4 else 'no, FREEZE_EXPECT_B4=0'})", flush=True)
    hog_stop = threading.Event()
    try:
        run_once(c, hog_stop)
    finally:
        hog_stop.set()
        cleanup()
        with open(f"{OUT}/numbers.txt", "w") as f:
            f.write("\n".join(numbers) + "\n")
    if missing:
        raise SystemExit("RED: every check above passed, but the kernel lacks integration B4 needs:\n  " + "\n  ".join(missing))
    print("freeze: green", flush=True)


if __name__ == "__main__":
    main()
