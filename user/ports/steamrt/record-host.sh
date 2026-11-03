#!/bin/sh
# SX-3c: the reference run on CachyOS: the same tree (out/), the same clean environment and graphics provider as
# host/tests/sx/v2_pv.py gives V2, inside an outer bwrap that hides the host's GPU (a /dev without /dev/dri, as in
# QEMU) and the home directory. Output: <out dir>/{true,system-info}.{out,err,rc}.
# Usage: sh user/ports/steamrt/record-host.sh <out dir>
set -eu
here=$(cd "$(dirname "$0")" && pwd)
dest=${1:?usage: record-host.sh <out dir>}
mkdir -p "$dest"
dest=$(cd "$dest" && pwd)
work=$(mktemp -d "${TMPDIR:-/var/tmp}/sx3c-host.XXXXXX")
trap 'rm -rf "$work"' EXIT
mkdir -p "$work/home" "$work/xdg"
chmod 700 "$work/xdg"
# A copy-runtime variable directory of its own (the tree's var/ is left alone).
for run in "true:true" "system-info:steam-runtime-system-info"; do
  name=${run%%:*} cmd=${run#*:}
  set +e
  bwrap --dev-bind / / --dev /dev --tmpfs /home --tmpfs /tmp --bind "$work/home" /tmp/pv/home --bind "$work/xdg" /tmp/pv/xdg \
    --ro-bind "$here/out/gfx" /tmp/pv/gfx -- \
    env -i HOME=/tmp/pv/home USER=user LANG=C.UTF-8 PATH=/usr/bin:/bin XDG_RUNTIME_DIR=/tmp/pv/xdg \
    PRESSURE_VESSEL_GRAPHICS_PROVIDER=/tmp/pv/gfx PRESSURE_VESSEL_VARIABLE_DIR="$work/var" PRESSURE_VESSEL_VERBOSE=1 \
    "$here/out/SteamLinuxRuntime_sniper/run" -- $cmd > "$dest/$name.out" 2> "$dest/$name.err"
  echo $? > "$dest/$name.rc"
  set -e
  echo "record-host: $name: rc=$(cat "$dest/$name.rc")"
done
