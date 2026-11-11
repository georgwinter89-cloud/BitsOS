#!/usr/bin/env python3
"""WY-20: an X11 client as a game on SDL2's X11 back end is one, in plain protocol (the host has no xinput): a window,
XI2 raw motion selected on the root. Prints `ready WID`, then `raw DX DY` per XI_RawMotion (the raw values). Commands
on stdin, one per line, each answered with a line: `grab` (GrabPointer confined to the window), `ungrab`, `hide`
(XFixes HideCursor and a warp to the window's middle, which is what makes Xwayland lock), `show`, `sync`."""
import os
import select
import socket
import struct
import sys

display = os.environ["DISPLAY"].lstrip(":")
s = socket.socket(socket.AF_UNIX)
s.connect(f"/tmp/.X11-unix/X{display}")
s.sendall(struct.pack("<BxHHHHxx", 0x6C, 11, 0, 0, 0))


def read(n):
    b = b""
    while len(b) < n:
        c = s.recv(n - len(b))
        if not c:
            print("x11: connection closed", flush=True)
            sys.exit(1)
        b += c
    return b


head = read(8)
extra = read(struct.unpack("<H", head[6:8])[0] * 4)
if head[0] != 1:
    print(f"x11: setup refused: {extra[:80]!r}", flush=True)
    sys.exit(1)
rid_base = struct.unpack("<I", extra[4:8])[0]
vendor_len, _, _, nformats = struct.unpack("<HHBB", extra[16:22])
screen = 32 + (vendor_len + 3) // 4 * 4 + nformats * 8
root = struct.unpack("<I", extra[screen:screen + 4])[0]
white = struct.unpack("<I", extra[screen + 8:screen + 12])[0]

WID = rid_base + 1
seq = 0
events = []


def req(data):
    global seq
    seq += 1
    s.sendall(data)
    return seq


def read_event():
    """One reply, event or error as (type, whole bytes)."""
    b = read(32)
    if b[0] == 1 or b[0] == 35:
        b += read(struct.unpack("<I", b[4:8])[0] * 4)
    return b


def reply(want):
    while True:
        b = read_event()
        if b[0] == 0:
            print(f"x11: error {b[1]} major {b[10]} minor {struct.unpack('<H', b[8:10])[0]}", flush=True)
            sys.exit(1)
        if b[0] == 1 and struct.unpack("<H", b[2:4])[0] == want:
            return b
        events.append(b)


def extension(name):
    n = name.encode()
    pad = b"\0" * (-len(n) % 4)
    q = req(struct.pack("<BxHHxx", 98, 2 + (len(n) + len(pad)) // 4, len(n)) + n + pad)
    r = reply(q)
    if not r[8]:
        print(f"x11: no {name}", flush=True)
        sys.exit(1)
    return r[9]


# A visible cursor, XC_left_ptr of the server's own cursor font: without one Xwayland treats it as hidden.
FONT, CURSOR = rid_base + 2, rid_base + 3
req(struct.pack("<BxHIHxx", 45, 5, FONT, 6) + b"cursor\0\0")
req(struct.pack("<BxHIIIHH6H", 94, 8, CURSOR, FONT, FONT, 68, 69, 0, 0, 0, 0xffff, 0xffff, 0xffff))
# CreateWindow with a background pixel and the cursor, MapWindow.
req(struct.pack("<BBHIIhhHHHHIIII", 1, 0, 10, WID, root, 0, 0, 400, 300, 0, 1, 0, 0x2 | 0x4000, white, CURSOR))
req(struct.pack("<BxHI", 8, 2, WID))
xi = extension("XInputExtension")
q = req(struct.pack("<BBHHH", xi, 47, 2, 2, 2))  # XIQueryVersion 2.2
reply(q)
# XISelectEvents on the root: all master devices, XI_RawMotion (17).
req(struct.pack("<BBHIHxxHHI", xi, 46, 5, root, 1, 1, 1, 1 << 17))
xf = extension("XFIXES")
q = req(struct.pack("<BBHII", xf, 0, 3, 4, 0))  # XFixesQueryVersion
reply(q)
q = req(struct.pack("<BxH", 43, 1))  # GetInputFocus: everything above has been seen
reply(q)
print(f"ready {WID:#x}", flush=True)


def fp3232(b):
    hi, lo = struct.unpack("<iI", b)
    return hi + lo / 2**32


def on_event(b):
    if b[0] == 35 and b[1] == xi and struct.unpack("<H", b[8:10])[0] == 17:
        vlen = struct.unpack("<H", b[22:24])[0]
        mask = int.from_bytes(b[32:32 + vlen * 4], "little")
        n = bin(mask).count("1")
        raw_at = 32 + vlen * 4 + 8 * n
        vals = {}
        i = 0
        for axis in range(vlen * 32):
            if mask >> axis & 1:
                vals[axis] = fp3232(b[raw_at + 8 * i:raw_at + 8 * i + 8])
                i += 1
        print(f"raw {vals.get(0, 0.0):.2f} {vals.get(1, 0.0):.2f}", flush=True)
    elif b[0] == 0:
        print(f"x11: error {b[1]} major {b[10]}", flush=True)


def command(line):
    if line == "grab":
        q = req(struct.pack("<BBHIHBBIII", 26, 0, 6, WID, 0, 1, 1, WID, 0, 0))
        print(f"grab status={reply(q)[1]}", flush=True)
    elif line == "ungrab":
        req(struct.pack("<BxHI", 27, 2, 0))
        print("ungrab", flush=True)
    elif line == "hide":
        req(struct.pack("<BBHI", xf, 29, 2, WID))
        req(struct.pack("<BxHIIhhHHhh", 41, 6, 0, WID, 0, 0, 0, 0, 200, 150))
        print("hide", flush=True)
    elif line == "show":
        req(struct.pack("<BBHI", xf, 30, 2, WID))
        print("show", flush=True)
    elif line == "sync":
        q = req(struct.pack("<BxH", 43, 1))
        reply(q)
        print("sync", flush=True)


while True:
    while events:
        on_event(events.pop(0))
    r, _, _ = select.select([s, sys.stdin], [], [])
    if s in r:
        on_event(read_event())
    if sys.stdin in r:
        line = sys.stdin.readline()
        if not line:
            break
        command(line.strip())
