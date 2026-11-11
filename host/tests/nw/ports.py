#!/usr/bin/env python3
"""Prints the first of N consecutive ports free for TCP and UDP on this machine, inside the lane's range.

Two runs on one machine must not share a server (a run's script would pass against the other's). The start is
random, so two runs starting together rarely pick the same; a server that still loses its bind makes its script RED.
Usage: ports.py <n>; BITSOS_PORT_LO and BITSOS_PORT_HI name the range, default this lane's 24500-24999.
"""
import os
import random
import socket
import sys


def free(port):
    for kind in (socket.SOCK_STREAM, socket.SOCK_DGRAM):
        s = socket.socket(socket.AF_INET, kind)
        try:
            s.bind(("127.0.0.1", port))
        except OSError:
            return False
        finally:
            s.close()
    return True


def main():
    n = int(sys.argv[1])
    lo = int(os.environ.get("BITSOS_PORT_LO", "24500"))
    hi = int(os.environ.get("BITSOS_PORT_HI", "24999"))
    starts = list(range(lo, hi - n + 2))
    random.shuffle(starts)
    for base in starts:
        if all(free(p) for p in range(base, base + n)):
            print(base)
            return 0
    print(f"no {n} free ports in {lo}-{hi}", file=sys.stderr)
    return 1


sys.exit(main())
