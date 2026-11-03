#!/usr/bin/env python3
"""MC-10: types TEXT into the guest through QEMU's USB keyboard (QMP send-key), on V2's German console layout.

Usage: qmp_type.py <qmp socket> <text>   ("\\n" in the text is Enter)"""
import json
import socket
import sys
import time

# QEMU qcodes on V2's German console layout ('-' where the US slash is, '/' = shift+7, '&' = shift+6).
KEYS = {c: [c] for c in "abcdefghijklmnopqrstuvwx0123456789"}
# QWERTZ: y and z swap places.
KEYS.update({"y": ["z"], "z": ["y"]})
KEYS.update({" ": ["spc"], "-": ["slash"], "\n": ["ret"], "/": ["shift", "7"], "&": ["shift", "6"], ",": ["comma"], ":": ["shift", "dot"]})


def main():
    path, text = sys.argv[1], sys.argv[2].replace("\\n", "\n")
    end = time.time() + 60
    while True:
        try:
            s = socket.socket(socket.AF_UNIX)
            s.settimeout(30)
            s.connect(path)
            break
        except OSError:
            if time.time() > end:
                raise SystemExit("RED: no QMP socket")
            time.sleep(0.5)
    f = s.makefile("rw")
    f.readline()

    def cmd(name, args=None):
        f.write(json.dumps({"execute": name, "arguments": args or {}}) + "\n")
        f.flush()
        while True:
            r = json.loads(f.readline())
            if "return" in r:
                return r["return"]
            if "error" in r:
                raise SystemExit(f"RED: qmp {name}: {r['error']}")

    cmd("qmp_capabilities")
    for ch in text:
        if ch not in KEYS:
            raise SystemExit(f"RED: no key for {ch!r}")
        cmd("send-key", {"keys": [{"type": "qcode", "data": n} for n in KEYS[ch]], "hold-time": 40})
        time.sleep(0.08)


main()
