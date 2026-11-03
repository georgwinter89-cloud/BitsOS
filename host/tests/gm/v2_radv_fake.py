#!/usr/bin/env python3
"""GM-1c scenario client (20-radv-fake.sh): the staged Mesa (RADV, radeonsi) against the fake GPU's render node.

Usage: v2_radv_fake.py <mcp host port> <token> <qmp socket> <run dir> <reference dir or ->
Every Mesa program runs with errlog.so (failed calls) and drmtrace.so (every DRM ioctl, tools/drmtrace.md) preloaded.
T0 `vulkaninfo --summary` and `vulkaninfo` with RADV only: exit 0, the device is the 780M (vendor, device, name from
the tree's amdgpu.ids, integrated, RADV); its properties and INFO answers must match a real 780M's: the reference
dir (vulkaninfo.out, vulkaninfo.info of a Linux run), or with "-" the committed ref-780m/ (vulkaninfo.props,
vulkaninfo.info); a property difference must be one of ref-780m/allow.txt, an INFO answer both runs asked for must
be the same (VRAM_GTT and MEMORY excepted: they follow the carve-out). Hostile: drmhostile under drmtrace (bad
pointers and counts, a 'd' ioctl on /dev/null, fork, a partly unmapped buffer) survives, the trace says so, and
drmreplay plays it and damaged copies of vkcube's trace without a signal.
T1 vkcube --wsi wayland with RADV in bitscomp (linux-dmabuf v4 + syncobj on renderD128): instance, device, swapchain
images taken by bitscomp, up to the first CS (the GPU work the fake cannot draw yet: GX-8); the trace's first CS
with its IBs' PM4 packets is reported.
T2 radeonsi: `eglinfo -B` names it on the platforms that have the node, then eglgears_wayland the same way as T1.
Since CR-GM1c-1 (INT-W28) eglgears_wayland must end with exit 0 and the fake must not hang on any of its CS;
dma-bufs bitscomp refuses must be refused for the known reason (CR-GM1c-2, report.json "known_gaps").
Replay: drmreplay plays vkcube's trace up to its first CS against the node again and must get the same answers.
Exit 0 = green; report.json has the findings (how far each test got)."""
import base64
import json
import os
import re
import sys
import time

sys.dont_write_bytecode = True
HERE = os.path.dirname(os.path.abspath(__file__))
REFDIR = sys.argv[5] if sys.argv[5] != "-" else os.path.join(HERE, "ref-780m")
sys.path.insert(0, os.path.join(HERE, "..", "wy"))
sys.path.insert(0, os.path.join(HERE, "tools"))
sys.argv = sys.argv[:5] + ["-"]
import v2_check as v  # noqa: E402
from v2_check import Qmp, call, ctl, need  # noqa: E402
import drmtrace  # noqa: E402

T = "/boot/BITSOS2/mesa"
G = "/boot/BITSOS2/gmtest"
WORK = "/tmp/gmr"
ENV = dict(v.ENV, HOME=f"{WORK}/home", LD_PRELOAD=f"{G}/errlog.so {G}/drmtrace.so")
RADV = {"MESA_VK_DRIVERS": "radeon_icd"}
# A Radeon 780M as RADV names it (vulkaninfo of the Mini-PC on Linux; device and name: the tree's amdgpu.ids).
FACTS = {"vendorID": "0x1002", "deviceID": "0x1900", "deviceType": "PHYSICAL_DEVICE_TYPE_INTEGRATED_GPU",
         "deviceName": "AMD Radeon 780M Graphics (RADV PHOENIX)", "driverID": "DRIVER_ID_MESA_RADV"}
# Keys of a full vulkaninfo that differ between runs, kernels or Mesa versions, or with the carve-out size.
VOLATILE = re.compile(r"Version|driverInfo|UUID|LUID|pciBus|pciDevice|pciDomain|pciFunction|primary|render|size|budget|usage|"
                      r"Identifier|timestampPeriod|maxMemoryAllocationSize|maxBufferSize|deviceNodeMask")
# The INFO queries whose answers follow the carve-out and the RAM: VRAM_GTT, MEMORY (compared only by record).
INFO_SIZES = (0x14, 0x19)
REFUSAL = re.compile(r"dmabuf: pixman refuses [^\n]*?PermissionDenied[^\n]*?\(refusal")
REPORT = {"known_gaps": []}
_serial_off = 0
ALL_EVENTS = []


def mesa(c, prog, args, tag, extra=None, timeout_ms=180000):
    env = dict(ENV, ERRLOG_FILE=f"{WORK}/err-{tag}.log", DRMTRACE_FILE=f"{WORK}/trace-{tag}-%p.txt", **(extra or {}))
    _, text, _ = call(c, "run", {"path": "/bin/sh", "args": [f"{T}/run", prog, *args], "env": env, "timeout_ms": timeout_ms})
    open(os.path.join(v.RUN, f"{tag}.out"), "w").write(text)
    return text


def sh(c, script, timeout_ms=30000):
    _, text, _ = call(c, "run", {"path": "/bin/sh", "args": ["-c", script], "env": v.ENV, "timeout_ms": timeout_ms})
    return text


def ended(text):
    m = re.search(r"ended: (exit|signal) (\d+)", text)
    return (m.group(1), int(m.group(2))) if m else None


def fetch(c, path):
    """A file of V2, read in pieces of read_file's 2 MiB until its `eof`; None if it is not there. An error after the
    first piece fails the check: a log cut short must not pass as the whole one."""
    out, off = b"", 0
    while True:
        err, text, _ = call(c, "read_file", {"path": path, "offset": off, "length": 2 << 20})
        if err:
            need(not out, f"read_file {path} goes on at offset {off}", text)
            return None
        j = json.loads(text)
        piece = j["text"].encode() if "text" in j else base64.b64decode(j["data"])
        out += piece
        if j.get("eof", True):
            return out.decode("utf-8", "replace")
        nxt = int(j.get("next_offset", off + len(piece)))
        need(nxt > off, f"read_file {path} moves on (offset {off})", text[:300])
        off = nxt


def trace_of(c, tag):
    """The biggest trace of a program run (the shell and the loader leave empty ones), parsed, and saved."""
    name = sh(c, f"ls -S {WORK}/trace-{tag}-[0-9]*.txt 2>/dev/null | head -1").strip().splitlines()
    path = next((l for l in name if l.startswith(WORK)), None)
    text = fetch(c, path) if path else None
    if not text:
        return []
    open(os.path.join(v.RUN, f"trace-{tag}.txt"), "w").write(text)
    return drmtrace.parse(text)


def klog_new(c):
    """drm events since the last call, from the serial log (it has every level; MCP `log` keeps only the newest
    records, and a frame's gem/dma_buf events push the CS ones out of its ring)."""
    global _serial_off
    del c
    with open(os.path.join(v.RUN, "serial.log"), "rb") as f:
        f.seek(_serial_off)
        data = f.read()
    cut = data.rfind(b"\n") + 1
    _serial_off += cut
    new = [l for l in data[:cut].decode("utf-8", "replace").splitlines() if " drm " in l]
    ALL_EVENTS.extend(new)
    return new


def drm_events(lines):
    t = "\n".join(lines)
    return {"cs_submits": len(re.findall(r"cs\.submit", t)),
            "cs_refused": sorted(set(re.findall(r"cs\.refused .*?errno=(\S+)", t))),
            "fake_cs": sorted(set(re.findall(r"fake\.\w+(?: why=\S+)?", t))),
            "unsupported": sorted(set(re.findall(r"ioctl\.unsupported .*?name=(\S+)", t))),
            "invalid": sorted(set(re.findall(r"ioctl\.invalid .*?nr=(\S+)", t))),
            "unknown": sorted(set(re.findall(r"(info\.unknown_query|ctx\.unknown_op|info\.mmr_refused) .*?key=(\S+)", t))),
            "vm_refused": sorted(set(re.findall(r"vm\.(?:op_refused|bad_va|update_failed|create_failed)[^\n]*", t)))[:10]}


def props(text):
    """{section/key: value} of vulkaninfo's "key = value" lines, the section the last line ending in ':' at column 0."""
    out, sec = {}, ""
    for l in text.splitlines():
        if re.match(r"^[A-Za-z][\w ]*:\s*$", l):
            sec = l.strip()[:-1]
            continue
        m = re.match(r"^\s+(\w[\w\[\]. ]*?)\s+=\s+(.*)$", l)
        if m and sec:
            out[f"{sec}/{m.group(1)}"] = m.group(2).strip()
    return out


def ref_props():
    """The reference's properties: a real run's vulkaninfo.out, or the committed vulkaninfo.props; None if neither."""
    out = os.path.join(REFDIR, "vulkaninfo.out")
    if os.path.exists(out):
        return props(open(out, errors="replace").read())
    lst = os.path.join(REFDIR, "vulkaninfo.props")
    if os.path.exists(lst):
        return dict(l.rstrip("\n").split(" = ", 1) for l in open(lst) if " = " in l and not l.startswith("#"))
    return None


def allowed():
    """ref-780m/allow.txt: {key: (here, real)} of the known differences."""
    out = {}
    for l in open(os.path.join(HERE, "ref-780m", "allow.txt")):
        if l.startswith("#") or "|" not in l:
            continue
        k, here, real = (x.strip() for x in l.split("|")[:3])
        out[k] = (here, real)
    return out


def device_block(text):
    m = re.search(r"GPU0:\n(.*?)(?:\nGPU1:|\Z)", text, re.S)
    return dict(re.findall(r"^\s+(\w+)\s+= (.*)$", m.group(1), re.M)) if m else {}


def windows(c, sock):
    return [(int(m.group(1)), m.group(2)) for m in re.finditer(r" id=(\d+) app_id=(\S*)", ctl_on(c, sock, "list"))]


def ctl_on(c, sock, *words):
    _, text, _ = call(c, "run", {"path": "/bin/bitscomp", "args": ["ctl", "--socket", sock, *words], "env": v.ENV, "timeout_ms": 20000})
    return text


def t0(c):
    text = mesa(c, "vulkaninfo", ["--summary"], "summary", RADV)
    dev = device_block(text)
    REPORT["t0_summary"] = {"ended": ended(text), "device": dev}
    need(ended(text) == ("exit", 0), "T0: vulkaninfo --summary with RADV ends with exit 0", text[-3000:])
    need("amdgpu.ids" not in text, "T0: libdrm finds amdgpu.ids in the tree (AMDGPU_ASIC_ID_TABLE_PATHS)", text[-2000:])
    need("GPU1:" not in text, "T0: RADV lists one device (the fake's node)", text[-2000:])
    wrong = {k: (dev.get(k), want) for k, want in FACTS.items() if dev.get(k) != want}
    need(not wrong, "T0: the device is a Radeon 780M as on Linux (vendor, device, name, integrated, RADV)", json.dumps(wrong))
    print(f"T0: {dev.get('deviceName')} {dev.get('driverInfo')} api {dev.get('apiVersion')}", flush=True)
    recs = trace_of(c, "summary")
    REPORT["t0_trace"] = drmtrace.summary(recs)
    need(any(r["kind"] == "I" for r in recs), "T0: drmtrace saw vulkaninfo's DRM ioctls", "\n".join(REPORT["t0_trace"]))

    text = mesa(c, "vulkaninfo", [], "vulkaninfo", RADV)
    need(ended(text) == ("exit", 0), "T0: vulkaninfo (all properties) with RADV ends with exit 0", text[-3000:])
    mine = props(text)
    REPORT["t0_properties"] = len(mine)
    theirs = ref_props()
    if theirs is None:
        print(f"SKIP: T0 property comparison: no vulkaninfo.out or vulkaninfo.props in {REFDIR}", flush=True)
        REPORT["t0_vs_real"] = f"SKIP: no reference in {REFDIR}"
    else:
        keys = [k for k in theirs if k in mine and not VOLATILE.search(k.split("/")[-1]) and not k.startswith("Presentable")]
        diff = {k: (mine[k], theirs[k]) for k in keys if mine[k] != theirs[k]}
        allow = allowed()
        new = {k: d for k, d in diff.items() if allow.get(k) != d}
        gone = sorted(k for k in allow if k not in diff)
        REPORT["t0_vs_real"] = {"reference": REFDIR, "compared": len(keys), "differ": len(diff), "not_allowed": new,
                                "allowed_gone": gone, "only_here": len(set(mine) - set(theirs)),
                                "only_real": len(set(theirs) - set(mine)), "differences": diff}
        print(f"T0: {len(keys)} properties compared with a real 780M: {len(diff)} differ, {len(new)} of them not in allow.txt",
              flush=True)
        for k in gone:
            print(f"NOTE: T0: the known difference {k} is gone: its allow.txt line can go", flush=True)
        need(len(keys) >= 500, f"T0: the reference has the properties to compare ({len(keys)})")
        need(not new, "T0: every property differing from the real 780M is a known one (ref-780m/allow.txt)", json.dumps(new, indent=1))
    info = os.path.join(REFDIR, "vulkaninfo.info")
    if not os.path.exists(info):
        print(f"SKIP: T0 INFO comparison: no vulkaninfo.info in {REFDIR}", flush=True)
        REPORT["t0_info_vs_real"] = f"SKIP: no vulkaninfo.info in {REFDIR}"
    else:
        ref = {k: a for k, a in drmtrace.read_info(open(info).read()).items() if k[0] not in INFO_SIZES}
        mine_info = {k: a for k, a in drmtrace.info_answers(recs).items() if k[0] not in INFO_SIZES}
        lines = drmtrace.compare_info(mine_info, ref)
        REPORT["t0_info_vs_real"] = lines
        print("T0: INFO answers against a real 780M:", flush=True)
        for l in lines:
            print("  " + l, flush=True)
        equal = [l for l in lines if l.endswith(": equal")]
        need(len(equal) >= 5, f"T0: vulkaninfo and the reference share INFO queries ({len(equal)} equal)", "\n".join(lines))
        need(not drmtrace.info_differs(lines), "T0: every INFO answer both runs asked for is the real 780M's",
             "\n".join(drmtrace.info_differs(lines)))
    ev = drm_events(klog_new(c))
    REPORT["t0_events"] = ev
    need(not ev["unsupported"] and not ev["unknown"], "T0: every ioctl and INFO query of vulkaninfo is answered", json.dumps(ev))


class Prog:
    """A Mesa window program that keeps running (MCP `run` with a short timeout gives its call number)."""

    def __init__(self, c, prog, args, tag, extra):
        self.c, self.tag = c, tag
        env = dict(ENV, ERRLOG_FILE=f"{WORK}/err-{tag}.log", DRMTRACE_FILE=f"{WORK}/trace-{tag}-%p.txt",
                   DRMTRACE_MAX_CS="3", **extra)
        _, text, _ = call(c, "run", {"path": "/bin/sh", "args": [f"{T}/run", prog, *args], "env": env, "timeout_ms": 1500})
        m = re.search(r"\(call (\d+)\)", text)
        self.n, self.text = (int(m.group(1)) if m else None), text

    def poll(self):
        if self.n is not None:
            _, text, _ = call(self.c, "output", {"call": self.n})
            self.text += "\n" + text
        open(os.path.join(v.RUN, f"{self.tag}.out"), "w").write(self.text)
        return self.text

    def stop(self, sock):
        if ended(self.poll()):
            return ended(self.text)
        ctl_on(self.c, sock, "close")
        end = time.time() + 15
        while not ended(self.poll()) and time.time() < end:
            time.sleep(0.5)
        if not ended(self.text) and self.n is not None:
            call(self.c, "stop", {"call": self.n})
            time.sleep(2)
            self.n = None
            return ("stopped", 0)
        return ended(self.text)


def dmabuf_counts(stats):
    """(imported, failed) of a `bitscomp ctl stats` answer."""
    return tuple(int((re.search(rf"dmabuf_{k}=(\d+)", stats) or [0, 0])[1]) for k in ("imported", "failed"))


def to_first_cs(c, q, sock, comp, prog, args, tag, extra, label):
    """Runs a window program until its first CS (or its end, or 60 s); what it reached goes into REPORT[tag]. The
    program is stopped also when a check in between fails, so the next test never finds the node's VMID taken."""
    klog_new(c)
    events = []
    before = {w for w, _ in windows(c, sock)}
    comp_from = len(comp.poll())
    # bitscomp's dma-buf counters count from its start: this program's are the difference.
    base = dmabuf_counts(ctl_on(c, sock, "stats"))
    p = Prog(c, prog, args, tag, extra)
    how = None
    try:
        t0_, first_cs, win = time.time(), None, None
        while time.time() - t0_ < 60:
            if ended(p.poll()):
                break
            new = [w for w in windows(c, sock) if w[0] not in before]
            win = win or (new[0] if new else None)
            events += klog_new(c)
            n_cs = sh(c, f"cat {WORK}/trace-{tag}-[0-9]*.txt 2>/dev/null | grep -c ' nr=0x44 '")
            if re.search(r"^[1-9]\d*$", n_cs, re.M) and first_cs is None:
                first_cs = round(time.time() - t0_, 1)
            if first_cs is not None and time.time() - t0_ > first_cs + 3:
                break
            time.sleep(0.5)
        stats = ctl_on(c, sock, "stats")
        q.screendump(f"-{tag}")
    finally:
        how = p.stop(sock)
    recs = trace_of(c, tag)
    cs = drmtrace.cs_list(recs)
    events += klog_new(c)
    ev = drm_events(events)
    r = {"ended": how, "window": win, "first_cs_s": first_cs, "trace_cs": len(cs),
         "first_cs": drmtrace.describe_cs(cs[0]) if cs else None, "trace": drmtrace.summary(recs),
         "dmabuf_imported": dmabuf_counts(stats)[0] - base[0], "dmabuf_failed": dmabuf_counts(stats)[1] - base[1],
         "events": ev, "tail": [l for l in p.text.splitlines() if not re.match(r"call \d+ run: running", l)][-12:],
         "bitscomp_refusals": len(REFUSAL.findall(comp.poll()[comp_from:])),
         "bitscomp_refusal_lines": sorted(set(re.findall(r"dmabuf: (pixman refuses [^\n]*?)\(refusal", comp.poll()[comp_from:])))}
    REPORT[tag] = r
    print(f"{label}: window {win}, dma-bufs taken by bitscomp {r['dmabuf_imported']} (refused {r['dmabuf_failed']}), "
          f"first CS after {first_cs} s, {len(cs)} CS in the trace, ended {how}", flush=True)
    for l in r["first_cs"] or []:
        print("  " + l, flush=True)
    return r, recs


def first_cs_ok(r, recs, label):
    cs = drmtrace.cs_list(recs)
    need(cs and cs[0]["ret"] == 0, f"{label}: the first CS was taken (ret 0)", "\n".join(r["trace"]))


def refusals_known(r, label, vram_images=False):
    """Every dma-buf bitscomp refused, refused for the known reason: a NO_CPU_ACCESS buffer Pixman cannot map
    (EPERM as on Linux, CR-GM1c-2). A change in bitscomp (a GPU renderer, or no main device) shows up here."""
    need(r["dmabuf_failed"] == r["bitscomp_refusals"],
         f"{label}: bitscomp refused {r['dmabuf_failed']} dma-bufs, all for the known reason (EPERM mapping, CR-GM1c-2)",
         json.dumps({k: r[k] for k in ("dmabuf_imported", "dmabuf_failed", "bitscomp_refusals", "bitscomp_refusal_lines")}))
    if r["dmabuf_failed"]:
        REPORT["known_gaps"].append(f"{label}: {r['dmabuf_failed']} dma-bufs refused by bitscomp (CR-GM1c-2)")
    # RADV's swapchain images are VRAM with NO_CPU_ACCESS: Pixman refuses them today.
    if vram_images and (r["dmabuf_failed"] == 0 or r["window"]):
        print(f"NOTE: {label}: bitscomp took the images (window {r['window']}): CR-GM1c-2's behaviour changed", flush=True)


def t1(c, q, sock, comp):
    r, recs = to_first_cs(c, q, sock, comp, "vkcube", ["--wsi", "wayland"], "vkcube", RADV, "T1 vkcube")
    tail = "\n".join(r["tail"])
    need(r["ended"] is None or r["ended"][0] != "signal", f"T1: vkcube ends without a signal ({r['ended']})", tail)
    need(not re.search(r"Cannot connect to wayland|vkCreate\w+ failed|VK_ERROR", tail),
         "T1: vkcube made its instance, device and swapchain on bitscomp's Wayland", tail)
    need(r["dmabuf_imported"] + r["dmabuf_failed"] >= 1, "T1: RADV sent its swapchain images to bitscomp as dma-bufs", json.dumps(r))
    refusals_known(r, "T1 vkcube", vram_images=True)
    first_cs_ok(r, recs, "T1 vkcube")
    need(not r["events"]["fake_cs"] and not r["events"]["cs_refused"], "T1: the fake ran vkcube's first CS without a hang",
         json.dumps(r["events"]))
    return recs


def hang_opcodes(events):
    """The PM4 opcodes of the fake's Unsupported hangs in kernel log lines."""
    return sorted({(int(h) >> 8) & 0xff for h in re.findall(r"fake\.cs_hang why=\"Hang\(Unsupported \{ header: (\d+) \}\)", "\n".join(events))})


def t2(c, q, sock, comp):
    text = mesa(c, "eglinfo", ["-B"], "eglinfo", {})
    plats = {}
    for blk in re.split(r"\n(?=\w[\w ]* platform:)", text):
        m = re.match(r"(\w[\w ]*) platform:", blk)
        if m:
            rend = re.search(r"OpenGL (?:core profile |ES profile )?renderer: (.*)", blk)
            plats[m.group(1)] = rend.group(1) if rend else None
    REPORT["t2_eglinfo"] = {"ended": ended(text), "renderers": plats}
    print(f"T2 eglinfo -B: {json.dumps(plats)}", flush=True)
    need(ended(text) is not None and ended(text)[0] == "exit", "T2: eglinfo -B ends without a signal", text[-3000:])
    need(any(r and "radeonsi" in r for r in plats.values()), "T2: radeonsi initialises on the fake's node (eglinfo -B)", text[-3000:])
    mark = len(ALL_EVENTS)
    r, recs = to_first_cs(c, q, sock, comp, "eglgears_wayland", [], "eglgears", {}, "T2 eglgears_wayland")
    ops = hang_opcodes(ALL_EVENTS[mark:])
    r["hang_opcodes"] = [f"0x{o:02x} {drmtrace.PM4.get(o, '?')}" for o in ops]
    tail = "\n".join(r["tail"])
    info = [x for x in recs if x["kind"] == "I" and x["nr"] == 0x45]
    need(info, "T2: eglgears_wayland opened the node (radeonsi, not llvmpipe)", "\n".join(r["trace"]))
    first_cs_ok(r, recs, "T2 eglgears_wayland")
    need(r["dmabuf_imported"] + r["dmabuf_failed"] >= 1, "T2: radeonsi sent its window buffers to bitscomp as dma-bufs", json.dumps(r))
    refusals_known(r, "T2 eglgears_wayland")
    # CR-GM1c-1 is done (INT-W28: GR's interpreter runs DMA_DATA, COPY_DATA, the draws and the _INDEX forms): no
    # fake hang on any packet, and radeonsi ends normally when bitscomp closes its window.
    hangs = [e for e in r["events"]["fake_cs"] if e.startswith("fake.cs_hang")]
    need(not ops and not hangs, "T2: the fake ran every CS of eglgears_wayland without a hang",
         json.dumps({"hang_opcodes": r["hang_opcodes"], "events": r["events"]}))
    need(r["ended"] == ("exit", 0), f"T2: eglgears_wayland ends with exit 0 ({r['ended']})", tail)


def replay(c):
    """drmreplay plays vkcube's trace up to its first CS once more: the same answers, the CS taken again."""
    found = [l for l in sh(c, f"ls -S {WORK}/trace-vkcube-[0-9]*.txt").splitlines() if l.startswith(WORK)]
    need(found, "replay: vkcube's trace is there")
    _, text, _ = call(c, "run", {"path": "/bin/sh", "args": [f"{T}/run", f"{G}/drmreplay", "--until-cs", "1", found[0]],
                                 "env": dict(v.ENV, HOME=f"{WORK}/home"), "timeout_ms": 120000})
    open(os.path.join(v.RUN, "replay.out"), "w").write(text)
    m = re.search(r"replay: (\d+) ioctls, (\d+) same, (\d+) differ, (\d+) skipped, (\d+) CS, (\d+) damaged", text)
    REPORT["replay"] = {"ended": ended(text), "summary": m.group(0) if m else None, "tail": text.splitlines()[-15:]}
    print(f"replay: {REPORT['replay']['summary']}", flush=True)
    need(ended(text) == ("exit", 0) and m and int(m.group(5)) >= 1 and int(m.group(3)) == 0 and int(m.group(6)) == 0,
         "replay: vkcube's ioctls up to its first CS give the same answers again", text[-3000:])
    return found[0]


def damaged_replays(c, trace_path):
    """drmreplay on two damaged copies of vkcube's trace (an IB chunk's data cut in half, bad hex in a GEM_CREATE):
    a damaged count and exit 1, never a signal; drmtrace.py lists the damaged lines (no traceback)."""
    text = open(os.path.join(v.RUN, "trace-vkcube.txt")).read().splitlines()
    cut, garbled = list(text), list(text)
    i = next(k for k, l in enumerate(cut) if re.match(r"P \d+ chunk \d+ id=1 len_dw=\d+ data=", l))
    head, data = cut[i].split("data=", 1)
    cut[i] = head + "data=" + data[:len(data) // 2 // 2 * 2]
    j = next(k for k, l in enumerate(garbled) if re.match(r"I \d+ .* nr=0x40 ", l))
    garbled[j] = re.sub(r" in=(..)", " in=zz", garbled[j], count=1)
    for name, lines, why in (("cut", cut, "chunk data shorter than len_dw"), ("garbled", garbled, "not the request's size")):
        body = "\n".join(lines) + "\n"
        local = os.path.join(v.RUN, f"trace-vkcube-{name}.txt")
        open(local, "w").write(body)
        recs = drmtrace.parse(body)
        if name == "garbled":
            need(drmtrace.damaged(recs), "drmtrace.py: the garbled trace has a damaged line (no traceback)")
        path = f"{WORK}/trace-damaged-{name}.txt"
        err, t, _ = call(c, "write_file", {"path": path, "text": body})
        need(not err, f"replay: the {name} trace is on V2", t)
        _, out, _ = call(c, "run", {"path": "/bin/sh", "args": [f"{T}/run", f"{G}/drmreplay", "--until-cs", "1", path],
                                    "env": dict(v.ENV, HOME=f"{WORK}/home"), "timeout_ms": 120000})
        open(os.path.join(v.RUN, f"replay-{name}.out"), "w").write(out)
        m = re.search(r"replay: .*, (\d+) damaged", out)
        REPORT[f"replay_{name}"] = {"ended": ended(out), "summary": m.group(0) if m else None}
        print(f"replay of the {name} trace: {REPORT[f'replay_{name}']['summary']} ({ended(out)})", flush=True)
        need(ended(out) == ("exit", 1) and m and int(m.group(1)) >= 1 and why in out,
             f"replay: the {name} trace is reported damaged ({why}), exit 1, no signal", out[-3000:])


def hostile(c):
    """drmhostile under drmtrace on the fake's node: every check of the program passes (it gets errors, no signal),
    the trace has the unreadable payloads instead of a crash, nothing of /dev/null, each fork child its own file,
    the IB of the partly unmapped buffer as the program wrote it; drmreplay plays the trace without a difference."""
    sh(c, f"rm -f {WORK}/trace-hostile-*.txt")
    text = mesa(c, f"{G}/drmhostile", [], "hostile", timeout_ms=120000)
    REPORT["hostile"] = {"ended": ended(text), "lines": [l for l in text.splitlines() if l.startswith("drmhostile:")]}
    need(ended(text) == ("exit", 0) and re.search(r"drmhostile: ok \d+ checks", text),
         "hostile: drmhostile's checks pass under drmtrace (bad pointers and counts, /dev/null, fork, partial munmap)",
         text[-3000:])
    files = [l for l in sh(c, f"ls -S {WORK}/trace-hostile-[0-9]*.txt").splitlines() if l.startswith(WORK)]
    main_path = files[0]
    tr = fetch(c, main_path) or ""
    open(os.path.join(v.RUN, "trace-hostile.txt"), "w").write(tr)
    recs = drmtrace.parse(tr)
    need(not drmtrace.damaged(recs), "hostile: the trace has no damaged line", json.dumps(drmtrace.damaged(recs)[:3]))
    need(re.search(r"^U \d+ fd=\d+ nr=0x45 ", tr, re.M), "hostile: INFO with a bad argument pointer is a U record")
    need(re.search(r"^P \d+ chunks n=1 unreadable$", tr, re.M), "hostile: a bad chunk array is 'unreadable'")
    need(re.search(r"^P \d+ chunk 0 unreadable$", tr, re.M), "hostile: a bad chunk pointer is 'unreadable'")
    need(re.search(r"^P \d+ chunk 0 id=1 len_dw=1073741824 unreadable$", tr, re.M),
         "hostile: a chunk of 2^30 dwords is 'unreadable' (no 32-bit wrap)")
    need(re.search(r"^P \d+ bolist - size=8 n=2147483647 unreadable$", tr, re.M), "hostile: a BO list of 2^31 entries is 'unreadable'")
    need(re.search(r"^P \d+ fences n=2147483647 unreadable$", tr, re.M), "hostile: 2^31 fences are 'unreadable'")
    need(re.search(r"^P \d+ handles n=1073741824 unreadable$", tr, re.M), "hostile: 2^30 syncobj handles are 'unreadable'")
    null = re.search(r"drmhostile: devnull fd=(\d+)", text)
    need(null and not re.search(rf"^[IU] \d+ fd={null.group(1)} ", tr, re.M), "hostile: the 'd' ioctl on /dev/null is not traced")
    ib = re.search(r"drmhostile: ib va=(0x[0-9a-f]+) data=([0-9a-f]+)", text)
    got = re.search(rf"^P \d+ ib 1 va={ib.group(1)} ip=0 ring=0 flags=0x0 bytes=64 data=([0-9a-f]+)$", tr, re.M) if ib else None
    need(got and got.group(1) == ib.group(2), "hostile: the IB in a buffer whose front and tail were unmapped is traced as written",
         ib.group(0) if ib else text[-1000:])
    kids = [f for f in files[1:] if "trace-hostile-" in f]
    sizes = sh(c, f"for f in {' '.join(kids)}; do grep -c '^I ' $f; done") if kids else ""
    ones = re.findall(r"^1$", sizes, re.M)
    REPORT["hostile"]["fork_child_traces"] = len(ones)
    need(len(ones) >= 32, f"hostile: each fork child wrote its one DRM call to its own trace ({len(ones)} of 32)", sizes[-500:])
    _, out, _ = call(c, "run", {"path": "/bin/sh", "args": [f"{T}/run", f"{G}/drmreplay", main_path],
                                "env": dict(v.ENV, HOME=f"{WORK}/home"), "timeout_ms": 120000})
    open(os.path.join(v.RUN, "replay-hostile.out"), "w").write(out)
    m = re.search(r"replay: (\d+) ioctls, (\d+) same, (\d+) differ, (\d+) skipped, (\d+) CS, (\d+) damaged", out)
    REPORT["hostile"]["replay"] = m.group(0) if m else out[-500:]
    print(f"hostile replay: {REPORT['hostile']['replay']}", flush=True)
    need(ended(out) == ("exit", 0) and m and int(m.group(3)) == 0 and int(m.group(6)) == 0 and int(m.group(5)) >= 1,
         "hostile: drmreplay plays the hostile trace (bad calls skipped, the NOP CS again, nothing differs)", out[-3000:])


def main():
    c = v.connect(300)
    q = Qmp(v.QMP)
    sh(c, f"mkdir -p {WORK}/home && rm -f {WORK}/err-*.log {WORK}/trace-*.txt")
    out = sh(c, "ls -l /dev/dri")
    need("renderD128" in out, "/dev/dri/renderD128 (drm=fake)", out)
    text = mesa(c, "--check", [], "check", RADV)
    need("render nodes: renderD128" in text and "resolve inside" in text, "run --check: the tree resolves, the node is there", text)
    klog_new(c)
    t0(c)
    hostile(c)
    klog_new(c)

    sock = "wayland-3"
    comp = v.Job(c, "/bin/bitscomp", ["--socket", sock, "--stats", "5"], sock=sock)
    comp.wait_for(r"backend=fbdev size=1280x800", "bitscomp started on fbdev at 1280x800")
    comp.wait_for(r"dmabuf: zwp_linux_dmabuf_v1 for renderer \w+, main device 226:128 \(/dev/dri/renderD128\)",
                  "bitscomp names the fake's node as the main device of linux-dmabuf")
    REPORT["bitscomp_syncobj"] = "wp_linux_drm_syncobj_manager_v1 on /dev/dri/renderD128" in comp.poll()
    global ENV
    ENV = dict(ENV, WAYLAND_DISPLAY=sock)
    failures = []
    for name, fn in (("T1", lambda: t1(c, q, sock, comp)), ("T2", lambda: t2(c, q, sock, comp))):
        try:
            fn()
        except SystemExit as e:
            failures.append(f"{name}: {str(e).splitlines()[0]}")
            print(str(e)[:3000], flush=True)
    try:
        damaged_replays(c, replay(c))
    except SystemExit as e:
        failures.append(f"replay: {str(e).splitlines()[0]}")
        print(str(e)[:3000], flush=True)
    ctl_on(c, sock, "quit")
    open(os.path.join(v.RUN, "bitscomp.out"), "w").write(comp.poll())

    REPORT["events_all"] = drm_events(ALL_EVENTS)
    _, text, _ = call(c, "log", {"level": "info", "max": 2000})
    open(os.path.join(v.RUN, "klog-info.txt"), "w").write(text)
    REPORT["kernel_gaps"] = sorted({re.sub(r"^\S+\s+\[[^]]*\]\s*", "", l) for l in text.splitlines()
                                    if re.search(r"syscall (enosys|enotty|enoprotoopt|nr\.invalid)", l)})
    errs = {}
    for tag in ("summary", "vulkaninfo", "vkcube", "eglinfo", "eglgears"):
        t = fetch(c, f"{WORK}/err-{tag}.log") or ""
        open(os.path.join(v.RUN, f"err-{tag}.log"), "w").write(t)
        errs[tag] = sorted({re.sub(r"arg=0x[0-9a-f]+", "arg=*", l) for l in t.splitlines() if l.startswith("errlog ")})[:40]
    REPORT["failed_calls"] = errs
    json.dump(REPORT, open(os.path.join(v.RUN, "report.json"), "w"), indent=1)
    print(f"report: {os.path.join(v.RUN, 'report.json')}")
    if failures:
        raise SystemExit("RED: " + "; ".join(failures))


if __name__ == "__main__":
    main()
