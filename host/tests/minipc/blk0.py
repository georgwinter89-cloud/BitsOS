#!/usr/bin/env python3
"""BL-0 Mini-PC measurement (plan/notes/desk-W35-mm-blk.md "BL-0 procedure", plan/notes/bl-BL-0.md), over MCP.

Usage: blk0.py <host:port> <out dir> [--tls DIR|auto] [--token-file F] [--state BL-0] [--label idle] [--part nvme1n1p3]
               [--dir /data/home/1000/bench] [--prepare] [--seq-mb 4096] [--rand-mb 1024] [--small 4000] [--runs 3]
               [--real FILE,FILE] [--ceiling-mb 1024] [--controller nvme1] [--allow-stick-data] [--selftest]
The desk runs it through blk0.sh against the Mini-PC with a ktest kernel (fs.drop_caches.all is the cold step's
drop_caches 3: MCP's `run` is uid 1000 and `write_file` stays out of /proc). It refuses to measure unless /data is the
Lexar partition (`--part`): /run/bitsos/data-state says "mounted pool rw" and names the partition, or the boot's
ext4 data.mounted event does. On the stick's data.img the numbers measure the USB stick (`--allow-stick-data` is for
the QEMU proof only; its rows say fs=stick-data-img).
Per measured run: ktest fs.drop_caches.all, 2 s, then sched threads, metrics, locks; `run` of blkbench (which makes
its file cold with DONTNEED + mincore before each of its runs); 4.5 s (two sampler rounds); the same three again.
Printed: one `key=value` block per run (CPU ms of vfs.readahead/<cpu>, mm.kswapd, the flusher; system time; lock waits; counter
deltas; copies per byte) and the table row of the procedure. Everything goes to <out>/blk0-<label>.txt too.
Exit 0 = measured, 1 = a step failed, 2 = refused (not the Lexar partition)."""
import base64
import json
import os
import re
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

FLAGS = ("--prepare", "--allow-stick-data", "--selftest")
VALUES = ("--state", "--label", "--part", "--dir", "--seq-mb", "--rand-mb", "--small", "--runs", "--real", "--ceiling-mb", "--controller")
THREADS = ("blk:", "vfs.readahead", "mm.kswapd", "vfs.flush", "vfs.writeback")
LOCKS = ("nvme.io", "bitsvol.pool", "vfs.inode.data", "vfs.inode.index", "vfs.cache.sync")
# INT-W35-BL: ext4.read_ (BL-3: read_page_calls now counts inline data only; read_windows, read_frames_bytes,
# read_frames_via_cache are the straight reads).
COUNTERS = ("block.", "nvme.inflight_max", "vfs.readahead.", "vfs.cache.fill", "ext4.read_", "mm.reclaim.", "vfs.reclaim.pages")
COPIES = ("readvec_zero", "disk_copy", "bounce", "nvme_bounce", "ext4_page", "bdev_cache")


# ---- parsers (pure; --selftest)

def run_answer(text):
    """MCP run's answer -> (ended, stdout, stderr)."""
    head = text.splitlines()[0] if text else ""
    ended = head.split(" ended: ", 1)[1].split(",")[0].split(" (")[0].strip() if " ended: " in head else ""

    def section(name, start):
        m = re.compile(r"--- " + name + r" \((\d+) [^\n]*\) ---\n").search(text, start)
        if not m:
            return "", start
        n = int(m.group(1))
        body = text[m.end():m.end() + n]
        return body, m.end() + len(body)

    out, at = section("stdout", 0)
    err, _ = section("stderr", at)
    return ended, out, err


def kv_lines(text):
    """blkbench's `test=<name> k=v ...` lines -> list of dicts."""
    rows = []
    for line in text.splitlines():
        if line.startswith("test="):
            rows.append(dict(p.split("=", 1) for p in line.split() if "=" in p))
    return rows


def threads(text):
    """sched threads=true -> {name: (runtime_ms, user_ms, sys_ms)} summed per name (kernel threads by name)."""
    out, on = {}, False
    for line in text.splitlines():
        if line.startswith("id name class state"):
            on = True
            continue
        f = line.split()
        if not on or len(f) < 19 or not f[0].isdigit():
            if on and not f:
                on = False
            continue
        name = " ".join(f[1:len(f) - 17])
        try:
            vals = (int(f[-12]), int(f[-9]), int(f[-8]))
        except ValueError:
            continue
        old = out.get(name, (0, 0, 0))
        out[name] = tuple(a + b for a, b in zip(old, vals))
    return out


def _ms(t):
    if t.endswith("ms"):
        return float(t[:-2])
    if t.endswith("us"):
        return float(t[:-2]) / 1000
    return 0.0


def locks(text):
    """locks -> {class: (acquired, contended, wait_total_ms)}."""
    out = {}
    for line in text.splitlines():
        f = line.split()
        if len(f) == 10 and ":" in f[1] and f[2].isdigit():
            out[f[0]] = (int(f[2]), int(f[3]), _ms(f[4]))
    return out


def metrics(text):
    """metrics -> ({series: last value}, to_seconds)."""
    vals, to = {}, None
    for line in text.splitlines():
        if line.startswith("metrics "):
            m = re.search(r" to=([0-9.]+)", line)
            to = float(m.group(1)) if m else None
            continue
        f = line.split()
        if len(f) >= 3 and not line.startswith("time "):
            last = [v for v in f[2:] if v != "-"]
            if last:
                try:
                    vals[f[0]] = float(last[-1])
                except ValueError:
                    pass
    return vals, to


def lexar(state, events, part, window=""):
    """None when /data is the Lexar partition `part`, else why not. `window` is /proc/bitsos/data-window: with the
    console log at "error" the boot's data.mounted event is not kept, the armed window still names the partition."""
    if not state.startswith("mounted pool rw"):
        return f"/run/bitsos/data-state is {state.strip()!r}, not 'mounted pool rw'"
    if part in state.split():
        return None
    lines = [l for l in events.splitlines() if "data.mounted" in l]
    if any(f"image={part}" in l or f'image="{part}"' in l for l in lines):
        return None
    if any("data.img" in l for l in lines):
        return "/data is the stick's data.img: the numbers would measure the USB stick"
    if window.startswith("window=armed") and f"part={part}" in window.split():
        return None
    return f"neither data-state nor the boot's data.mounted event names {part}"


def deltas(a, b):
    return {k: b[k] - a.get(k, 0) for k in b if isinstance(b[k], (int, float)) and b[k] - a.get(k, 0)}


def selftest():
    ans = "pid 9 ended: exit 0 (12 ms)\n--- stdout (25 bytes) ---\ntest=seq mb_s=12.5 runs=3\n--- stderr (0 bytes) ---\n"
    ended, out, err = run_answer(ans)
    assert ended == "exit 0" and kv_lines(out) == [{"test": "seq", "mb_s": "12.5", "runs": "3"}] and err == "", (ended, out, err)
    sched = ("threads=2 shown=2\nid name class state cpu pin rt_prio runtime_ms migrations wakeups user_ms sys_ms nice policy affinity vcsw ivcsw group wait_max_us\n"
             "12 blk:nvme1n1 interactive sleeping 3 - 0 150 2 900 0 150 0 other all 900 1 0 40\n"
             "40 my prog normal running 1 - 0 30 0 3 20 10 0 other all 3 0 40 0\n")
    assert threads(sched) == {"blk:nvme1n1": (150, 0, 150), "my prog": (30, 20, 10)}, threads(sched)
    lk = "locks classes=1 shown=1 checker=off ...\nclass level acquired contended wait_total wait_max hold_avg hold_max try_failed timeouts\nnvme.io leaf:9 100 4 12ms 3.5us 1.0us 2.0us 0 0\n"
    assert locks(lk) == {"nvme.io": (100, 4, 12.0)}, locks(lk)
    mt = "metrics from=1.0 to=9.5 period_s=2 rounds=4 points=1 series=2 more=0\ntime 9.5\nblock.copy_bytes.bounce n 4096\ndisk.nvme1n1.read_bytes B/s - 100\n"
    assert metrics(mt) == ({"block.copy_bytes.bounce": 4096.0, "disk.nvme1n1.read_bytes": 100.0}, 9.5), metrics(mt)
    ev = '#12 [3.1] I ext4 data.mounted at=/data pool=true writable=true created=false image=nvme1n1p3 | /data lies on the window partition'
    assert lexar("mounted pool rw\n", ev, "nvme1n1p3") is None
    assert lexar("mounted pool rw on nvme1n1p3\n", "", "nvme1n1p3") is None
    assert "data.img" in lexar("mounted pool rw\n", "#3 [2.0] I ext4 data.mounted at=/data image=/boot/BITSOS2/data.img", "nvme1n1p3")
    assert "not 'mounted pool rw'" in lexar("skipped no boot stick\n", ev, "nvme1n1p3")
    assert lexar("mounted pool rw\n", "", "nvme1n1p4", "window=armed disk=nvme1n1 part=nvme1n1p4 first=1 blocks=2\n") is None
    assert "names nvme1n1p4" in lexar("mounted pool rw\n", "", "nvme1n1p4", "window=armed disk=nvme1n1 part=nvme1n1p3 first=1 blocks=2\n")
    assert "names nvme1n1p4" in lexar("mounted pool rw\n", "", "nvme1n1p4", "window=none\n")
    assert deltas({"a": 1, "b": 5}, {"a": 4, "b": 5, "c": 2}) == {"a": 3, "c": 2}
    print("GREEN: blk0.py parsers")


if "--selftest" in sys.argv:
    selftest()
    sys.exit(0)

import mp  # noqa: E402

OPTS = mp.options(sys.argv[1:], positional=2, flags=FLAGS, values=VALUES)
V = {k: OPTS.values[k] for k in VALUES}
OUT = OPTS.rest[1]
LABEL = V["--label"] or "idle"
STATE = V["--state"] or "BL-0"
PART = V["--part"] or "nvme1n1p3"
DIR = V["--dir"] or "/data/home/1000/bench"
RUNS = V["--runs"] or "3"
LOG = []


def say(text):
    LOG.append(text)
    print(text, flush=True)


def red(text, code=1):
    say(f"RED: {text}")
    save()
    sys.exit(code)


def save():
    os.makedirs(OUT, exist_ok=True)
    with open(os.path.join(OUT, f"blk0-{LABEL}.txt"), "w") as f:
        f.write("\n".join(LOG) + "\n")


def call(name, args=None, timeout=900):
    err, text, _ = mp.call(OPTS, name, args, timeout)
    if err:
        red(f"MCP {name} {json.dumps(args)[:200]}: {text[:400]}")
    return text


def read_file(path):
    j = json.loads(call("read_file", {"path": path}))
    return base64.b64decode(j.get("data", "")).decode("utf-8", "replace") if "data" in j else j.get("text", "")


def run(argv, ms=1_800_000):
    """Runs a program to its end (polling `output` while it runs) -> (ended, stdout, stderr)."""
    text = call("run", {"path": argv[0], "args": argv[1:], "timeout_ms": min(ms, 600_000)}, timeout=700)
    t0 = time.time()
    while text.startswith("Still running") or " ended: " not in text.splitlines()[0]:
        m = re.search(r"\(call (\d+)\)", text)
        if not m or time.time() - t0 > ms / 1000:
            red(f"{' '.join(argv)} did not end: {text[:300]}")
        time.sleep(5)
        text = call("output", {"call": int(m.group(1))}, timeout=120)
    return run_answer(text)


def blkbench(*args):
    ended, out, err = run(["/bin/blkbench", *args])
    for line in out.splitlines():
        say("  " + line)
    if ended != "exit 0":
        red(f"blkbench {' '.join(args)}: {ended}\n{out[-800:]}\n{err[-800:]}")
    return kv_lines(out)


def drop():
    text = call("ktest", {"filter": "fs.drop_caches.all", "timeout_ms": 120000}, timeout=200)
    if "ok fs.drop_caches.all" not in text:
        red(f"ktest fs.drop_caches.all (the kernel needs ktests: Reload one first): {text[:400]}")
    time.sleep(2)


def system_ms():
    """System time of all CPUs from /proc/stat's `cpu` line (USER_HZ 100), or None (INT-W35-BL: BL-2 removed the
    blk: threads, so the block layer's CPU is in the readers' and the system time)."""
    try:
        for line in read_file("/proc/stat").splitlines():
            f = line.split()
            if f and f[0] == "cpu" and len(f) > 3:
                return int(f[3]) * 10
    except Exception:  # noqa: BLE001 - a kernel without /proc/stat: the column says "-"
        return None
    return None


def snapshot():
    m, to = metrics(call("metrics", {"name": ",".join(COUNTERS + ("disk.",)), "last": 4, "max": 1, "agg": "last"}))
    return {"threads": threads(call("sched", {"threads": True, "max": 2000})), "locks": locks(call("locks", {"max": 2000, "sort": "name"})), "metrics": m, "to": to,
            "sys_ms": system_ms()}


def measure(what, args):
    drop()
    a = snapshot()
    rows = blkbench(*args)
    time.sleep(4.5)
    b = snapshot()
    main = [r for r in rows if r.get("test") not in ("cold", "cold_resident")]
    res = main[-1] if main else {}
    cpu = {}
    for name, (rt, us, sy) in b["threads"].items():
        if name.startswith(THREADS):
            o = a["threads"].get(name, (0, 0, 0))
            if rt - o[0]:
                cpu[name] = (rt - o[0], us - o[1], sy - o[2])
    lk = {}
    for name, (acq, con, wait) in b["locks"].items():
        if name.startswith(LOCKS):
            o = a["locks"].get(name, (0, 0, 0.0))
            if acq - o[0]:
                lk[name] = (acq - o[0], con - o[1], round(wait - o[2], 3))
    counters = deltas(a["metrics"], {k: v for k, v in b["metrics"].items() if not k.startswith("disk.")})
    disks = {}
    if a["to"] is not None and b["to"] is not None:
        span = {"from": a["to"], "to": b["to"], "max": 1, "agg": "avg", "name": "disk."}
        disks, _ = metrics(call("metrics", span))
    runs = int(res.get("runs", RUNS) or 1)
    # rand reads `count` blocks of `bs` bytes; the others name their bytes.
    per_run = float(res["count"]) * float(res["bs"]) if res.get("test") == "rand" else float(res.get("bytes", 0) or 0)
    read = per_run * runs
    copies = sum(counters.get(f"block.copy_bytes.{c}", 0) for c in COPIES)
    say(f"run={what} label={LABEL} " + " ".join(f"{k}={v}" for k, v in res.items() if k != "test"))
    say("  cpu_ms(runtime,user,sys) " + " ".join(f"{n}={v[0]},{v[1]},{v[2]}" for n, v in sorted(cpu.items())))
    say("  locks(acquired,contended,wait_ms) " + " ".join(f"{n}={v[0]},{v[1]},{v[2]}" for n, v in sorted(lk.items())))
    say("  counters " + " ".join(f"{k}={int(v)}" for k, v in sorted(counters.items())))
    say("  disks_avg " + " ".join(f"{k}={v:g}" for k, v in sorted(disks.items()) if v))
    say(f"  copies_per_byte={copies / read:.2f}" if read else "  copies_per_byte=-")
    sys_ms = b["sys_ms"] - a["sys_ms"] if a["sys_ms"] is not None and b["sys_ms"] is not None else None
    say(f"  system_ms={sys_ms if sys_ms is not None else '-'}")
    return {"res": res, "cpu": cpu, "sys_ms": sys_ms, "copies": copies / read if read else None, "inflight": b["metrics"].get("block.inflight_max"),
            "nvme_inflight": b["metrics"].get("nvme.inflight_max")}


def main():
    state = read_file("/run/bitsos/data-state")
    events = call("events", {"subsystem": "ext4", "name": "data.mounted", "from": 0, "max": 20})
    why = lexar(state, events, PART, read_file("/proc/bitsos/data-window"))
    fs = "ext4/bitsvol on the Lexar"
    if why:
        if not OPTS.flags["--allow-stick-data"]:
            red(f"refused: {why}", 2)
        say(f"note: {why}; measuring anyway (--allow-stick-data: QEMU proof only, not usable numbers)")
        fs = "stick-data-img"
    say(f"blk0 state={STATE} label={LABEL} part={PART} dir={DIR} fs={fs} data-state={state.strip()!r}")
    ended, _, err = run(["/bin/blkbench"])
    if ended != "exit 2" or "usage" not in err:
        red(f"/bin/blkbench is missing or broken: {ended} {err[:200]}")
    ended, _, _ = run(["/bin/sh", "-c", f"test -f {DIR}/seq.bin && test -f {DIR}/rand.bin && test -d {DIR}/small"], 60000)
    if OPTS.flags["--prepare"] or ended != "exit 0":
        sizes = [a for k in ("--seq-mb", "--rand-mb", "--small") if V[k] for a in (k, V[k])]
        blkbench("prepare", DIR, *sizes)
    ended, out, _ = run(["/bin/sh", "-c", "find /data -type f -size +200M 2>/dev/null | sort | head -40"], 300000)
    say("real files over 200 MB on /data: " + (" ".join(out.split()) if ended == "exit 0" and out.strip() else "(none found)"))
    ceil = None
    text = call("nvme", {"speed_mb": int(V["--ceiling-mb"] or 1024), **({"controller": V["--controller"]} if V["--controller"] else {})}, timeout=300)
    m = re.search(r"= (\d+) MB/s", text)
    if m:
        ceil = int(m.group(1))
    say(f"ceiling nvme speed_mb: {text.strip().splitlines()[-1] if text.strip() else '-'}")
    r = {
        "seq": measure("seq", ["seq", f"{DIR}/seq.bin", "--runs", RUNS]),
        "rand1": measure("rand1", ["rand", f"{DIR}/rand.bin", "--threads", "1", "--runs", RUNS]),
        "rand32": measure("rand32", ["rand", f"{DIR}/rand.bin", "--threads", "32", "--runs", RUNS]),
        "small": measure("small", ["small", f"{DIR}/small", "--runs", RUNS]),
        "mmap": measure("mmap", ["mmap", f"{DIR}/seq.bin", "--runs", RUNS]),
    }
    for f in [x for x in (V["--real"] or "").split(",") if x]:
        r[f] = measure(f"real {f}", ["seq", f, "--runs", RUNS])
    seq = float(r["seq"]["res"].get("mb_s", 0) or 0)
    cpu = r["seq"]["cpu"]
    # INT-W35-BL: no blk: threads since BL-2 and per-CPU vfs.readahead/<cpu> workers since BL-4: the column is
    # blkbench's own user/sys, the system time of all CPUs (/proc/stat) and the read-ahead workers' sum.
    res = r["seq"]["res"]
    ra_ms = sum(v[0] for n, v in cpu.items() if n == "vfs.readahead" or n.startswith("vfs.readahead/"))
    sysm = r["seq"]["sys_ms"]
    blk = "/".join([str(res.get("user_ms", "-")), str(res.get("sys_ms", "-")), str(sysm if sysm is not None else "-"), str(ra_ms)])
    row = " | ".join([
        STATE, f"Mini-PC/{fs}", "fadvise+drop_caches", f"{seq:.0f}", str(ceil or "-"), f"{seq / ceil:.2f}" if ceil else "-",
        f"{float(r['rand1']['res'].get('iops', 0)):.0f} {r['rand1']['res'].get('lat_p99_us', '-')}us", f"{float(r['rand32']['res'].get('iops', 0)):.0f}",
        f"{float(r['small']['res'].get('files_s', 0)):.0f}", f"{float(r['mmap']['res'].get('mb_s', 0)):.0f}", "(desk)", "(desk)", blk,
        f"{r['seq']['copies']:.2f}" if r["seq"]["copies"] is not None else "-",
        f"{int(r['seq']['inflight'] or 0)}/{int(r['seq']['nvme_inflight'] or 0)}",
    ])
    say("table: state | machine/fs | cold | seq MB/s | ceiling MB/s | seq/ceil | rand QD1 IOPS p99 | rand x32 IOPS | small files/s | mmap MB/s | X4 load s | Steam start s | CPU ms bench user/sys, system, ra | copies/byte | inflight_max block/nvme")
    say("row:   " + row)
    save()
    say(f"GREEN: BL-0 measured ({LABEL}); the row is in {os.path.join(OUT, f'blk0-{LABEL}.txt')}")


main()
