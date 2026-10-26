#!/bin/bash
# FS-13: checks host/tests/fs/mountflags-linux.txt against Linux itself. Builds host/tests/fs/progs/fs-mountflags.c
# for this machine and runs the modes proc and tmpfs on CachyOS as a normal user (tmpfs needs unprivileged user
# namespaces; without them the test says so and passes). On V2 ktest fs.mountflags.programs runs `at DIR` on a
# nosuid,nodev,noexec tmpfs the kernel mounts and compares it with the tmpfs block; `cargo xtask abi-diff
# abi-mountflags` compares `proc` (user/abi-tests/c/mountflags.c is the same file).
set -eu
cd "$BITSOS_ROOT"
RUN="${BITSOS_RUN_DIR:-$(mktemp -d)}"
mkdir -p "$RUN"
cc -O1 -o "$RUN/fs-mountflags" host/tests/fs/progs/fs-mountflags.c
out="$RUN/mountflags-host.txt"
grep '^#' host/tests/fs/mountflags-linux.txt > "$out"
for mode in proc tmpfs; do
    echo "=== $mode" >> "$out"
    set +e
    "$RUN/fs-mountflags" "$mode" > "$RUN/stdout" 2>/dev/null
    rc=$?
    set -e
    if [ "$mode" = tmpfs ] && head -1 "$RUN/stdout" | grep -q '^setup: E'; then
        echo "fs-mountflags: no user namespaces here ($(head -1 "$RUN/stdout")); tmpfs block not checked"
        exit 0
    fi
    if [ "$rc" -gt 128 ]; then echo "end: signal $((rc - 128))" >> "$out"; else echo "end: exit $rc" >> "$out"; fi
    cat "$RUN/stdout" >> "$out"
done
if diff -u host/tests/fs/mountflags-linux.txt "$out"; then
    echo "fs-mountflags: 2 modes the same as host/tests/fs/mountflags-linux.txt"
else
    echo "fs-mountflags: Linux answers differently from host/tests/fs/mountflags-linux.txt"; exit 1
fi
