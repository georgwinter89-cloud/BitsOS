#!/bin/bash
# FS-14: checks host/tests/fs/memfd-lazy-linux.txt against Linux itself. Builds host/tests/fs/progs/fs-memfd-lazy.c
# for this machine and runs the modes pools, blocks and limit on CachyOS as a normal user (limit needs unprivileged
# user namespaces; without them the test says so and checks the other two). On V2 ktest fs.memfd.lazy runs pools and
# blocks, fs.memfd.lazy.limit runs `at DIR` on a 64-page tmpfs the kernel mounts and compares it with the limit block;
# `cargo xtask abi-diff abi-memfd-lazy` compares pools and blocks (user/abi-tests/c/memfd-lazy.c is the same file).
set -eu
cd "$BITSOS_ROOT"
RUN="${BITSOS_RUN_DIR:-$(mktemp -d)}"
mkdir -p "$RUN"
cc -O1 -o "$RUN/fs-memfd-lazy" host/tests/fs/progs/fs-memfd-lazy.c
want=host/tests/fs/memfd-lazy-linux.txt
out="$RUN/memfd-lazy-host.txt"
grep '^#' "$want" > "$out"
for mode in pools blocks limit; do
    echo "=== $mode" >> "$out"
    set +e
    "$RUN/fs-memfd-lazy" "$mode" > "$RUN/stdout" 2>/dev/null
    rc=$?
    set -e
    if [ "$mode" = limit ] && head -1 "$RUN/stdout" | grep -q '^setup: E'; then
        echo "fs-memfd-lazy: no user namespaces here ($(head -1 "$RUN/stdout")); limit block not checked"
        sed -i '$d' "$out"
        want_cut="$RUN/want.txt"
        sed '/^=== limit$/,$d' "$want" > "$want_cut"
        want="$want_cut"
        break
    fi
    if [ "$rc" -gt 128 ]; then echo "end: signal $((rc - 128))" >> "$out"; else echo "end: exit $rc" >> "$out"; fi
    cat "$RUN/stdout" >> "$out"
done
if diff -u "$want" "$out"; then
    echo "fs-memfd-lazy: the same as host/tests/fs/memfd-lazy-linux.txt"
else
    echo "fs-memfd-lazy: Linux answers differently from host/tests/fs/memfd-lazy-linux.txt"; exit 1
fi
