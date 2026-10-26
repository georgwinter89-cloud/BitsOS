#!/usr/bin/env python3
"""Mirror a read-only source tree into the build area with LF line endings.

The source trees were checked out on Windows (CRLF everywhere), which breaks musl's
configure, sed and awk scripts. A file is rewritten only when its converted content
differs, and keeps the source's mtime, so make and ninja rebuild only real changes.
    mirror.py SRC DST [SKIP_DIR_NAME...]
"""
import os
import shutil
import stat
import sys


def convert(data: bytes) -> bytes:
    if b"\0" in data:  # binary file: copy as is
        return data
    return data.replace(b"\r\n", b"\n")


def mirror(src: str, dst: str, skip: set) -> None:
    seen = set()
    for root, dirs, files in os.walk(src):
        dirs[:] = sorted(d for d in dirs if d not in skip)
        rel = os.path.relpath(root, src)
        out_dir = os.path.normpath(os.path.join(dst, rel))
        os.makedirs(out_dir, exist_ok=True)
        seen.add(out_dir)
        for name in files:
            s = os.path.join(root, name)
            d = os.path.join(out_dir, name)
            seen.add(d)
            with open(s, "rb") as f:
                data = convert(f.read())
            st = os.stat(s)
            try:
                with open(d, "rb") as f:
                    same = f.read() == data
            except FileNotFoundError:
                same = False
            if not same:
                with open(d, "wb") as f:
                    f.write(data)
                os.utime(d, ns=(st.st_atime_ns, st.st_mtime_ns))
            mode = 0o755 if (data.startswith(b"#!") or st.st_mode & stat.S_IXUSR) else 0o644
            if stat.S_IMODE(os.stat(d).st_mode) != mode:
                os.chmod(d, mode)
    for root, dirs, files in os.walk(dst, topdown=False):
        for name in files:
            p = os.path.join(root, name)
            if p not in seen:
                os.remove(p)
        if os.path.normpath(root) not in seen:
            shutil.rmtree(root, ignore_errors=True)


if __name__ == "__main__":
    if len(sys.argv) < 3:
        sys.exit(__doc__)
    mirror(sys.argv[1], sys.argv[2], set(sys.argv[3:]))
