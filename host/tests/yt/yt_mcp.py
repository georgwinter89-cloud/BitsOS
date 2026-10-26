#!/usr/bin/env python3
"""YT-4a MCP client (QEMU hostfwd; the MCP plumbing as in host/tests/sn/alsa_mcp.py, copied: scenarios do not use
another lane's files).

  yt_mcp.py PORT TOKEN wait SECONDS                     waits until MCP answers
  yt_mcp.py PORT TOKEN call TOOL JSON [OUT.png]         one tool call (JSON or @file): text to stdout, an image to OUT.png
  yt_mcp.py PORT TOKEN nplb RUNDIR FILTER SECONDS       runs nplb on V2 (output into /tmp/nplb/nplb.txt there), waits
                                                        up to SECONDS, copies the file to RUNDIR/nplb.txt
  yt_mcp.py PORT TOKEN avsync RUNDIR URL SECONDS        YT-5: `cobalt URL` with the frame trace, polls `cobalt --stats`
                                                        until the clip has played, copies av-stats.txt, av-trace.txt
                                                        and cobalt.log to RUNDIR, ends Cobalt
  yt_mcp.py PORT TOKEN console RUNDIR URL SECONDS [desk] YT-6: types `cobalt URL` into the console shell (init's, uid 0)
                                                        as Georg would, optionally after starting a desktop (bitscomp)
                                                        from it; checks Cobalt's uid, polls --stats, copies the files
                                                        to RUNDIR, ends Cobalt with Ctrl+Alt+Backspace
  yt_mcp.py PORT TOKEN get V2PATH HOSTFILE              copies a file from V2
  yt_mcp.py PORT TOKEN put HOSTFILE V2PATH              copies a file to V2

Exit status 0 = the call/run worked (nplb: it reached gtest's end line)."""
import base64
import http.client
import json
import os
import re
import sys
import time

PORT, TOKEN = int(sys.argv[1]), sys.argv[2]
next_id = 0
COBALT = "/boot/BITSOS2/cobalt"


def rpc(c, method, params=None):
    global next_id
    next_id += 1
    c.request("POST", "/mcp", json.dumps({"jsonrpc": "2.0", "id": next_id, "method": method, "params": params or {}}),
              {"Authorization": f"Bearer {TOKEN}", "Content-Type": "application/json"})
    r = c.getresponse()
    j = json.loads(r.read() or b"null")
    if r.status != 200 or not j or "result" not in j:
        raise ValueError(f"{method}: HTTP {r.status} {j}")
    return j["result"]


def connect(wait_s):
    end = time.time() + wait_s
    while True:
        try:
            c = http.client.HTTPConnection("127.0.0.1", PORT, timeout=120)
            rpc(c, "initialize", {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "yt4a", "version": "1"}})
            return c
        except (OSError, http.client.HTTPException, ValueError) as e:
            if time.time() > end:
                raise SystemExit(f"RED: no MCP answer on 127.0.0.1:{PORT}: {e}")
            time.sleep(1)


def call(c, name, args):
    r = rpc(c, "tools/call", {"name": name, "arguments": args})
    text = "\n".join(i.get("text", "") for i in r["content"] if i["type"] == "text")
    imgs = [base64.b64decode(i["data"]) for i in r["content"] if i["type"] == "image"]
    return r.get("isError", False), text, imgs


def get(c, path, host):
    data, off = b"", 0
    while True:
        err, t, _ = call(c, "read_file", {"path": path, "offset": off})
        if err:
            raise SystemExit(f"RED: read_file {path}: {t}")
        j = json.loads(t)
        data += base64.b64decode(j["data"]) if "data" in j else j.get("text", "").encode()
        if j.get("eof", True) or "next_offset" not in j:
            break
        off = j["next_offset"]
    open(host, "wb").write(data)
    return data


def put(c, path, data):
    step = 96 * 1024
    for off in range(0, len(data), step):
        err, t, _ = call(c, "write_file", {"path": path, "data": base64.b64encode(data[off:off + step]).decode(), "offset": off})
        if err:
            raise SystemExit(f"RED: write_file {path} at {off}: {t}")


# nplb as YT-1's reference run (user/ports/cobalt-launcher/scripts/nplb-yt.sh, fb branch) on the stick's Cobalt tree:
# the fake framebuffer in RAM, no input device, HOME in RAM. The working directory must be writable (the link and
# symlink tests make their files relative to it; on Linux it was the build directory), so it is /tmp/nplb, not the
# stick; library and content paths are relative to the loader's directory, not to the working directory.
# exec: kill_after_ms then ends nplb itself.
NPLB_SH = """mkdir -p /tmp/nplb/home && cd /tmp/nplb && exec {cobalt}/elf_loader_sandbox \
--evergreen_library=app/nplb/lib/libnplb.so --evergreen_content=app/nplb/content \
--gtest_filter='{filt}' > /tmp/nplb/nplb.txt 2>&1"""


def nplb(c, rundir, filt, secs):
    sh = NPLB_SH.format(cobalt=COBALT, filt=filt)
    env = ["PATH=/bin:/sbin:/usr/bin:/boot/BITSOS2/bin", "HOME=/tmp/nplb/home", "XDG_RUNTIME_DIR=/tmp/nplb",
           "BITSOS_BACKEND=fb", "BITSOS_FB=/tmp/nplb/fb0", "BITSOS_FB_FAKE=2560x1440",
           # UL-6: the console's language, as init gives its shell (SbSystemGetLocaleId answers from LANG).
           "LANG=de_DE.UTF-8"]
    start = time.time()
    err, t, _ = call(c, "run", {"path": "/bin/sh", "args": ["-c", sh], "env": env, "timeout_ms": 20000,
                                "kill_after_ms": secs * 1000})
    m = re.search(r"\(call (\d+)\)", t)
    # The run tool's status line: "... ended: ..." / "... finished ..." / "no call ..." (crates/proc/src/mcp_run.rs).
    def ended(t):
        first = t.splitlines()[0] if t else ""
        return "ended:" in first or "finished" in first or "no call" in first
    # A busy guest (-smp 4, nplb load) may miss one HTTP answer: reconnect and poll on until the deadline.
    while m and not ended(t) and time.time() - start < secs + 60:
        time.sleep(15)
        try:
            err, t, _ = call(c, "output", {"call": int(m.group(1))})
        except (OSError, http.client.HTTPException, ValueError) as e:
            print(f"  ... {int(time.time() - start)} s: poll failed ({e}), reconnecting", flush=True)
            t = ""
            c = connect(30)
            continue
        print(f"  ... {int(time.time() - start)} s: {t.splitlines()[0][:120] if t else ''}", flush=True)
    print(f"nplb run: {t[:2000]}")
    print(f"nplb took {int(time.time() - start)} s (QEMU; not judged)")
    # Always fetch what nplb wrote, also after a poll failure or the deadline: the scenario judges the file.
    for attempt in range(3):
        try:
            data = get(c, "/tmp/nplb/nplb.txt", f"{rundir}/nplb.txt").decode(errors="replace")
            break
        except (OSError, http.client.HTTPException, ValueError) as e:
            print(f"  fetch of nplb.txt failed ({e}), reconnecting", flush=True)
            c = connect(30)
    else:
        return 1
    return 0 if re.search(r"^\[==========\] .* ran", data, re.M) else 1


def avsync(c, rundir, url, secs):
    launcher = "/boot/BITSOS2/bin/cobalt"
    # No "env": MCP's own environment stays (XDG_RUNTIME_DIR decides the launcher's run directory, also for
    # the `--stats` calls below); the trace switch is added by the shell.
    err, t, _ = call(c, "run", {"path": "/bin/sh", "args": ["-c", f"COBALT_AV_TRACE=1 exec /bin/sh {launcher} '{url}'"],
                                "timeout_ms": 5000})
    print(f"start: {t.splitlines()[0][:160] if t else ''}", flush=True)
    start, done, where, last = time.time(), False, None, ""
    # SECONDS 0: a page without video (caps.html): give it 30 s, then only its log is wanted.
    while time.time() - start < (secs + 120 if secs else 30):
        time.sleep(5 if secs else 30)
        try:
            err, last, _ = call(c, "run", {"path": "/bin/sh", "args": [launcher, "--stats"], "timeout_ms": 15000})
        except (OSError, http.client.HTTPException, ValueError) as e:
            print(f"  ... {int(time.time() - start)} s: poll failed ({e}), reconnecting", flush=True)
            c = connect(30)
            continue
        m = re.search(r"(/\S+)/av-stats\.txt", last)
        where = m.group(1) if m else where
        media = re.search(r"media_time_us=(-?\d+)", last)
        shown = re.search(r"frames_shown=(\d+) frames_dropped=(\d+)", last)
        print(f"  ... {int(time.time() - start)} s: media {int(media.group(1)) / 1e6 if media else '-'} s, "
              f"shown/dropped {shown.groups() if shown else '-'}", flush=True)
        if not secs or (media and int(media.group(1)) >= (secs - 1) * 1_000_000):
            done = True
            break
    time.sleep(2)  # the numbers file follows at most once a second
    print("stats:\n" + "\n".join("  " + l for l in last.splitlines()))
    os.makedirs(rundir, exist_ok=True)
    for name in (["av-stats.txt", "av-trace.txt", "cobalt.log"] if secs else ["cobalt.log"]) if where else []:
        try:
            get(c, f"{where}/{name}", f"{rundir}/{name}")
        except (OSError, http.client.HTTPException, ValueError, SystemExit) as e:
            print(f"  fetch of {name} failed: {e}")
            c = connect(30)
    call(c, "run", {"path": "/bin/sh", "args": ["-c", "kill $(pidof loader_app) 2>/dev/null; true"], "timeout_ms": 5000})
    for _ in range(30):  # the launcher saves the profile after Cobalt's end
        _, t, _ = call(c, "run", {"path": "/bin/sh", "args": ["-c", "pidof loader_app || echo gone"], "timeout_ms": 5000})
        if "gone" in t:
            break
        time.sleep(1)
    print(f"avsync: {'played to the end' if done else 'did NOT reach the end'} in {int(time.time() - start)} s "
          "(QEMU; not judged)")
    return 0 if done else 1


def sh(c, cmd, timeout_ms=10000):
    _, t, _ = call(c, "run", {"path": "/bin/sh", "args": ["-c", cmd], "timeout_ms": timeout_ms})
    m = re.search(r"--- stdout \(\d+ bytes\) ---\n(.*?)--- stderr", t, re.S)
    return m.group(1).strip() if m else t


def screen(c):
    return call(c, "screen", {"format": "text"})[1]


def wait_for(what, timeout, probe):
    end = time.time() + timeout
    while time.time() < end:
        v = probe()
        if v:
            return v
        time.sleep(1)
    print(f"RED: {what} (waited {timeout} s)")
    return None


def console(c, rundir, url, secs, desk):
    """YT-6: Cobalt started from the console shell as Georg starts it. MCP's own programs are uid 1000; the console
    shell is init's (uid 0), so the keys go to it through MCP's keyboard (send_keys, layout de)."""
    red = 0
    run = "/run/user/1000/cobalt"
    os.makedirs(rundir, exist_ok=True)
    if not wait_for("the console shell's prompt", 120, lambda: re.search(r"(?m)^/ # ", screen(c))):
        return 1
    # Names of this run alone: a root-owned file of an earlier run (uid 1000 cannot remove it) must not answer.
    n = f"{os.getpid()}.{int(time.time() * 1000)}"
    uid_file, quit_file = f"/tmp/yt6-shell-uid.{n}", f"/tmp/yt6-quit.{n}"
    sh(c, f"rm -f {run}/launcher.log")
    line = f"rm -f {uid_file}; id -u >{uid_file}; COBALT_AV_TRACE=1 cobalt '{url}'\n"
    if desk:
        # A desktop started from the console runs as root (a terminal in it is a root shell): the launcher hands it
        # over. Its socket is root's, so a root watcher ends it at the end when this script asks (quit_file).
        line = ("(setsid bitscomp </dev/null >/tmp/yt6-bitscomp.log 2>&1 &); "
                f"(while [ ! -e {quit_file} ]; do sleep 1; done; bitscomp ctl quit >{quit_file}.out 2>&1) & sleep 5; " + line)
    call(c, "send_keys", {"text": line})
    shell_uid = wait_for("the console shell wrote its uid", 60, lambda: sh(c, f"cat {uid_file} 2>/dev/null"))
    print(f"console shell uid: {shell_uid}")
    if shell_uid != "0":
        print("RED: the console shell is not root (the scenario tests the root start)")
        red = 1
    pid = wait_for("Cobalt (loader_app) started", 90, lambda: sh(c, "pidof loader_app"))
    if not pid:
        print(screen(c)[-1500:])
        return 1
    uid = " ".join(sh(c, f"sed -n 's/^Uid:[[:space:]]*//p' /proc/{pid.split()[0]}/status").split())
    print(f"loader_app pid {pid}: Uid {uid}")
    if uid.split() != ["1000"] * 4:
        print("RED: Cobalt does not run as uid 1000 (real, effective, saved, fs)")
        red = 1
    start, done, last = time.time(), False, ""
    while time.time() - start < secs + 150:
        time.sleep(5)
        last = sh(c, "/bin/sh /boot/BITSOS2/bin/cobalt --stats", 15000)
        media = re.search(r"media_time_us=(-?\d+)", last)
        shown = re.search(r"frames_shown=(\d+) frames_dropped=(\d+)", last)
        print(f"  ... {int(time.time() - start)} s: media {int(media.group(1)) / 1e6 if media else '-'} s, "
              f"shown/dropped {shown.groups() if shown else '-'}", flush=True)
        if media and int(media.group(1)) >= (secs - 1) * 1_000_000:
            done = True
            break
    time.sleep(2)
    print("stats:\n" + "\n".join("  " + l for l in last.splitlines()))
    for name in ("av-stats.txt", "cobalt.log"):
        try:
            get(c, f"{run}/{name}", f"{rundir}/{name}")
        except (OSError, http.client.HTTPException, ValueError, SystemExit) as e:
            print(f"  fetch of {name} failed: {e}")
            c = connect(30)
    owner = sh(c, f"stat -c '%u:%g/%a' /run/user/1000 {run} {run}/home {run}/cobalt.log")
    print(f"owners (uid:gid/mode) of /run/user/1000, run dir, home, log: {' '.join(owner.split())}")
    call(c, "send_keys", {"keys": ["ctrl+alt+backspace"]})  # how Georg ends it
    if not wait_for("Cobalt ended after Ctrl+Alt+Backspace", 60, lambda: "gone" in sh(c, "pidof loader_app || echo gone")):
        red = 1
    if desk:
        log = wait_for("the launcher started the desktop again", 60,
                       lambda: (lambda t: t if "desktop started again" in t else None)(sh(c, f"cat {run}/launcher.log")))
        open(f"{rundir}/launcher.log", "w").write(log or sh(c, f"cat {run}/launcher.log"))
        dpid = sh(c, "pidof bitscomp")
        duid = " ".join(sh(c, f"sed -n 's/^Uid:[[:space:]]*//p' /proc/{dpid.split()[0]}/status").split()) if dpid else ""
        print(f"desktop after Cobalt: pid {dpid or '-'} Uid {duid or '-'}")
        print("desk-files-in-run: " + (" ".join(sh(c, f"ls -a {run} | grep '^desk[.]'").split()) or "-"))
        if not dpid or duid.split()[:1] != ["1000"]:
            print("RED: the desktop did not come back as it was (uid 1000)")
            red = 1
        # The root desktop must be gone before the MCP start (uid 1000 may not hand over a root desktop).
        sh(c, f"touch {quit_file}")
        if not wait_for("the root desktop ended (root watcher, bitscomp ctl quit)", 30,
                        lambda: "gone" in sh(c, "pidof bitscomp || echo gone")):
            print("RED: the root desktop still runs: " + sh(c, f"cat {quit_file}.out 2>/dev/null"))
            red = 1
    else:
        t = wait_for("the launcher's end on the console", 60,
                     lambda: (lambda t: t if re.search(r"cobalt: profile (saved|not saved)|sign-in not kept", t) else None)(screen(c)))
        open(f"{rundir}/console.txt", "w").write(t or screen(c))
    print(f"console: {'played to the end' if done else 'did NOT reach the end'} in {int(time.time() - start)} s")
    return red or (0 if done else 1)


def main():
    cmd = sys.argv[3]
    if cmd == "wait":
        connect(int(sys.argv[4]))
        return 0
    c = connect(30)
    if cmd == "call":
        arg = sys.argv[5]
        err, t, imgs = call(c, sys.argv[4], json.loads(open(arg[1:]).read() if arg.startswith("@") else arg))
        print(t)
        if imgs and len(sys.argv) > 6:
            open(sys.argv[6], "wb").write(imgs[0])
        return 1 if err else 0
    if cmd == "get":
        get(c, sys.argv[4], sys.argv[5])
        return 0
    if cmd == "put":
        put(c, sys.argv[5], open(sys.argv[4], "rb").read())
        return 0
    if cmd == "avsync":
        return avsync(c, sys.argv[4], sys.argv[5], int(sys.argv[6]))
    if cmd == "console":
        return console(c, sys.argv[4], sys.argv[5], int(sys.argv[6]), len(sys.argv) > 7 and sys.argv[7] == "desk")
    if cmd == "nplb":
        return nplb(c, sys.argv[4], sys.argv[5], int(sys.argv[6]))
    raise SystemExit(f"unknown command {cmd}")


if __name__ == "__main__":
    sys.exit(main())
