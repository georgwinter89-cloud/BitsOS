#!/usr/bin/env bash
# Copies the backend sources (src/fb) into the lane's Cobalt copy, where the
# Cobalt build picks them up as //starboard/shared/bitsos/fb. The copy is
# /var/tmp/claude-v2/yt/cobalt (never Georg's /home/georg/cobalt).
set -euo pipefail
here=$(cd "$(dirname "$0")/.." && pwd)
tree=${COBALT_COPY:-/var/tmp/claude-v2/yt/cobalt}/cobalt/src
case "$tree" in /home/georg/cobalt*) echo "refusing Georg's original tree" >&2; exit 1;; esac
dst=$tree/starboard/shared/bitsos/fb
mkdir -p "$dst"
# Only changed files are touched, so the incremental build stays small.
rsync -rc --delete "$here/src/fb/" "$dst/"
echo "synced into $dst"
