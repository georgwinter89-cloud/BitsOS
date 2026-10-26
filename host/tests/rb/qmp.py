#!/usr/bin/env python3
"""RB-2 helper: one QMP command on a QEMU monitor socket the scenario started (its own QEMU only).
Usage: qmp.py <unix socket path> <command> (e.g. system_reset). Prints QEMU's answer; exit 0 on "return"."""
import json
import socket
import sys

path, cmd = sys.argv[1], sys.argv[2]
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.settimeout(10)
s.connect(path)
f = s.makefile("rw")


def answer():
    while True:
        line = f.readline()
        if not line:
            raise SystemExit("qmp: connection closed")
        msg = json.loads(line)
        if "return" in msg or "error" in msg:
            return msg


f.readline()  # greeting
f.write(json.dumps({"execute": "qmp_capabilities"}) + "\n")
f.flush()
answer()
f.write(json.dumps({"execute": cmd}) + "\n")
f.flush()
r = answer()
print(f"qmp {cmd}: {r}")
sys.exit(0 if "return" in r else 1)
