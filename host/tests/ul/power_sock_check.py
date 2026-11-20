#!/usr/bin/env python3
"""GM-6w: the power socket's peer rule, driven over MCP and QMP (our own QEMU only).

Usage: power_sock_check.py <mcp port> <token> <qmp socket>
QMP send-key is a USB keyboard (physical keys); MCP's run starts programs as user 1000.
1. MCP run `init --power poweroff` (user 1000, not bitscomp): refused before its line is read.
2. `init --power poweroff` typed (root, plain words, no compositor grant): refused, the machine stays.
3. `init --power-holder poweroff --no-input` typed: holds the grant but leaves out "input":"physical":
   refused, the grant ends with it, the machine stays.
4. `init --power-holder poweroff` typed: takes fb0's compositor grant, sends the JSON request with
   "input":"physical"; returns once QEMU has gone (ACPI S5). The shell script checks the serial log."""
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
sys.argv = [sys.argv[0], "poweroff"] + sys.argv[1:]
import power_check as pc  # noqa: E402  (UL-7's MCP and QMP helpers)


def main():
    c = pc.connect(240)
    q = pc.Qmp(pc.QMP_PATH)
    pc.time.sleep(2)
    pc.wait_screen(c, pc.PROMPT.rstrip(), "the console shell's prompt", timeout=90)
    t, _ = pc.tool(c, "run", {"path": "/bin/init", "args": ["--power", "poweroff"], "timeout_ms": 10000})
    pc.need("refused user 1000 asks only through /boot/BITSOS2/bin/bitscomp" in t, "user 1000 outside bitscomp is refused", t)
    q.type("init --power poweroff\n")
    pc.wait_screen(c, "holds the compositor grant", "root without the compositor grant is refused")
    t, _ = pc.tool(c, "run", {"path": "/bin/cat", "args": ["/proc/bitsos/compositor"], "timeout_ms": 5000})
    pc.need("holder=none" in t, "no compositor holds the grant before the holder peer", t)
    pc.time.sleep(1)
    pc.wait_screen(c, pc.PROMPT.rstrip(), "the machine stayed up")
    q.type("init --power-holder poweroff --no-input\n")
    pc.wait_screen(c, "real input asked", "the grant holder without the real-input statement is refused", timeout=30)
    held = ""
    for _ in range(40):
        held, _ = pc.tool(c, "run", {"path": "/bin/cat", "args": ["/proc/bitsos/compositor"], "timeout_ms": 5000})
        if "holder=none" in held:
            break
        pc.time.sleep(0.5)
    pc.need("holder=none" in held, "the refused holder ended and its grant with it", held)
    pc.wait_screen(c, pc.PROMPT.rstrip(), "the machine stayed up after the holder's refused request")
    q.type("init --power-holder poweroff\n")
    pc.need(q.gone(120), "QEMU ended after the compositor's JSON power-off request (ACPI S5)")
    print("ul power-sock: green")


main()
