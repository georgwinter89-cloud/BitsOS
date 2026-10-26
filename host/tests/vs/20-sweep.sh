#!/bin/bash
# slow
# VS-1: a short run of the long host sweep (random geometry, device limits 1-9 blocks, every 5th seed with
# crash images). The full sweep: BITSVOL_SEEDS=200 (about 8 minutes).
set -eu
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
BITSVOL_SEEDS="${BITSVOL_SEEDS:-20}" cargo test -p bitsvol --lib sweep -- --ignored
echo "GREEN: sweep"
