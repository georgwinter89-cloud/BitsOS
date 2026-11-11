#!/usr/bin/env python3
"""Asks the window manager through EWMH: x11-fullscreen.py <window> <0 remove|1 add>; prints the window's _NET_WM_STATE after 1 s."""
import os
import socket
import struct
import sys
import time

num = os.environ["DISPLAY"].lstrip(":").split(".")[0]
s = socket.socket(socket.AF_UNIX)
s.connect(f"/tmp/.X11-unix/X{num}")
s.sendall(struct.pack("<BxHHHHxx", 0x6C, 11, 0, 0, 0))
head = s.recv(8, socket.MSG_WAITALL)
if head[0] != 1:
    raise SystemExit("the X server refused the connection")
setup = s.recv(struct.unpack_from("<H", head, 6)[0] * 4, socket.MSG_WAITALL)
vendor, formats = struct.unpack_from("<H", setup, 16)[0], setup[21]
root = struct.unpack_from("<I", setup, 32 + (vendor + 3 & ~3) + 8 * formats)[0]


def reply():
    while True:
        b = s.recv(32, socket.MSG_WAITALL)
        if b[0] == 1:
            return b + s.recv(struct.unpack_from("<I", b, 4)[0] * 4, socket.MSG_WAITALL)
        if b[0] == 0:
            raise SystemExit(f"X error {b[1]}")


def atom(name):
    n = name.encode()
    s.sendall(struct.pack("<BBHH2x", 16, 0, 2 + (len(n) + 3) // 4, len(n)) + n.ljust((len(n) + 3) & ~3, b"\0"))
    return struct.unpack_from("<I", reply(), 8)[0]


win, action = int(sys.argv[1], 16), int(sys.argv[2])
state, full = atom("_NET_WM_STATE"), atom("_NET_WM_STATE_FULLSCREEN")
event = struct.pack("<BBHII5I", 33, 32, 0, win, state, action, full, 0, 1, 0)
s.sendall(struct.pack("<BBHII", 25, 0, 11, root, 0x180000) + event)
time.sleep(1)
s.sendall(struct.pack("<BBHIIIII", 20, 0, 6, win, state, 4, 0, 16))
b = reply()
n = struct.unpack_from("<I", b, 16)[0]
print("fullscreen" if full in struct.unpack_from(f"<{n}I", b, 32) else "not fullscreen")
