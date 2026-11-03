#!/usr/bin/env python3
"""DR-2 scenario client (20-radv.sh): the render node of the fake GPU met by real libdrm and real RADV, over MCP.

Usage: v2_radv.py <mcp host port> <token> <qmp socket> <run dir> <host reference dir>
1. /dev/dri/renderD128 and /sys/class/drm exist; 2. drmprobe (libdrm's drmGetDevices2/drmGetDevice2/drmGetVersion
through the staged Mesa tree's loader) prints the same "drm:" lines as on CachyOS; 3. `run --check` with RADV only
lists the node; 4. `vulkaninfo --summary` and `vulkaninfo` with RADV only (MESA_VK_DRIVERS=radeon_icd) end with an
exit code, never a signal, and name no crash; 5. the kernel's `drm` events name the first ioctl the fake does not
answer. DR-4: RADV gets through the amdgpu winsys set-up (INFO, CTX: no unknown query, no unsupported INFO/CTX)
and stops at its first buffer (vkCreateDevice: GEM_CREATE is the next ioctl); lavapipe alone still lists itself
(with RADV beside it vulkaninfo now stops at RADV's vkCreateDevice, recorded only). DR-7: with GEM (DR-6) and the
per-fd GPU VM RADV gets past memory allocation: vulkaninfo ends with 0 and no "Failed to allocate", GEM_VA ran
(the drm `vm.created` event), no GEM or VM ioctl is unsupported; the next unsupported ioctl is recorded (none for
vulkaninfo: the next one, CS, comes with the first queue submit). DR-8: CS, WAIT_CS and WAIT_FENCES are answered;
vkcube with RADV is tried (Wayland and VK_KHR_display) and recorded only: how it ended, the CS jobs the fake ran and
the next unsupported ioctl. Exit 0 = green; report.json has the findings."""
import json
import os
import re
import sys

sys.dont_write_bytecode = True
HOSTREF = sys.argv[5]
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "wy"))
import v2_check as v  # noqa: E402
from v2_check import call, need  # noqa: E402

T = "/boot/BITSOS2/mesa"
PROBE = "/boot/BITSOS2/drtest/drmprobe"
WORK = "/tmp/dr"
ENV = dict(v.ENV, HOME=f"{WORK}/home")
RADV = dict(ENV, MESA_VK_DRIVERS="radeon_icd")
REPORT = {}


def run(c, args, env, timeout_ms=180000, tag=None):
    err, text, _ = call(c, "run", {"path": "/bin/sh", "args": args, "env": env, "timeout_ms": timeout_ms})
    if tag:
        open(os.path.join(v.RUN, f"{tag}.out"), "w").write(text)
    return err, text


def ended(text):
    m = re.search(r"ended: (exit|signal) (\d+)", text)
    return (m.group(1), int(m.group(2))) if m else None


def main():
    c = v.connect(300)
    run(c, ["-c", f"mkdir -p {WORK}/home"], ENV)

    _, text = run(c, ["-c", "ls -l /dev/dri; cat /sys/class/drm/version; ls /sys/class/drm"], ENV, tag="node")
    need("renderD128" in text and "226, 128" in text.replace("226,128", "226, 128"), "/dev/dri/renderD128 is char 226:128", text)
    need("drm 1.1.0 20060810" in text, "/sys/class/drm/version", text)

    _, text = run(c, [f"{T}/run", PROBE], ENV, tag="drmprobe")
    v2 = [l for l in text.splitlines() if l.startswith("drm: ")]
    host = [l for l in open(os.path.join(HOSTREF, "drmprobe.txt")).read().splitlines() if l.startswith("drm: ")]
    REPORT["drmprobe_v2"] = v2
    REPORT["drmprobe_amdgpu"] = [l for l in text.splitlines() if l.startswith("amdgpu: ")]
    need(v2 == host, "libdrm's discovery says on V2 what it says on CachyOS", "\n".join(["V2:"] + v2 + ["CachyOS:"] + host))
    need("amdgpu: amdgpu_device_initialize = 0 (ok)" in text, "libdrm_amdgpu's device set-up works (DR-4)", text[-2000:])

    _, text = run(c, [f"{T}/run", "--check"], RADV, tag="check")
    need("render nodes: renderD128" in text, "run --check sees the render node", text)

    for tag, args in (("vulkaninfo-summary", ["vulkaninfo", "--summary"]), ("vulkaninfo", ["vulkaninfo"])):
        _, text = run(c, [f"{T}/run", *args], RADV, tag=tag)
        how = ended(text)
        REPORT[tag] = {"ended": how, "tail": text.splitlines()[-25:]}
        need(how is not None and how[0] == "exit", f"{tag} with RADV ends with an exit code, not a signal ({how})", text[-3000:])
        need(not re.search(r"Segmentation fault|SIGSEGV|core dumped|Aborted", text), f"{tag}: no crash", text[-3000:])

    # The software drivers must not suffer from the fake GPU: lavapipe alone, and EGL with radeonsi beside llvmpipe.
    _, text = run(c, [f"{T}/run", "vulkaninfo", "--summary"], dict(ENV, MESA_VK_DRIVERS="lvp_icd"), tag="vulkaninfo-lvp")
    REPORT["vulkaninfo_lvp"] = {"ended": ended(text), "lavapipe": "llvmpipe" in text}
    need(ended(text) == ("exit", 0) and "llvmpipe" in text, "vulkaninfo with lavapipe lists lavapipe", text[-3000:])
    _, text = run(c, [f"{T}/run", "vulkaninfo", "--summary"], ENV, tag="vulkaninfo-both")
    REPORT["vulkaninfo_both"] = {"ended": ended(text), "lavapipe": "llvmpipe" in text, "tail": text.splitlines()[-8:]}
    _, text = run(c, [f"{T}/run", "eglinfo", "-B"], ENV, tag="eglinfo")
    REPORT["eglinfo"] = {"ended": ended(text), "llvmpipe": "llvmpipe" in text}
    need(ended(text) is not None and ended(text)[0] == "exit" and "llvmpipe" in text, "eglinfo -B still finds llvmpipe", text[-3000:])

    # DR-8: vkcube with RADV, recorded only (QEMU has no Wayland compositor or KMS for it, so it may stop before
    # its first frame; the report says where).
    REPORT["vkcube"] = {}
    for wsi in ("wayland", "display"):
        _, text = run(c, [f"{T}/run", "vkcube", "--c", "3", "--wsi", wsi], RADV, timeout_ms=120000, tag=f"vkcube-{wsi}")
        REPORT["vkcube"][wsi] = {"ended": ended(text), "tail": text.splitlines()[-6:]}
    _, cs_text, _ = call(c, "log", {"subsystem": "drm", "level": "debug", "name": "cs.", "max": 2000})
    open(os.path.join(v.RUN, "klog-cs.txt"), "w").write(cs_text)
    REPORT["cs_submits"] = len(re.findall(r"cs\.submit", cs_text))
    REPORT["cs_refused"] = sorted(set(re.findall(r"cs\.refused .*?errno=(\S+)", cs_text)))
    _, hang_text, _ = call(c, "log", {"subsystem": "drm", "name": "fake.cs_", "max": 200})
    REPORT["fake_cs_events"] = sorted(set(re.findall(r"fake\.cs_\w+", hang_text)))
    _, text, _ = call(c, "log", {"subsystem": "drm", "max": 400})
    open(os.path.join(v.RUN, "klog-drm.txt"), "w").write(text)
    unsupported = re.findall(r"ioctl\.unsupported .*?name=(\S+)", text)
    invalid = re.findall(r"ioctl\.invalid .*?nr=(\S+)", text)
    REPORT["first_unsupported"] = unsupported[0] if unsupported else None
    REPORT["unsupported"] = sorted(set(unsupported))
    REPORT["invalid_numbers"] = sorted(set(invalid))
    REPORT["unknown"] = sorted(set(re.findall(r"(info\.unknown_query|ctx\.unknown_op|info\.mmr_refused) .*?key=(\S+)", text)))
    radv = open(os.path.join(v.RUN, "vulkaninfo.out")).read()
    REPORT["radv_stops_at_first_buffer"] = "Failed to allocate a buffer" in radv
    REPORT["radv_exit"] = ended(radv)
    _, vm_text, _ = call(c, "log", {"subsystem": "drm", "level": "debug", "name": "vm.", "max": 2000})
    open(os.path.join(v.RUN, "klog-vm.txt"), "w").write(vm_text)
    REPORT["vm_created"] = len(re.findall(r"vm\.created", vm_text))
    REPORT["vm_refused"] = sorted(set(re.findall(r"vm\.(?:op_refused|bad_va|update_failed|create_failed) .*", vm_text)))[:20]
    _, text, _ = call(c, "log", {"level": "info", "max": 2000})
    open(os.path.join(v.RUN, "klog-info.txt"), "w").write(text)
    REPORT["enosys_enotty"] = sorted({l for l in text.splitlines() if "syscall.enosys" in l or "ENOTTY" in l})[:60]
    json.dump(REPORT, open(os.path.join(v.RUN, "report.json"), "w"), indent=1)
    print(f"the first ioctl the fake does not answer: {REPORT['first_unsupported']}")
    need(not {u for u in REPORT["unsupported"] if u.startswith("AMDGPU_GEM") or u == "AMDGPU_VM"},
         "no GEM or VM ioctl is unsupported", json.dumps(REPORT["unsupported"]))
    need(not {"AMDGPU_CS", "AMDGPU_WAIT_CS", "AMDGPU_WAIT_FENCES"} & set(REPORT["unsupported"]),
         "CS, WAIT_CS and WAIT_FENCES are answered (DR-8)", json.dumps(REPORT["unsupported"]))
    need(REPORT["vm_created"] > 0, "RADV mapped memory into its GPU VM (GEM_VA, DR-7)", vm_text[-2000:])
    need(not {"AMDGPU_INFO", "AMDGPU_CTX"} & set(REPORT["unsupported"]) and not REPORT["unknown"],
         "RADV's INFO queries and contexts are all answered", json.dumps({k: REPORT[k] for k in ("unsupported", "unknown")}))
    need(not REPORT["radv_stops_at_first_buffer"] and REPORT["radv_exit"] == ("exit", 0),
         "RADV gets past memory allocation (DR-7)", radv[-2000:])
    print(json.dumps({k: REPORT[k] for k in ("first_unsupported", "unsupported", "invalid_numbers", "unknown", "drmprobe_amdgpu", "vm_created", "vm_refused",
                                            "vkcube", "cs_submits", "cs_refused", "fake_cs_events")}, indent=1))


if __name__ == "__main__":
    main()
