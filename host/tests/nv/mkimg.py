#!/usr/bin/env python3
"""NV test images: pattern images and GPT images (the pattern fills everything outside the table).

pattern byte at offset off = ((off >> 9) * 31 + (off & 511) * 7 + seed) & 0xFF (crates/drivers/nvme `test_pattern`).
Usage: mkimg.py pattern PATH SIZE SEED
       mkimg.py gpt PATH BLOCKS BLOCK_SIZE SEED TYPE:FIRST:LAST [...]   TYPE = efi|msr|msdata|linux
"""
import struct, sys, uuid, zlib

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


def main():
    kind, path = sys.argv[1], sys.argv[2]
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
