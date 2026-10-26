#!/bin/sh
# BusyBox for BitsOS V2: one pinned source tarball (download cache, SHA256 checked), static-pie with the UL-1
# musl sysroot (user/sysroot/bin/v2-cc), x86-64-v3. Output: out/busybox and out/applets (one applet name per line).
# Idempotent: rebuilds only when out/ is older than this script or the config fragment.
# V2_DOWNLOADS overrides the download cache, V2_OFFLINE=1 refuses to download, V2_JOBS caps make's jobs.
set -eu
VER=1.36.1
SHA256=b8cc24c9574d809e7279c3be349795c5d5ceb6fdf19ca709f80cde50e47de314
URL=https://busybox.net/downloads/busybox-$VER.tar.bz2

here=$(cd "$(dirname "$0")" && pwd)
cache=${V2_DOWNLOADS:-$HOME/.cache/bitsos-v2/downloads}
tarball=$cache/busybox-$VER.tar.bz2
out=$here/out
work=$here/build
cc=$here/../../sysroot/bin/v2-cc

if [ -x "$out/busybox" ] && [ -s "$out/applets" ] && [ "$out/busybox" -nt "$0" ] && [ "$out/busybox" -nt "$here/config.fragment" ]; then
  echo "busybox: up to date ($out/busybox)"
  exit 0
fi

mkdir -p "$cache"
if [ ! -f "$tarball" ]; then
  [ "${V2_OFFLINE:-0}" = 1 ] && { echo "busybox: $tarball missing and V2_OFFLINE=1" >&2; exit 1; }
  echo "busybox: downloading $URL"
  curl -fsSL --max-time 300 -o "$tarball.part" "$URL"
  mv "$tarball.part" "$tarball"
fi
echo "$SHA256  $tarball" | sha256sum -c --quiet - || { echo "busybox: SHA256 mismatch for $tarball (expected $SHA256)" >&2; exit 1; }

# A fresh source tree every time: the tarball's contents only ever run through this make.
rm -rf "$work"
mkdir -p "$work"
tar -xjf "$tarball" -C "$work" --no-same-owner --no-same-permissions
src=$work/busybox-$VER
jobs=${V2_JOBS:-8}

# defconfig, then our choices (config.fragment: lines "CONFIG_X=y" or "# CONFIG_X is not set";
# every other line is a comment).
make -C "$src" -s HOSTCC=cc defconfig >/dev/null
key_of() { printf '%s\n' "$1" | sed -n 's/^\(CONFIG_[A-Z0-9_]*\)=.*/\1/p; s/^# \(CONFIG_[A-Z0-9_]*\) is not set$/\1/p'; }
while IFS= read -r line; do
  key=$(key_of "$line")
  [ -n "$key" ] || continue
  sed -i "/^$key=/d; /^# $key is not set\$/d" "$src/.config"
  printf '%s\n' "$line" >> "$src/.config"
done < "$here/config.fragment"
yes "" | make -C "$src" -s HOSTCC=cc oldconfig >/dev/null

# Each fragment line must have survived oldconfig (a typo or a dependency would silently drop it).
bad=0
while IFS= read -r line; do
  [ -n "$(key_of "$line")" ] || continue
  grep -qxF "$line" "$src/.config" || { echo "busybox: config line not applied: $line" >&2; bad=1; }
done < "$here/config.fragment"
[ $bad = 0 ] || exit 1

make -C "$src" -j"$jobs" HOSTCC=cc CC="$cc" AR=llvm-ar NM=llvm-nm STRIP=llvm-strip OBJCOPY=llvm-objcopy \
  EXTRA_CFLAGS="-march=x86-64-v3 -Wno-unused-command-line-argument" busybox >"$work/make.log" 2>&1 || {
  tail -40 "$work/make.log" >&2; exit 1; }
make -C "$src" -s busybox.links >/dev/null

mkdir -p "$out"
cp "$src/busybox" "$out/busybox.tmp"
llvm-strip "$out/busybox.tmp"
# busybox.links lists "/usr/bin/wc" etc.; V2 puts every program into /bin, so out/applets keeps the names only.
sed 's,.*/,,' "$src/busybox.links" | sort -u > "$out/applets"
cp "$src/.config" "$out/config"
mv "$out/busybox.tmp" "$out/busybox"
echo "busybox: built $out/busybox ($(wc -c < "$out/busybox") bytes, $(wc -l < "$out/applets") applets)"
