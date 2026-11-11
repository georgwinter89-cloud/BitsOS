#!/usr/bin/env python3
"""GM-2a: reads Valve's client manifest (a KeyValues text): `version FILE` prints its version, `packages FILE` one line
`<name> <file> <size> <sha2>` per package. A manifest without the fields is refused."""
import re
import sys


def main():
    what, path = sys.argv[1], sys.argv[2]
    text = open(path, encoding="utf-8", errors="strict").read()
    ver = re.search(r'"version"\s*"(\d+)"', text)
    if not ver:
        raise SystemExit(f"{path}: no version")
    if what == "version":
        print(ver.group(1))
        return
    found = 0
    for m in re.finditer(r'"([^"]+)"\s*\{([^{}]*)\}', text):
        f = dict(re.findall(r'"(\w+)"\s*"([^"]*)"', m.group(2)))
        if "file" not in f:
            continue
        if not (re.fullmatch(r"[\w.]+", f["file"]) and f["size"].isdigit() and re.fullmatch(r"[0-9a-f]{64}", f["sha2"])):
            raise SystemExit(f"{path}: package {m.group(1)} is not what this script knows")
        print(m.group(1), f["file"], f["size"], f["sha2"])
        found += 1
    if not found:
        raise SystemExit(f"{path}: no packages")


if __name__ == "__main__":
    main()
