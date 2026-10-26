#!/usr/bin/env python3
"""MC-4b scenario helper (not a scenario): after each `reboot` over MCP the QEMU of the scenario resets; OVMF
sometimes stalls before bitsboot (CONTRACTS §1, the usb-storage flake). This watches the serial log: when a reset
line (`mcp reboot.requested`) is not followed by bitsboot's banner within 30 s, it resets the machine once more
through QMP (nothing ran on the stick in between). Runs until it is killed; prints one line per nudge.

Usage: ovmf_nudge.py <serial log> <qmp socket>"""
import json
import socket
import sys
import time

LOG, SOCK = sys.argv[1], sys.argv[2]
RESET, BANNER = "mcp reboot.requested", "bitsboot: BitsOS V2 loader"


def text():
    try:
        with open(LOG, "rb") as f:
            return f.read().decode("utf-8", "replace")
    except FileNotFoundError:
        return ""


def qmp_reset():
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    s.settimeout(10)
    s.connect(SOCK)
    f = s.makefile("rw")
    f.readline()
    for c in ("qmp_capabilities", "system_reset"):
        f.write(json.dumps({"execute": c}) + "\n")
        f.flush()
        while True:
            m = json.loads(f.readline())
            if "return" in m or "error" in m:
                break
    s.close()


seen = 0  # where the last handled reset line ended
while True:
    t = text()
    at = t.find(RESET, seen)
    if at < 0:
        time.sleep(0.5)
        continue
    seen = at + len(RESET)
    since = time.time()
    for attempt in range(3):
        while time.time() - since < 30 and BANNER not in text()[seen:]:
            time.sleep(0.5)
        if BANNER in text()[seen:]:
            break
        print(f"nudge: no bitsboot banner 30 s after a reset, QMP system_reset ({attempt + 1})", flush=True)
        seen = len(text())
        qmp_reset()
        since = time.time()
