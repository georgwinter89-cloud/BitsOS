#!/usr/bin/env bash
# Makes the image's /etc/ld.so.cache: libraries of the stick's Mesa and Xwayland trees by name, at their V2 paths. Names the
# initramfs has in /usr/lib stay out, so nothing that runs today changes its libraries. Run after staging both trees.
# Usage: host-gl-cache.sh <the initramfs's /usr/lib names, one per line, or empty file> [stage dir]
set -euo pipefail
stage=${2:-/var/tmp/claude-v2/stage}
skip=$1
root=$(mktemp -d)
trap 'rm -rf "$root"' EXIT
mkdir -p "$root/etc"
: >"$root/etc/ld.so.conf"
for tree in mesa xwayland; do
  to=$root/boot/BITSOS2/$tree/lib
  mkdir -p "$to"
  echo "/boot/BITSOS2/$tree/lib" >>"$root/etc/ld.so.conf"
  for f in "$stage/$tree"/lib/*.so*; do
    n=${f##*/}
    case $n in ld-linux*|libc.so*|libm.so*|libdl.so*|libpthread.so*|librt.so*|libresolv.so*|libutil.so*|libmvec.so*|libnss_*|libnsl.so*|libanl.so*|libBrokenLocale.so*|libthread_db.so*|libc_malloc_debug.so*) continue ;; esac
    grep -qxF "$n" "$skip" && continue
    cp -L "$f" "$to/$n"
  done
done
ldconfig -r "$root" -X
cp "$root/etc/ld.so.cache" "$stage/ld.so.cache"
echo "host-gl-cache: $stage/ld.so.cache: $(strings "$stage/ld.so.cache" | grep -c '^/boot/BITSOS2/') libraries"
