#!/usr/bin/env python3
"""Collects QEMU QMP events until one condition holds (CP-5 power scenario helper).

usage: qmp_events.py SOCKET TIMEOUT_S [--until-shutdown] [--resets N --serial LOG --boots M] [--quit]
Prints every event as one JSON line. --until-shutdown stops at the first SHUTDOWN event. --resets N with
--serial LOG and --boots M stops once N RESET events came and LOG holds M 'kernel start' lines (the machine
came back). --quit ends QEMU afterwards (if it still runs). Exit 0 when the condition held, 1 on timeout.

xtask may restart QEMU (its boot retries when OVMF stalls): the new instance opens the same socket path, so a
closed connection is followed by a new one until the timeout.
"""
import json
import os
import socket
import sys
import time


class Qmp:
    """One QMP connection with its own line buffer (a makefile() object may lose data after a timeout)."""

    def __init__(self, path, end):
        self.s = None
        self.buf = b""
        while time.time() < end:
            if os.path.exists(path):
                s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
                try:
                    s.connect(path)
                    s.settimeout(0.5)
                    self.s = s
                    self.send({"execute": "qmp_capabilities"})
                    return
                except OSError:
                    s.close()
                    self.s = None
            time.sleep(0.2)

    def send(self, obj):
        self.s.sendall((json.dumps(obj) + "\n").encode())

    def lines(self):
        """Complete lines received so far; [] after a quiet 0.5 s; EOFError when the connection closed."""
        try:
            chunk = self.s.recv(65536)
        except (socket.timeout, TimeoutError):
            return []
        except OSError:
            raise EOFError
        if not chunk:
            raise EOFError
        self.buf += chunk
        *done, self.buf = self.buf.split(b"\n")
        return [d for d in done if d.strip()]

    def close(self):
        if self.s:
            self.s.close()


def main():
    a = sys.argv[1:]
    if len(a) < 2:
        print(__doc__, file=sys.stderr)
        return 2
    path, timeout = a[0], float(a[1])
    opt = a[2:]
    until_shutdown = "--until-shutdown" in opt
    quit_after = "--quit" in opt

    def val(name, default=None):
        return opt[opt.index(name) + 1] if name in opt else default

    resets = int(val("--resets", "0"))
    serial = val("--serial")
    boots = int(val("--boots", "0"))
    end = time.time() + timeout
    q = Qmp(path, end)
    if not q.s:
        print("qmp: no socket", file=sys.stderr)
        return 1
    seen_resets = 0
    ok = False
    while time.time() < end and not ok:
        try:
            got = q.lines()
        except EOFError:
            q.close()
            print("qmp: connection closed; waiting for a new QEMU on the same socket", flush=True)
            time.sleep(1)
            q = Qmp(path, end)
            if not q.s:
                break
            continue
        for line in got:
            try:
                msg = json.loads(line)
            except ValueError:
                continue
            if "event" in msg:
                print(json.dumps(msg), flush=True)
                if msg["event"] == "SHUTDOWN" and until_shutdown:
                    ok = True
                if msg["event"] == "RESET":
                    seen_resets += 1
        if resets and seen_resets >= resets and serial and os.path.exists(serial):
            with open(serial, "rb") as fh:
                n = fh.read().count(b"kernel start")
            if n >= boots:
                ok = True
    if q.s and not ok:
        # Say where the machine is (running, paused after a shutdown, ...) for the log.
        try:
            q.send({"execute": "query-status"})
            t_end = time.time() + 2
            while time.time() < t_end:
                for line in q.lines():
                    if b"return" in line:
                        print("status: " + line.decode(errors="replace"), flush=True)
                        t_end = 0
        except (EOFError, OSError):
            pass
    if quit_after and q.s:
        try:
            q.send({"execute": "quit"})
            time.sleep(0.5)
        except OSError:
            pass
    q.close()
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
