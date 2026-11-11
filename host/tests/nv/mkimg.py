#!/usr/bin/env python3
"""NV test images: pattern images and GPT images (the pattern fills everything outside the table).

pattern byte at offset off = ((off >> 9) * 31 + (off & 511) * 7 + seed) & 0xFF (crates/drivers/nvme `test_pattern`).
Usage: mkimg.py pattern PATH SIZE SEED
       mkimg.py gpt PATH BLOCKS BLOCK_SIZE SEED TYPE:FIRST:LAST [...]   TYPE = efi|msr|msdata|linux
       mkimg.py real PATH BLOCKS BLOCK_SIZE SEED [--backup-differs] TYPE:FIRST:LAST[:ntfs] [...]
                 a sparse image built like Georg's Lexar SSD (NV-2p): a GPT, marks of pattern every 64 MiB, each
                 partition's first 128 KiB and last block blank; `:ntfs` puts an NTFS boot sector at the partition's
                 start. Partition GUIDs: 4E56000N-... like `gpt` (`guid` prints the N-th).
       mkimg.py guid N
       mkimg.py outside PATH FIRST BLOCKS BLOCK_SIZE   sha256 of every byte outside the range
       mkimg.py outside-ranges PATH BLOCK_SIZE FIRST:BLOCKS [...]   the same outside several ranges
"""
import hashlib, os, struct, sys, uuid, zlib

TYPES = {
    "efi": "C12A7328-F81F-11D2-BA4B-00A0C93EC93B",
    "msr": "E3C9E316-0B5C-4DB8-817D-F92DF00215AE",
    "msdata": "EBD0A0A2-B9E5-4433-87C0-68B6B72699C7",
    "linux": "0FC63DAF-8483-4772-8E79-3D69D8477DE4",
}


def pattern(size, seed):
    # One 512-byte row per sector: row(s)[i] = (s*31 + i*7 + seed) & 0xFF; 256 distinct rows repeat.
    rows = [bytes(((s * 31 + i * 7 + seed) & 0xFF) for i in range(512)) for s in range(256)]
    out = bytearray(size)
    for s in range(size // 512):
        out[s * 512:(s + 1) * 512] = rows[s & 0xFF]
    return out


def gpt(blocks, bs, seed, parts):
    img = pattern(blocks * bs, seed)
    n_entries, esize = 128, 128
    eblocks = (n_entries * esize + bs - 1) // bs
    arr = bytearray(n_entries * esize)
    for i, (t, first, last) in enumerate(parts):
        e = struct.pack("<16s16sQQQ72s", uuid.UUID(TYPES[t]).bytes_le, uuid.UUID(int=0x4E56_0000 + i + 1).bytes_le,
                        first, last, 0, ("part%d" % (i + 1)).encode("utf-16-le"))
        arr[i * esize:(i + 1) * esize] = e
    arr_crc = zlib.crc32(arr) & 0xFFFFFFFF
    last = blocks - 1
    first_usable = 2 + eblocks
    last_usable = last - eblocks - 1
    # Protective MBR (block 0 cleared first).
    img[0:bs] = bytes(bs)
    img[0x1BE:0x1BE + 16] = struct.pack("<BBBBBBBBII", 0, 0, 2, 0, 0xEE, 0xFF, 0xFF, 0xFF, 1, min(last, 0xFFFFFFFF))
    img[510:512] = b"\x55\xAA"
    for my, alt, elba in ((1, last, 2), (last, 1, last - eblocks)):
        h = bytearray(struct.pack("<8sIIIIQQQQ16sQIII", b"EFI PART", 0x00010000, 92, 0, 0, my, alt, first_usable,
                                  last_usable, uuid.UUID(int=0x4E56_1234).bytes_le, elba, n_entries, esize, arr_crc))
        struct.pack_into("<I", h, 16, zlib.crc32(h) & 0xFFFFFFFF)
        img[my * bs:(my + 1) * bs] = bytes(h) + bytes(bs - 92)
        img[elba * bs:elba * bs + len(arr)] = arr
        # The rest of the entry blocks is part of the table: zero, not pattern.
        img[elba * bs + len(arr):(elba + eblocks) * bs] = bytes(eblocks * bs - len(arr))
    return img


def table(blocks, bs, parts):
    """(protective MBR, primary+array, backup array, backup header, array blocks) as `gpt` lays them out."""
    n_entries, esize = 128, 128
    eblocks = (n_entries * esize + bs - 1) // bs
    arr = bytearray(n_entries * esize)
    for i, (t, first, last) in enumerate(parts):
        e = struct.pack("<16s16sQQQ72s", uuid.UUID(TYPES[t]).bytes_le, uuid.UUID(int=0x4E56_0000 + i + 1).bytes_le,
                        first, last, 0, ("part%d" % (i + 1)).encode("utf-16-le"))
        arr[i * esize:(i + 1) * esize] = e
    arr_crc = zlib.crc32(arr) & 0xFFFFFFFF
    last = blocks - 1
    hdrs = []
    for my, alt, elba in ((1, last, 2), (last, 1, last - eblocks)):
        h = bytearray(struct.pack("<8sIIIIQQQQ16sQIII", b"EFI PART", 0x00010000, 92, 0, 0, my, alt, 2 + eblocks,
                                  last - eblocks - 1, uuid.UUID(int=0x4E56_1234).bytes_le, elba, n_entries, esize, arr_crc))
        struct.pack_into("<I", h, 16, zlib.crc32(h) & 0xFFFFFFFF)
        hdrs.append((my, elba, bytes(h)))
    return bytes(arr), hdrs


def real(path, blocks, bs, seed, specs, backup_differs):
    parts = [(s.split(":")[0], int(s.split(":")[1]), int(s.split(":")[2])) for s in specs]
    rows = [bytes(((s * 31 + i * 7 + seed) & 0xFF) for i in range(512)) for s in range(256)]
    with open(path, "wb") as f:
        f.truncate(blocks * bs)
        for at in range(0, blocks * bs, 64 << 20):
            f.seek(at)
            f.write(b"".join(rows[(at // 512 + k) & 0xFF] for k in range((1 << 16) // 512)))
        f.seek(blocks * bs - (1 << 16))
        f.write(b"".join(rows[k & 0xFF] for k in range((1 << 16) // 512)))
        for spec, (_, first, last) in zip(specs, parts):
            f.seek(first * bs)
            f.write(bytes(128 << 10))
            f.seek(last * bs)
            f.write(bytes(bs))
            if spec.endswith(":ntfs"):
                f.seek(first * bs + 3)
                f.write(b"NTFS    ")
        arr, hdrs = table(blocks, bs, parts)
        f.seek(0)
        mbr = bytearray(bs)
        mbr[0x1BE:0x1BE + 16] = struct.pack("<BBBBBBBBII", 0, 0, 2, 0, 0xEE, 0xFF, 0xFF, 0xFF, 1, min(blocks - 1, 0xFFFFFFFF))
        mbr[510:512] = b"\x55\xAA"
        f.write(mbr)
        for my, elba, h in hdrs:
            f.seek(my * bs)
            f.write(h + bytes(bs - 92))
            a = bytearray(arr)
            if backup_differs and my != 1:
                a[128 * 2 + 32] ^= 1
            f.seek(elba * bs)
            f.write(a + bytes(((len(a) + bs - 1) // bs) * bs - len(a)))


def outside(path, first, blocks, bs):
    outside_ranges(path, bs, [(first, blocks)])


def outside_ranges(path, bs, ranges):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        size = os.fstat(f.fileno()).st_size
        cuts = sorted((first * bs, (first + blocks) * bs) for first, blocks in ranges)
        edges = [0] + [x for c in cuts for x in c] + [size]
        for lo, hi in zip(edges[0::2], edges[1::2]):
            f.seek(lo)
            left = hi - lo
            while left:
                d = f.read(min(left, 1 << 20))
                h.update(d)
                left -= len(d)
    print(h.hexdigest())


def main():
    kind = sys.argv[1]
    if kind == "guid":
        print(str(uuid.UUID(int=0x4E56_0000 + int(sys.argv[2]))).upper())
        return
    path = sys.argv[2]
    if kind == "outside":
        outside(path, int(sys.argv[3]), int(sys.argv[4]), int(sys.argv[5]))
        return
    if kind == "outside-ranges":
        outside_ranges(path, int(sys.argv[3]), [tuple(int(x) for x in r.split(":")) for r in sys.argv[4:]])
        return
    if kind == "real":
        rest = sys.argv[6:]
        bd = "--backup-differs" in rest
        real(path, int(sys.argv[3]), int(sys.argv[4]), int(sys.argv[5]), [r for r in rest if not r.startswith("--")], bd)
        return
    if kind == "pattern":
        data = pattern(int(sys.argv[3]), int(sys.argv[4]))
    else:
        blocks, bs, seed = int(sys.argv[3]), int(sys.argv[4]), int(sys.argv[5])
        parts = []
        for spec in sys.argv[6:]:
            t, f, l = spec.split(":")
            parts.append((t, int(f), int(l)))
        data = gpt(blocks, bs, seed, parts)
    with open(path, "wb") as f:
        f.write(data)


if __name__ == "__main__":
    main()
