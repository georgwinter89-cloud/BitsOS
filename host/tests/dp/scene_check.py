#!/usr/bin/env python3
"""DP-1 screendump scene: the screen QEMU shows is compared, pixel for pixel, with the kernel's RAM frame.

usage: scene_check.py QMP_SOCKET COM2_SOCKET OUT_DIR [--timeout SECS]

The kernel (ktest dp.scene.screendump) writes one `DPSCENE step=...` line per step on COM2 and waits. For each
step this script takes QMP `screendump` (PPM) and `pmemsave` of the RAM frame, answers `g`, and compares:
screen pixel == model(RAM pixel) for the scanout layout the line names (masks + bytes per pixel; QEMU's std VGA
reads the scanout as 32-bit x8r8g8b8). Layouts with 2 or 3 bytes per pixel are written packed (row pitch
stride * bytes) onto a scanout the kernel cleared to black; QEMU still shows it as 32-bit pixels, so the model
builds the packed byte stream and reads it back as x8r8g8b8 (`model_packed`). Pixels listed as hidden were changed in the RAM frame without damage:
the screen must still show their old value. Exit 0 when every step matched and the kernel said `done`
(QEMU is left running; the kernel ends it after the ktest).
"""
import json
import os
import socket
import sys
import time


class Qmp:
    def __init__(self, path):
        self.s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.s.settimeout(60)
        self.s.connect(path)
        self.f = self.s.makefile("rw", encoding="utf-8", newline="\n")
        json.loads(self.f.readline())
        self.cmd("qmp_capabilities")

    def cmd(self, name, **args):
        msg = {"execute": name}
        if args:
            msg["arguments"] = args
        self.f.write(json.dumps(msg) + "\n")
        self.f.flush()
        while True:
            line = self.f.readline()
            if not line:
                raise SystemExit("qmp: connection closed")
            r = json.loads(line)
            if "return" in r:
                return r["return"]
            if "error" in r:
                raise SystemExit(f"qmp {name}: {r['error']}")


def read_ppm(path):
    data = open(path, "rb").read()
    fields, pos = [], 0
    while len(fields) < 4:
        while data[pos:pos + 1].isspace():
            pos += 1
        if data[pos:pos + 1] == b"#":
            pos = data.index(b"\n", pos) + 1
            continue
        end = pos
        while not data[end:end + 1].isspace():
            end += 1
        fields.append(data[pos:end])
        pos = end
    if fields[0] != b"P6" or fields[3] != b"255":
        raise SystemExit(f"{path}: not an 8-bit P6 PPM")
    return int(fields[1]), int(fields[2]), data[pos + 1:]


def chan(mask):
    if mask == 0:
        return None
    shift = (mask & -mask).bit_length() - 1
    return shift, bin(mask).count("1")


def scale(v, bits):
    return ((v << (bits - 8)) | (v >> (16 - bits))) if bits >= 8 else v >> (8 - bits)


def encode(p, chans):
    """Scanout value of a 0x00RRGGBB pixel; written independently of the kernel's code."""
    out = 0
    for i, c in enumerate(chans):
        if c:
            v = (p >> (16 - 8 * i)) & 0xFF
            out |= scale(v, c[1]) << c[0]
    return out


def shown(v):
    """What QEMU's std VGA shows for a 32-bit scanout value: R, G, B."""
    return (v >> 16) & 0xFF, (v >> 8) & 0xFF, v & 0xFF


def model(ram, n, chans):
    """Expected screen bytes (R, G, B per pixel) for n RAM pixels (little-endian 0x00RRGGBB)."""
    src = {0: ram[2::4][:n], 1: ram[1::4][:n], 2: ram[0::4][:n]}  # RAM channel index (r, g, b) -> bytes
    out = bytearray(3 * n)
    for k, shift in ((0, 16), (1, 8), (2, 0)):  # screen R, G, B = scanout bits 16-23, 8-15, 0-7
        feeding = [(i, c) for i, c in enumerate(chans) if c and c[0] < shift + 8 and c[0] + c[1] > shift]
        if not feeding:
            continue  # stays 0
        tables = []
        for i, c in feeding:
            tables.append((i, bytes(((scale(v, c[1]) << c[0]) >> shift) & 0xFF for v in range(256))))
        if len(tables) == 1:
            i, t = tables[0]
            out[k::3] = src[i].translate(t)
        else:  # several channels share this byte: per pixel (slow, not used by the scene's layouts)
            for px in range(n):
                v = 0
                for i, t in tables:
                    v |= t[src[i][px]]
                out[3 * px + k] = v
    return out


def model_packed(ram, w, h, stride, chans, nbytes):
    """Expected screen bytes for a layout of nbytes (< 4) per pixel, packed with row pitch stride * nbytes into
    a black scanout of 32-bit pixels (stride == w: the rows are one contiguous byte stream)."""
    n = w * h
    if stride != w:
        raise SystemExit(f"packed step with stride {stride} != width {w}: the kernel sends these only without padding")
    src = {0: ram[2::4][:n], 1: ram[1::4][:n], 2: ram[0::4][:n]}
    packed = bytearray(nbytes * n)
    for k in range(nbytes):  # scanout byte k of each pixel: OR of every channel feeding bits 8k..8k+7
        acc = 0
        for i, c in enumerate(chans):
            if c and c[0] < 8 * k + 8 and c[0] + c[1] > 8 * k:
                t = bytes(((scale(v, c[1]) << c[0]) >> (8 * k)) & 0xFF for v in range(256))
                acc |= int.from_bytes(src[i].translate(t), "little")
        packed[k::nbytes] = acc.to_bytes(n, "little")
    fb = bytes(packed) + bytes(4 * n - len(packed))  # the rest of the scanout stays black
    out = bytearray(3 * n)
    out[0::3], out[1::3], out[2::3] = fb[2::4], fb[1::4], fb[0::4]
    return out


def compare(step, w, h, screen, ram, chans, hidden, stride=None, nbytes=4):
    n = w * h
    exp = model(ram, n, chans) if nbytes == 4 else model_packed(ram, w, h, stride, chans, nbytes)
    for x, y, old in hidden:
        r, g, b = shown(encode(old, chans))
        exp[3 * (y * w + x):3 * (y * w + x) + 3] = bytes((r, g, b))
    if exp == screen[:3 * n]:
        return 0
    bad, shown_rows = 0, 0
    for y in range(h):
        a, b = 3 * y * w, 3 * (y + 1) * w
        if exp[a:b] == screen[a:b]:
            continue
        for x in range(w):
            i = a + 3 * x
            if exp[i:i + 3] != screen[i:i + 3]:
                bad += 1
                if shown_rows < 8:
                    print(f"  {step}: pixel ({x},{y}) screen {tuple(screen[i:i+3])} want {tuple(exp[i:i+3])}")
                    shown_rows += 1
    return bad


def parse(line):
    f = dict(kv.split("=", 1) for kv in line.split()[1:] if "=" in kv)
    masks = [int(m, 16) for m in f["masks"].split(",")]
    hidden = []
    for item in filter(None, f.get("hidden", "").split(";")):
        x, y, old = item.split(":")
        hidden.append((int(x), int(y), int(old, 16)))
    w = int(f["width"])
    return f["step"], int(f["phys"], 16), w, int(f["height"]), int(f.get("stride", w)), masks, int(f["bytes"]), hidden


def main():
    args = sys.argv[1:]
    timeout = 300
    if "--timeout" in args:
        i = args.index("--timeout")
        timeout = int(args[i + 1])
        del args[i:i + 2]
    qmp_path, com2_path, out = args
    deadline = time.time() + timeout

    def connect():
        """Both sockets of the QEMU that runs now (xtask restarts QEMU when OVMF stalls before bitsboot)."""
        while time.time() < deadline:
            try:
                c = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
                c.connect(com2_path)
                c.settimeout(1)
                return c, Qmp(qmp_path)
            except (OSError, SystemExit, ValueError):
                time.sleep(0.5)
        raise SystemExit("FAIL: could not connect to QEMU's COM2 and QMP sockets")

    com2, qmp = connect()
    buf, failed, steps, done = b"", 0, 0, False
    while time.time() < deadline and not done:
        try:
            chunk = com2.recv(4096)
        except socket.timeout:
            continue
        except OSError:
            chunk = b""
        if not chunk:
            if steps:
                break  # QEMU ended in the middle of the scene
            com2.close()
            time.sleep(1)
            com2, qmp = connect()
            buf = b""
            continue
        buf += chunk
        while b"\n" in buf:
            raw, buf = buf.split(b"\n", 1)
            line = raw.decode("ascii", "replace").strip()
            if line == "DPSCENE done":
                done = True
                continue
            if not line.startswith("DPSCENE step="):
                continue
            step, phys, w, h, stride, masks, nbytes, hidden = parse(line)
            ppm = os.path.join(out, f"{step}.ppm")
            ramf = os.path.join(out, f"{step}.ram")
            qmp.cmd("screendump", filename=ppm)
            qmp.cmd("pmemsave", val=phys, size=w * h * 4, filename=ramf)
            com2.sendall(b"g")
            sw, sh, screen = read_ppm(ppm)
            ram = open(ramf, "rb").read()
            steps += 1
            if (sw, sh) != (w, h) or nbytes not in (2, 3, 4) or (nbytes < 4 and hidden):
                print(f"RED {step}: screen {sw}x{sh}, frame {w}x{h}, {nbytes} bytes per pixel, {len(hidden)} hidden")
                failed += 1
                continue
            chans = [chan(m) for m in masks[:3]]
            bad = compare(step, w, h, screen, ram, chans, hidden, stride, nbytes)
            if bad:
                print(f"RED {step}: {bad} of {w * h} pixels differ from the RAM frame (masks {[hex(m) for m in masks]})")
                failed += 1
            else:
                print(f"ok {step}: {w}x{h} screen == RAM frame through masks {[hex(m) for m in masks]}"
                      + (f", {len(hidden)} undamaged changes stayed off screen" if hidden else ""))
            os.remove(ramf)
    if not done:
        print(f"RED: the kernel did not finish the scene ({steps} steps seen)")
        failed += 1
    com2.close()  # QEMU keeps running: the kernel reports the ktest on COM1 and exits through isa-debug-exit
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
