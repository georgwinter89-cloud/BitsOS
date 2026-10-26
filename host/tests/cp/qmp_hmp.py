#!/usr/bin/env python3
"""Runs HMP commands over a QEMU QMP unix socket and prints their output (CP scenario helper).

usage: qmp_hmp.py SOCKET 'info lapic' ['info pic' ...] [--quit]
Each command's output is printed after a '=== <command>' line. --quit ends QEMU afterwards.
"""
import json
import socket
import sys


def main():
    args = sys.argv[1:]
    quit_after = "--quit" in args
    args = [a for a in args if a != "--quit"]
    if not args:
        print(__doc__, file=sys.stderr)
        return 2
    path, cmds = args[0], args[1:]
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    s.settimeout(20)
    s.connect(path)
    f = s.makefile("rw", encoding="utf-8", newline="\n")

    def reply():
        while True:
            line = f.readline()
            if not line:
                raise SystemExit("qmp: connection closed")
            msg = json.loads(line)
            if "return" in msg or "error" in msg:
                return msg

    def send(obj):
        f.write(json.dumps(obj) + "\n")
        f.flush()
        return reply()

    json.loads(f.readline())  # greeting
    send({"execute": "qmp_capabilities"})
    for c in cmds:
        r = send({"execute": "human-monitor-command", "arguments": {"command-line": c}})
        print("=== " + c)
        print(r.get("return", r.get("error")))
    if quit_after:
        try:
            send({"execute": "quit"})
        except (SystemExit, OSError, ValueError):
            pass
    return 0


if __name__ == "__main__":
    sys.exit(main())
