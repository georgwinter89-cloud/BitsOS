#!/usr/bin/env bash
# FS-8a: the ext4 image the fs.ext4.* ktests mount (made by host/tests/fs/tools/mk-ext4-ktest-image.sh with
# CachyOS's e2fsprogs; not byte-reproducible, mkfs -d copies the source files' ctimes) is clean for this
# machine's e2fsck, and its generator still makes a clean image. The ktests run in the gate's ktest step,
# the host tests (cargo test -p fs-ext4) in its unit step.
set -euo pipefail
cd "$(dirname "$0")/../../.."
for t in mkfs.ext4 e2fsck python3; do command -v "$t" > /dev/null || { echo "SKIP: $t missing"; exit 0; }; done
tmp=$(mktemp -d "${CARGO_TARGET_DIR:-target}/ext4-40.XXXXXX")
trap 'rm -rf "$tmp"' EXIT
unz() { python3 -c "import zlib,sys; open(sys.argv[2],'wb').write(zlib.decompress(open(sys.argv[1],'rb').read()))" "$1" "$2"; }
unz crates/fs-ext4/testdata/ktest.img.z "$tmp/committed.img"
e2fsck -fn "$tmp/committed.img" > "$tmp/fsck.log" 2>&1 || { cat "$tmp/fsck.log"; echo "FAIL: e2fsck of the committed ktest image"; exit 1; }
bash host/tests/fs/tools/mk-ext4-ktest-image.sh "$tmp/new.img.z" > /dev/null 2>&1
unz "$tmp/new.img.z" "$tmp/new.img"
e2fsck -fn "$tmp/new.img" > "$tmp/fsck2.log" 2>&1 || { cat "$tmp/fsck2.log"; echo "FAIL: e2fsck of a freshly made ktest image"; exit 1; }
cmp -s <(dumpe2fs -h "$tmp/committed.img" 2>/dev/null | grep -E '^(Filesystem features|Block size|Block count|Inode count)') \
       <(dumpe2fs -h "$tmp/new.img" 2>/dev/null | grep -E '^(Filesystem features|Block size|Block count|Inode count)') \
  || { echo "FAIL: the committed ktest image has other features/geometry than its generator makes now"; exit 1; }
echo "PASS: ext4 ktest image clean; generator agrees"
