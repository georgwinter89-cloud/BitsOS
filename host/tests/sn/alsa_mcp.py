#!/usr/bin/env python3
"""SN-4 scenario client: over MCP (QEMU hostfwd) it puts the test WAVs on V2, runs alsa-hw (raw ioctls; its
transcript must equal alsa-hw.expected), aplay -D hw:0,0 (S16 and FLOAT_LE at 48 kHz), aplay -D default (S16 at
44.1 kHz through plug), speaker-test, and prints each program's output for the log.

Usage: alsa_mcp.py <host port> <token> <dir with s48.wav f48.wav s441.wav> <alsa-hw.expected>
       alsa_mcp.py <host port> <token> cobalt <seconds>   (SN-7: only alsa-hw cobalt, Cobalt's write pattern)
Exit status 0 = green."""
import base64
import http.client
import json
import sys
import time

PORT, TOKEN, WAVS, EXPECTED = int(sys.argv[1]), sys.argv[2], sys.argv[3], sys.argv[4]
next_id = 0
fail = False


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
            rpc(c, "initialize", {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "sn4-alsa", "version": "1"}})
            return c
        except (OSError, http.client.HTTPException, ValueError) as e:
            if time.time() > end:
                raise SystemExit(f"RED: no MCP answer on 127.0.0.1:{PORT}: {e}")
            time.sleep(1)


def call(c, name, args):
    r = rpc(c, "tools/call", {"name": name, "arguments": args})
    return r["isError"], "\n".join(i.get("text", "") for i in r["content"] if i["type"] == "text")


def need(cond, what, detail=""):
    global fail
    if cond:
        print(f"ok   {what}")
    else:
        print(f"FAIL {what}\n{detail}")
        fail = True


def put(c, path, data):
    step = 96 * 1024
    for off in range(0, len(data), step):
        err, t = call(c, "write_file", {"path": path, "data": base64.b64encode(data[off:off + step]).decode(), "offset": off})
        if err:
            raise SystemExit(f"RED: write_file {path} at {off}: {t}")


def ok(st):
    return bool(st) and st.endswith("exit 0")


def run(c, argv, timeout_ms=60000):
    err, t = call(c, "run", {"path": argv[0], "args": argv[1:], "timeout_ms": timeout_ms, "kill_after_ms": timeout_ms})
    if err or "\n--- stdout (" not in t:
        return None, t, t
    head, rest = t.split("\n--- stdout (", 1)
    out, errpart = rest.split("\n--- stderr (", 1)
    body = lambda part: part.split(") ---", 1)[1].removeprefix("\n")
    return head.splitlines()[0], body(out), body(errpart)


def log_times(c, name):
    """Kernel log times (ms) of `snd <name>` events."""
    err, t = call(c, "log", {"subsystem": "snd", "name": name, "max": 500})
    out = []
    for line in t.splitlines():
        if "[" in line and "]" in line:
            try:
                out.append(float(line.split("[", 1)[1].split("]", 1)[0]) * 1000)
            except ValueError:
                pass
    return out


def cobalt(secs):
    c = connect(180)
    # Let the boot settle first (the stick's file system and services start in the first seconds).
    time.sleep(20)
    st, out, err = run(c, ["/bin/alsa-hw", "cobalt", str(secs)], (secs + 60) * 1000)
    print(f"--- alsa-hw cobalt {secs}: {st}\n{out[:3000]}{err}")
    head = out.splitlines()[0] if out else ""
    need(head.startswith(f"cobalt xruns ") and f" frames {-(-secs * 48000 // 512) * 512} " in head and head.endswith("drain 0"),
         f"alsa-hw cobalt: {secs} s with Cobalt's writes played and drained", out + err)
    # An XRUN is QEMU timing when the writer itself was away >= 20 ms just before (its 2048 frames last
    # 42 ms, the mixer's floor keeps 20 of them: 22 ms away starve it), or the guest's output ran dry (the KernelRt mixer itself was
    # not run) within the second before it; every other XRUN is a logic error. One output underrun
    # explains one XRUN only (the refill to the floor after it), so a mixer that starves clients while
    # also running dry cannot hide behind a few stalls.
    under = sorted(log_times(c, "mix.hw_underrun") + log_times(c, "hda.underrun"))
    logic, excused, used = [], 0, set()
    for line in out.splitlines():
        if line.startswith("xrun "):
            _, t, _, _, gap, _ = line.split()
            t, gap = int(t), int(gap)
            u = next((i for i, u in enumerate(under) if i not in used and t - 1000 <= u <= t + 100), None)
            if gap >= 20:
                excused += 1
            elif u is not None:
                used.add(u)
                excused += 1
            else:
                logic.append(line)
    print(f"cobalt-verdict xruns_logic={len(logic)} xruns_qemu_timing={excused} output_underruns={len(under)}")
    need(not logic, "alsa-hw cobalt: 0 XRUNs that a stall of the guest does not explain", "\n".join(logic))
    sys.exit(1 if fail else 0)


def main():
    if WAVS == "cobalt":
        cobalt(int(EXPECTED))
    c = connect(180)
    for name in ["s48", "f48", "s441"]:
        put(c, f"/tmp/{name}.wav", open(f"{WAVS}/{name}.wav", "rb").read())
    print("ok   the three WAVs are on V2")
    # SN-12: no word is the usage (exit 2, nothing opened or played); the self-test needs its word.
    st, out, err = run(c, ["/bin/alsa-hw"])
    need(bool(st) and st.endswith("exit 2") and out == "" and err.startswith("usage: alsa-hw "), "alsa-hw without a word: usage, exit 2", f"{st}\n{out}{err}")
    # Every malformed call is the usage too (SN-12 fixer): a typo, an extra word, a value that is no number.
    for argv in (["mastr", "5"], ["state", "x"], ["selftest", "x"], ["master", "abc"], ["jack", "1"]):
        st, out, err = run(c, ["/bin/alsa-hw"] + argv)
        need(bool(st) and st.endswith("exit 2") and out == "" and err.startswith("usage: alsa-hw "),
             f"alsa-hw {' '.join(argv)}: usage, exit 2, no write", f"{st}\n{out}{err}")
    st, out, err = run(c, ["/bin/alsa-hw", "selftest"])
    print(f"--- alsa-hw selftest: {st}\n{out}{err}")
    want = open(EXPECTED).read()
    need(ok(st) and out.rstrip("\n") == want.rstrip("\n"), "alsa-hw: every answer as linux-6.6 gives it",
         "\n".join(f"  got  {g}\n  want {w}" for g, w in zip(out.splitlines() + [""] * 40, want.splitlines()) if g != w))
    for argv in (["/bin/aplay", "-v", "-D", "hw:0,0", "/tmp/s48.wav"], ["/bin/aplay", "-D", "hw:0,0", "/tmp/f48.wav"],
                 ["/bin/aplay", "-v", "/tmp/s441.wav"]):
        st, out, err = run(c, argv)
        print(f"--- {' '.join(argv)}: {st}\n{out}{err}")
        need(ok(st) and "Playing WAVE" in err, f"{' '.join(argv[1:])}: played", err)
    st, out, err = run(c, ["/bin/aplay", "-l"])
    print(f"--- aplay -l: {st}\n{out}{err}")
    need(ok(st) and "card 0: BitsOS [BitsOS], device 0: BitsOS PCM [BitsOS Mixer]" in out, "aplay -l lists card 0", out + err)
    st, out, err = run(c, ["/bin/speaker-test", "-D", "default", "-c", "2", "-l", "1", "-t", "sine", "-f", "440", "-P", "4"], 120000)
    print(f"--- speaker-test: {st}\n{out}{err}")
    need(ok(st) and "Rate set to 48000Hz" in out, "speaker-test: one loop on both channels", out + err)
    # After the sample-exact plays: the control elements (QEMU's hda-duplex senses no jack).
    st, out, err = run(c, ["/bin/alsa-hw", "jack"])
    need(ok(st) and out.strip() == "jack 0 0", "alsa-hw jack: Headphone Jack reads 0", out + err)
    st, out, err = run(c, ["/bin/alsa-hw", "master", "16384"])
    need(ok(st) and out.strip() == "master 0 16384", "alsa-hw master 16384: written", out + err)
    st, out, err = run(c, ["/bin/alsa-hw", "master", "40000"])
    need(not ok(st) and out.strip() == "master -22 40000", "alsa-hw master 40000: EINVAL (above 32768)", out + err)
    run(c, ["/bin/alsa-hw", "master", "32768"])
    st, out, err = run(c, ["/bin/alsa-hw", "state"])
    lines = out.splitlines()
    need(ok(st) and [l.split()[0] for l in lines] == ["jack", "master", "switch", "cvolume", "cswitch"]
         and "jack 0 0 0" in lines and "master 0 32768 32768" in lines and "switch 0 1 1" in lines,
         "alsa-hw state: the controls, read only", out + err)
    st, out, err = run(c, ["/bin/cat", "/proc/asound/cards"])
    need(ok(st) and "BitsOS Mixer" in out, "/proc/asound/cards", out + err)
    sys.exit(1 if fail else 0)


if __name__ == "__main__":
    main()
