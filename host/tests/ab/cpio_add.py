#!/usr/bin/env python3
"""Adds files to a cpio "newc" archive (the initramfs xtask writes): copies every entry up to the trailer, then
the new files, then a new trailer. Usage: cpio_add.py <in> <out> <name-in-archive>=<file> ..."""
import sys


def entries(buf):
    pos = 0
    while True:
        if buf[pos:pos + 6] != b"070701":
            raise SystemExit(f"cpio_add: no newc header at {pos}")
        f = [int(buf[pos + 6 + i * 8:pos + 14 + i * 8], 16) for i in range(13)]
        namesize, size = f[11], f[6]
        name_end = pos + 110 + namesize
        name = buf[pos + 110:name_end - 1].decode()
        data_start = (name_end + 3) & ~3
        end = (data_start + size + 3) & ~3
        if name == "TRAILER!!!":
            return
        yield buf[pos:end]
        pos = end


def header(ino, mode, size, name):
    n = name.encode() + b"\0"
    h = b"070701" + b"".join(b"%08X" % x for x in [ino, mode, 0, 0, 1, 0, size, 0, 0, 0, 0, len(n), 0]) + n
    return h + b"\0" * (-len(h) % 4)


def main():
    src, dst, adds = sys.argv[1], sys.argv[2], sys.argv[3:]
    buf = open(src, "rb").read()
    out = bytearray()
    for e in entries(buf):
        out += e
    ino = 100000
    for a in adds:
        name, path = a.split("=", 1)
        data = open(path, "rb").read()
        out += header(ino, 0o100755, len(data), name) + data + b"\0" * (-len(data) % 4)
        ino += 1
    out += header(0, 0, 0, "TRAILER!!!")
    open(dst, "wb").write(out)
    print(f"cpio_add: {len(adds)} files added, {len(out)} bytes")


main()
