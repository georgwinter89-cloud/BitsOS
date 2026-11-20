#!/usr/bin/env python3
"""CRASHDISK helper for 45-crashdisk.sh: reads \\BITSOS2\\CRASH\\SLOTS.BIN (taken out of the ESP with mtype) and
prints one line per slot: `slot N empty|record seq=S bytes=B flags=F sum=ok|bad|none first=<first line>`.
Usage: crashdisk_check.py SLOTS.BIN   (exit 1 when the file does not have 4 slots of 256 KiB)
The layout is crates/kexec/src/crash/disk/format.rs."""
import struct, sys

SLOT = 256 * 1024
HEADER = 128


def fnv64(data, h=0xcbf29ce484222325):
    for b in data:
        h ^= b
        h = (h * 0x100000001b3) & 0xFFFFFFFFFFFFFFFF
    return h


def main():
    data = open(sys.argv[1], "rb").read()
    if len(data) != 4 * SLOT:
        print(f"size {len(data)}, want {4 * SLOT}")
        return 1
    for i in range(4):
        s = data[i * SLOT:(i + 1) * SLOT]
        if s[:8] != b"BITSCRDK":
            print(f"slot {i} unprepared")
            continue
        version, hbytes, kind, slot, seq, blen, flags = struct.unpack_from("<HHHHQII", s, 8)
        if version != 1 or hbytes != HEADER or slot != i:
            print(f"slot {i} damaged")
            continue
        if kind == 0:
            print(f"slot {i} empty")
            continue
        body = s[HEADER:HEADER + blen]
        (stored,) = struct.unpack_from("<Q", s, 120)
        ok = fnv64(s[:120] + body) == stored
        first = body.split(b"\n", 1)[0].decode("utf-8", "replace")
        print(f"slot {i} record seq={seq} bytes={blen} flags={flags} sum={'ok' if ok else 'bad'} first={first}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
