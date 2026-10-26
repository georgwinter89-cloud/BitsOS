#!/usr/bin/env bash
# YT-4a: stages Starboard's conformance suite nplb for V2 from YT-1's build, for host/tests/yt/10-nplb.sh (not in
# the manifest: 47 MB of programs plus 278 MB of test data would ride in every gate's ktest image). The scenario
# copies the stage into \BITSOS2\cobalt\ of its own image, next to loader_app, so elf_loader_sandbox finds
# cobalt\lib\ through its RPATH $ORIGIN/lib like loader_app does:
#   elf_loader_sandbox             debug info stripped (35.7 -> ~12 MB)
#   app/nplb/lib/libnplb.so        as built
#   app/nplb/content/test/...      the real directory: the build's link points at the container path /work/...
# Run it again after a Cobalt rebuild. The stage is replaced as a whole (new dir, rename).
set -euo pipefail
OUT=${COBALT_OUT:-/var/tmp/claude-v2/yt/cobalt/cobalt/src/out/evergreen-x64-bitsos_qa}
TEST=${COBALT_TEST:-/var/tmp/claude-v2/yt/cobalt/cobalt/src/out/evergreen-x64_qa/test}
DEST=${NPLB_STAGE:-/var/tmp/claude-v2/yt/cobalt/v2-stick/nplb}
for f in "$OUT/elf_loader_sandbox" "$OUT/app/nplb/lib/libnplb.so"; do
  [ -f "$f" ] || { echo "stage-nplb-v2: $f is missing (YT-1's build)" >&2; exit 1; }
done
[ -d "$TEST/starboard" ] || { echo "stage-nplb-v2: $TEST/starboard is missing (YT-1's build)" >&2; exit 1; }
# FAT keeps names case-insensitively: two names differing only in case would overwrite each other on the stick.
clash=$(find "$TEST" | tr '[:upper:]' '[:lower:]' | sort | uniq -d)
[ -z "$clash" ] || { echo "stage-nplb-v2: names that clash on FAT: $clash" >&2; exit 1; }
NEW=$DEST.new
rm -rf "$NEW" "$DEST.old"
mkdir -p "$NEW/app/nplb/lib" "$NEW/app/nplb/content"
strip --strip-debug -o "$NEW/elf_loader_sandbox" "$OUT/elf_loader_sandbox"
cp --reflink=auto "$OUT/app/nplb/lib/libnplb.so" "$NEW/app/nplb/lib/"
cp -rL --reflink=auto "$TEST" "$NEW/app/nplb/content/test"
if [ -e "$DEST" ]; then mv "$DEST" "$DEST.old"; fi
mv "$NEW" "$DEST"
rm -rf "$DEST.old"
echo "stage-nplb-v2: $DEST: $(find "$DEST" -type f | wc -l) files, $(du -sm "$DEST" | cut -f1) MB"
