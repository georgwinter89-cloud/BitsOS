#!/usr/bin/env bash
# Makes crates/fs-ext4/testdata/ktest.img.z, the ext4 image the fs.ext4.* ktests mount (FS-8a): 32 MiB,
# mkfs.ext4 defaults of CachyOS (metadata_csum, 64bit, flex_bg, journal, orphan_file), filled with -d from
# a small tree (files, fast and slow symlink, hard link, a sparse file, a directory e2fsck -D indexes as an
# htree). Deterministic (fixed time, UUID and hash seed), zlib-compressed. Run from the worktree root;
# the output path may be given (host/tests/fs/40-ext4.sh checks the committed file is reproducible).
set -euo pipefail
out=${1:-crates/fs-ext4/testdata/ktest.img.z}
work=$(mktemp -d "${CARGO_TARGET_DIR:-target}/ext4img.XXXXXX")
trap 'rm -rf "$work"' EXIT
t=$work/tree
mkdir -p "$t/dir/sub" "$t/many"
printf 'hello from ext4\n' > "$t/hello.txt"
python3 -c "import sys; sys.stdout.buffer.write(bytes((i*31)%251 for i in range(200000)))" > "$t/dir/sub/data.bin"
ln -s hello.txt "$t/fast-link"
ln -s "$(printf 'x/%.0s' $(seq 1 60))" "$t/slow-link"
ln "$t/hello.txt" "$t/dir/hard"
for i in $(seq -w 0 399); do printf '%s' "$i" > "$t/many/entry-with-a-long-name-$i"; done
truncate -s 20M "$t/sparse" && printf 'end' | dd of="$t/sparse" bs=1 seek=$((20*1024*1024-3)) conv=notrunc status=none
find "$t" -exec touch -h -d @1700000000 {} +
img=$work/ktest.img
truncate -s 32M "$img"
E2FSPROGS_FAKE_TIME=1700000000 mkfs.ext4 -q -F -t ext4 -b 4096 -U 0b17051e-0000-4000-8000-000000000008 \
  -E hash_seed=0b17051e-0000-4000-8000-0000000000aa,lazy_itable_init=1,nodiscard -L ktest -d "$t" "$img"
E2FSCK_TIME=1700000000 E2FSPROGS_FAKE_TIME=1700000000 e2fsck -fyD "$img" > /dev/null || [ $? -le 1 ]
e2fsck -fn "$img" > /dev/null
python3 -c "import zlib,sys; sys.stdout.buffer.write(zlib.compress(open(sys.argv[1],'rb').read(), 9))" "$img" > "$out"
echo "$out: $(stat -c %s "$out") bytes"
