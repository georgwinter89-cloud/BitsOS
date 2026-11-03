#!/usr/bin/env python3
"""SX-3c: judges what host/tests/sx/v2_pv.py brought back from V2 (<v2 dir>) and, when given, compares
steam-runtime-system-info's report with the CachyOS reference run (<host dir>, user/ports/steamrt/record-host.sh).

Checked on V2: `run -- true` and `run -- steam-runtime-system-info` end with 0 and pressure-vessel printed no
error line (" E: "); the report is JSON; the container is pressure-vessel with the host at /run/host; the runtime
is the sniper tree and ok; x86_64 can run and its libraries are ok; Vulkan's lavapipe and Mesa's EGL vendor come
from the provider and are ok; the Vulkan check lists llvmpipe; --copy-runtime hard-linked files into var/.
With the reference: every section equal after leaving out what differs by design (KNOWN below, each with its
reason; plan/notes/sx-SX-3c.md explains every red line of the report).

A command that fails inside (`run -- sh -c 'exit 7'`) must hand back its exit code 7. SX_REQUIRE_HOST_REF=1 makes a
missing reference a failure; the last line says whether the report was compared.

Arguments: <v2 dir> [<host dir>]"""
import json
import os
import re
import sys

V2 = sys.argv[1]
HOST = sys.argv[2] if len(sys.argv) > 2 else None
REQUIRE_REF = bool(os.environ.get("SX_REQUIRE_HOST_REF"))
bad = []
compared = False


def check(cond, what):
    print(("ok: " if cond else "RED: ") + what, flush=True)
    if not cond:
        bad.append(what)


def read(d, name):
    try:
        return open(os.path.join(d, name), errors="replace").read()
    except OSError:
        return ""


# Left out of the comparison, with the reason (the report keys they touch):
KNOWN = [
    ("virtualization", "V2 runs in QEMU/KVM, the reference on the bare Gaming-PC"),
    ("container.host.os-release", "V2 names itself (ID=bitsos, user/packaging/os-release), the reference CachyOS; "
     "checked on its own"),
    ("architectures.i386-linux-gnu", "V2 runs no 32-bit programs (AB-10a/b); an i386 ELF is ENOEXEC as on a Linux "
     "without IA32 emulation, so the 32-bit helpers fall back to /bin/sh"),
    ("runtime.overrides i386 lines", "the same: no 32-bit graphics stack is captured"),
]


def normalised(rep):
    r = json.loads(json.dumps(rep))
    r.pop("virtualization", None)
    r.get("container", {}).get("host", {}).pop("os-release", None)
    r.get("architectures", {}).pop("i386-linux-gnu", None)
    ov = r.get("runtime", {}).get("overrides", {})
    if isinstance(ov.get("list"), list):
        ov["list"] = [x for x in ov["list"] if "i386" not in x]
    return r


def main():
    for name in ("true", "system-info"):
        rc = read(V2, f"{name}.rc").strip()
        check(rc == "0", f"run -- {name}: exit {rc or '?'} (want 0)")
        errs = [l for l in read(V2, f"{name}.err").splitlines() if " E: " in l]
        check(not errs, f"run -- {name}: no pressure-vessel error line" + (f" (first: {errs[0][:200]})" if errs else ""))
    try:
        rep = json.loads(read(V2, "system-info.out"))
    except ValueError as e:
        check(False, f"steam-runtime-system-info's report is JSON ({e})")
        return
    cont = rep.get("container", {})
    check(cont.get("type") == "pressure-vessel" and cont.get("host", {}).get("path") == "/run/host",
          f"container: {cont.get('type')}, host at {cont.get('host', {}).get('path')}")
    osr = cont.get("host", {}).get("os-release", {})
    check(osr.get("id") == "bitsos" and osr.get("version_id") == "2",
          f"container.host.os-release names V2: id={osr.get('id')} version_id={osr.get('version_id')}")
    rt = rep.get("runtime", {})
    check(rt.get("ok") is True and rt.get("version", "").startswith("3.0."), f"runtime {rt.get('version')} ok={rt.get('ok')}")
    x = rep.get("architectures", {}).get("x86_64-linux-gnu", {})
    check(x.get("can-run") is True and x.get("libraries-ok") is True,
          f"x86_64: can-run={x.get('can-run')} libraries-ok={x.get('libraries-ok')}")
    icds = rep.get("vulkan", {}).get("icds", [])
    check(any(i.get("ok") and i.get("library_path", "").endswith("/libvulkan_lvp.so") for i in icds),
          "Vulkan: lavapipe's ICD from the provider, ok")
    egl = rep.get("egl", {}).get("icds", [])
    check(any(i.get("ok") and i.get("library_path") == "libEGL_mesa.so.0" for i in egl), "EGL: Mesa's vendor file, ok")
    vk = x.get("graphics-details", {}).get("x11/vulkan", {})
    names = [d.get("name", "") for d in vk.get("devices", [])]
    check(any(n.startswith("llvmpipe") for n in names), f"the Vulkan check finds llvmpipe ({names})")
    rc = read(V2, "exit7.rc").strip()
    check(rc == "7", f"run -- sh -c 'exit 7': exit {rc or '?'} (want 7, the command's own code)")
    virt = rep.get("virtualization", {})
    check(isinstance(virt, dict) and bool(virt.get("type")), f"the report names a virtualization type ({virt.get('type') if isinstance(virt, dict) else virt})")
    m = re.search(r"linked=(\d+)", read(V2, "var.txt"))
    check(m is not None and int(m.group(1)) > 0, f"--copy-runtime: {m.group(1) if m else '?'} hard-linked files in var/")
    global compared
    ref = None
    if HOST:
        try:
            ref = json.loads(read(HOST, "system-info.out"))
        except ValueError:
            pass
    if ref is None:
        why = "no CachyOS reference given" if not HOST else "the CachyOS reference run has no report"
        if REQUIRE_REF:
            check(False, f"{why} (SX_REQUIRE_HOST_REF=1)")
        else:
            print(f"note: {why}, the report is not compared")
        return
    compared = True
    a, b = normalised(ref), normalised(rep)
    for k in sorted(set(a) | set(b)):
        same = json.dumps(a.get(k), sort_keys=True) == json.dumps(b.get(k), sort_keys=True)
        check(same, f"report section {k}: as on CachyOS")
        if not same:
            print("   CachyOS: " + json.dumps(a.get(k), sort_keys=True)[:1500])
            print("   V2:      " + json.dumps(b.get(k), sort_keys=True)[:1500])
    for key, why in KNOWN:
        print(f"known: {key}: {why}")


main()
verdict = "compared with CachyOS: " + ("yes" if compared else "no")
if bad:
    print(f"pressure-vessel: RED ({len(bad)} checks; {verdict})")
    sys.exit(1)
print(f"pressure-vessel: green ({verdict})")
