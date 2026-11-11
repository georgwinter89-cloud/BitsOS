#!/usr/bin/env bash
# GM-1d: what the stick manifest's `tree mesa` and `tree deqp` lines read: the Mesa tree (stage-v2.sh) and deqp-vk
# (deqp-vk.sh, a first build takes about ten minutes) at /var/tmp/claude-v2/stage/, as Brave's and LibreOffice's. The
# scenarios that copy a tree themselves use user/ports/mesa/out/ and need only stage-v2.sh. Run it again after a Mesa update.
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
STAGE=${V2_STAGE:-/var/tmp/claude-v2/stage}
mkdir -p "$STAGE"
MESA_STAGE=$STAGE/mesa bash "$HERE/stage-v2.sh"
bash "$HERE/deqp-vk.sh"
rm -rf "$STAGE/deqp.new"
cp -r "$HERE/../out/deqp/tree" "$STAGE/deqp.new"
rm -rf "$STAGE/deqp"
mv "$STAGE/deqp.new" "$STAGE/deqp"
echo "stage-stick: $STAGE/mesa and $STAGE/deqp"
