#!/usr/bin/env python3
"""DR-4 scenario client (30-libdrm.sh): the static libdrm program on V2 against the fake GPU, over MCP.

Usage: v2_libdrm.py <mcp host port> <token> <unused> <run dir> <fixtures dir>
The program's "Q" lines (AMDGPU_INFO answers through libdrm) must equal the capture of the real 780M line for
line, its "G" lines (amdgpu_query_gpu_info) the derived fixture; device set-up, NORMAL and LOW contexts work and
the kernel logs no unsupported amdgpu ioctl on the way. Exit 0 = green; report.json has the findings."""
import json
import os
import re
import sys

sys.dont_write_bytecode = True
FIX = sys.argv[5]
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "wy"))
import v2_check as v  # noqa: E402
from v2_check import call, need  # noqa: E402

PROBE = "/boot/BITSOS2/drtest/libdrm-info"
PROBE_BO = "/boot/BITSOS2/drtest/libdrm-bo"
PROBE_CS = "/boot/BITSOS2/drtest/libdrm-cs"


def lines(path, tag):
    return [l for l in open(path).read().splitlines() if l.startswith(tag + " ")]


def main():
    c = v.connect(300)
    err, text, _ = call(c, "run", {"path": PROBE, "args": [], "env": v.ENV, "timeout_ms": 60000})
    open(os.path.join(v.RUN, "libdrm-info.out"), "w").write(text)
    out = text.splitlines()
    report = {"head": out[:4]}
    need("amdgpu_device_initialize: 0 (DRM 3.54)" in text, "amdgpu_device_initialize works on the fake", text[-3000:])
    need("amdgpu_query_gpu_info: 0" in text, "amdgpu_query_gpu_info works", text[-3000:])
    got_q = [l for l in out if l.startswith("Q ")]
    want_q = lines(os.path.join(FIX, "780m-info.txt"), "Q")
    diff = [(w, g) for w, g in zip(want_q, got_q) if w != g]
    report["q_lines"] = len(got_q)
    report["q_diff"] = diff[:5]
    need(len(got_q) == len(want_q) and not diff, f"all {len(want_q)} INFO answers equal Linux's on the 780M",
         "\n".join(f"want {w[:120]}\n got {g[:120]}" for w, g in diff[:5]) or f"{len(got_q)} lines")
    got_g = [l for l in out if l.startswith("G ")]
    want_g = lines(os.path.join(FIX, "780m-gpu-info.txt"), "G")
    need(got_g == want_g, "amdgpu_query_gpu_info's fields are the 780M's", "\n".join(["got:"] + got_g + ["want:"] + want_g))
    need(re.search(r"^ctx prio 0: 0$", text, re.M) and re.search(r"^ctx prio -512: 0$", text, re.M),
         "contexts at NORMAL and LOW", text[-1500:])
    need("libdrm-info: ok" in text and re.search(r"ended: exit 0", text), "the program ends with exit 0", text[-1500:])
    # DR-6: the buffer calls, line for line as on CachyOS (fixtures/libdrm-bo.txt).
    _, bo_text, _ = call(c, "run", {"path": PROBE_BO, "args": [], "env": v.ENV, "timeout_ms": 60000})
    open(os.path.join(v.RUN, "libdrm-bo.out"), "w").write(bo_text)
    want_bo = open(os.path.join(FIX, "libdrm-bo.txt")).read().splitlines()
    out_bo = bo_text.splitlines()
    first = next((i for i, l in enumerate(out_bo) if l.startswith("amdgpu_device_initialize")), len(out_bo))
    last = next((i for i, l in enumerate(out_bo) if l.startswith("libdrm-bo:")), len(out_bo) - 1)
    got_bo = out_bo[first:last + 1]
    bo_diff = [(w, g) for w, g in zip(want_bo, got_bo) if w != g]
    report["bo_diff"] = bo_diff[:5]
    need(len(got_bo) == len(want_bo) and not bo_diff, f"all {len(want_bo)} libdrm buffer lines equal CachyOS's",
         "\n".join(f"want {w}\n got {g}" for w, g in bo_diff[:5]) or bo_text[-1500:])
    # DR-8: the command-submission calls, line for line as on CachyOS (fixtures/libdrm-cs.txt).
    _, cs_text, _ = call(c, "run", {"path": PROBE_CS, "args": [], "env": v.ENV, "timeout_ms": 60000})
    open(os.path.join(v.RUN, "libdrm-cs.out"), "w").write(cs_text)
    want_cs = open(os.path.join(FIX, "libdrm-cs.txt")).read().splitlines()
    out_cs = cs_text.splitlines()
    first = next((i for i, l in enumerate(out_cs) if l.startswith("amdgpu_device_initialize")), len(out_cs))
    last = next((i for i, l in enumerate(out_cs) if l.startswith("libdrm-cs:")), len(out_cs) - 1)
    got_cs = out_cs[first:last + 1]
    cs_diff = [(w, g) for w, g in zip(want_cs, got_cs) if w != g]
    report["cs_diff"] = cs_diff[:5]
    need(len(got_cs) == len(want_cs) and not cs_diff, f"all {len(want_cs)} libdrm CS lines equal CachyOS's",
         "\n".join(f"want {w}\n got {g}" for w, g in cs_diff[:5]) or cs_text[-1500:])
    _, log, _ = call(c, "log", {"subsystem": "drm", "max": 400})
    open(os.path.join(v.RUN, "klog-drm.txt"), "w").write(log)
    report["unsupported"] = sorted(set(re.findall(r"ioctl\.unsupported .*?name=(\S+)", log)))
    report["unknown"] = sorted(set(re.findall(r"(info\.unknown_query|ctx\.unknown_op|info\.mmr_refused) .*?key=(\S+)", log)))
    json.dump(report, open(os.path.join(v.RUN, "report.json"), "w"), indent=1)
    need(not report["unsupported"] and not report["unknown"], "no amdgpu ioctl or query the fake does not know", json.dumps(report))
    print(json.dumps(report, indent=1))


if __name__ == "__main__":
    main()
