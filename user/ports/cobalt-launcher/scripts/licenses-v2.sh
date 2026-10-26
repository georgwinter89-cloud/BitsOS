#!/usr/bin/env bash
# YT-5: the licence texts for \BITSOS2\cobalt\LICENSES\ (stage-v2.sh copies them), made with Chromium's own tool
# (tools/licenses/licenses.py license_file) from the GN dependencies of the two shipped binaries, in the lane's
# container (cobalt-yt-v2). Output: <copy>/lic/{libcobalt,loader_app}.txt, plus the fonts' list from the tree.
# Run it again after a change of the build's dependencies (a new third_party part). Takes a few minutes.
set -euo pipefail
podman start cobalt-yt-v2 >/dev/null
podman exec cobalt-yt-v2 bash -c '. /work/env.sh; set -e; cd /work/cobalt/src; rm -rf /work/lic.new; mkdir /work/lic.new
  gen() { nice -n 10 python3 tools/licenses/licenses.py license_file "/work/lic.new/$1.txt" --format txt \
    --gn-out-dir out/evergreen-x64-bitsos_qa --gn-target "$2" > "/work/lic.new/$1.log" 2>&1 \
    || { echo "licenses-v2: $2 failed:"; tail -5 "/work/lic.new/$1.log"; exit 1; }; rm "/work/lic.new/$1.log"; }
  gen libcobalt //cobalt:cobalt
  gen loader_app "//starboard/loader_app:loader_app(//starboard/linux/x64x11/toolchain:starboard)"
  cp starboard/content/fonts/LICENSES.txt /work/lic.new/fonts.txt
  rm -rf /work/lic; mv /work/lic.new /work/lic
  for f in /work/lic/*.txt; do echo "licenses-v2: $f $(grep -c "^--------------------" "$f") parts, $(wc -c <"$f") bytes"; done'
