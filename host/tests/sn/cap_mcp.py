#!/usr/bin/env python3
"""SN-9 scenario client: over MCP (QEMU hostfwd) it runs alsa-hw capture (its transcript without "# " lines must
equal capture.expected), CachyOS's arecord (/bin/aplay with argv[0] "arecord", as the alsa-utils link does)
on hw:0,0 and on default (44.1 kHz mono through plug), arecord -l, and the capture controls.

The default device comes from host/tests/sn/asound.conf, put into /tmp/.asoundrc (HOME=/tmp) until UL packages it
as /etc/asound.conf: capture through plug -> mmap_emul -> hw:0,0.

Usage: cap_mcp.py <host port> <token> <capture.expected>     Exit status 0 = green."""
import base64
import http.client
import json
import struct
import sys
import time

PORT, TOKEN, EXPECTED = int(sys.argv[1]), sys.argv[2], sys.argv[3]
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
            rpc(c, "initialize", {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "sn9-capture", "version": "1"}})
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


ENV = ["PATH=/bin:/sbin:/usr/bin", "HOME=/tmp", "LANG=C"]


def run(c, path, argv=None, args=None, timeout_ms=60000):
    a = {"path": path, "timeout_ms": timeout_ms, "kill_after_ms": timeout_ms, "env": ENV}
    if argv is not None:
        a["argv"] = argv
    if args is not None:
        a["args"] = args
    err, t = call(c, "run", a)
    if err or "\n--- stdout (" not in t:
        return None, t, t
    head, rest = t.split("\n--- stdout (", 1)
    out, errpart = rest.split("\n--- stderr (", 1)
    body = lambda part: part.split(") ---", 1)[1].removeprefix("\n")
    return head.splitlines()[0], body(out), body(errpart)


def ok(st):
    return bool(st) and st.endswith("exit 0")


def wav_size(c, path):
    st, out, err = run(c, "/bin/busybox", args=["stat", "-c", "%s", path])
    try:
        return int(out.strip())
    except ValueError:
        return -1


def wav_header(c, path):
    """(format tag, channels, rate, bits, data bytes) of a canonical 44-byte RIFF/WAVE header, or None."""
    err, text = call(c, "read_file", {"path": path, "offset": 0, "length": 44})
    if err:
        return None
    h = base64.b64decode(json.loads(text).get("data", ""))
    if len(h) != 44 or h[0:4] != b"RIFF" or h[8:16] != b"WAVEfmt " or h[36:40] != b"data":
        return None
    tag, ch, rate = struct.unpack_from("<HHI", h, 20)
    bits, = struct.unpack_from("<H", h, 34)
    data, = struct.unpack_from("<I", h, 40)
    return (tag, ch, rate, bits, data)


def main():
    c = connect(180)
    conf = open(EXPECTED.rsplit("/", 1)[0] + "/asound.conf", "rb").read()
    err, t = call(c, "write_file", {"path": "/tmp/.asoundrc", "data": base64.b64encode(conf).decode()})
    need(not err, "asound.conf as /tmp/.asoundrc", t)
    st, out, err = run(c, "/bin/alsa-hw", args=["capture", "2"], timeout_ms=60000)
    print(f"--- alsa-hw capture 2: {st}\n{out}{err}")
    got = [l for l in out.splitlines() if not l.startswith("# ")]
    want = open(EXPECTED).read().splitlines()
    need(ok(st) and got == want, "alsa-hw capture: every answer as linux-6.6 gives it",
         "\n".join(f"  got  {g}\n  want {w}" for g, w in zip(got + [""] * 40, want + [""] * 40) if g != w))
    st, out, err = run(c, "/bin/aplay", argv=["arecord", "-D", "hw:0,0", "-f", "S16_LE", "-r", "48000", "-c", "2", "-d", "2", "/tmp/rec48.wav"])
    print(f"--- arecord -D hw:0,0 2 s: {st}\n{out}{err}")
    size = wav_size(c, "/tmp/rec48.wav")
    need(ok(st) and "Recording WAVE" in err and size == 44 + 2 * 48000 * 4, f"arecord hw:0,0: 2 s S16 stereo 48 kHz ({size} bytes)", err)
    hdr = wav_header(c, "/tmp/rec48.wav")
    need(hdr == (1, 2, 48000, 16, 2 * 48000 * 4), f"rec48.wav header: PCM, 2 ch, 48000 Hz, 16 bit ({hdr})")
    st, out, err = run(c, "/bin/aplay", argv=["arecord", "-D", "default", "-f", "S16_LE", "-r", "44100", "-c", "1", "-d", "1", "/tmp/rec441.wav"])
    print(f"--- arecord -D default 44.1 kHz mono 1 s: {st}\n{out}{err}")
    size = wav_size(c, "/tmp/rec441.wav")
    need(ok(st) and "Recording WAVE" in err and size == 44 + 44100 * 2, f"arecord default: 1 s at 44.1 kHz mono through plug ({size} bytes)", err)
    hdr = wav_header(c, "/tmp/rec441.wav")
    need(hdr == (1, 1, 44100, 16, 44100 * 2), f"rec441.wav header: PCM, 1 ch, 44100 Hz, 16 bit ({hdr})")
    st, out, err = run(c, "/bin/aplay", argv=["arecord", "-d", "1", "/tmp/rec8k.wav"])
    print(f"--- arecord (its defaults: U8 8 kHz mono) 1 s: {st}\n{out}{err}")
    size = wav_size(c, "/tmp/rec8k.wav")
    need(ok(st) and "Unsigned 8 bit, Rate 8000 Hz, Mono" in err and size == 44 + 8000, f"arecord: U8 8 kHz mono through plug ({size} bytes)", err)
    hdr = wav_header(c, "/tmp/rec8k.wav")
    need(hdr == (1, 1, 8000, 8, 8000), f"rec8k.wav header: PCM, 1 ch, 8000 Hz, 8 bit ({hdr})")
    st, out, err = run(c, "/bin/aplay", argv=["arecord", "-l"])
    print(f"--- arecord -l: {st}\n{out}{err}")
    need(ok(st) and "List of CAPTURE Hardware Devices" in out and "card 0: BitsOS [BitsOS], device 0: BitsOS PCM [BitsOS Mixer]" in out,
         "arecord -l lists card 0 device 0", out + err)
    st, out, err = run(c, "/bin/alsa-hw", args=["cvolume", "16384"])
    need(ok(st) and out.strip() == "cvolume 0 16384", "alsa-hw cvolume 16384: written", out + err)
    st, out, err = run(c, "/bin/alsa-hw", args=["cvolume", "40000"])
    need(not ok(st) and out.strip() == "cvolume -22 40000", "alsa-hw cvolume 40000: EINVAL (above 32768)", out + err)
    st, out, err = run(c, "/bin/alsa-hw", args=["cswitch", "0"])
    need(ok(st) and out.strip() == "cswitch 0 0", "alsa-hw cswitch 0: written", out + err)
    run(c, "/bin/alsa-hw", args=["cswitch", "1"])
    run(c, "/bin/alsa-hw", args=["cvolume", "32768"])
    st, out, err = run(c, "/bin/cat", args=["/proc/asound/devices"])
    need(ok(st) and " 24: [ 0- 0]: digital audio capture" in out, "/proc/asound/devices lists the capture device", out + err)
    sys.exit(1 if fail else 0)


if __name__ == "__main__":
    main()
