#!/bin/bash
# GP-R2: the remote desk's package without QEMU (remote_desk_check.mjs): the shared token lookup of every client tool
# (USERPROFILE, HOME, BITSOS_TOKEN_FILE, paths with spaces), step 28's split parts on the round's real lines, stage 8
# and step 30 judged on the card's interrupt, and the heartbeat that keeps a generation's proof
# without a pause while a long ktest call runs.
set -eu -o pipefail
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/pc-05-remote-desk}"
mkdir -p "$RUN_DIR"
LO="${BITSOS_PORT_LO:-22500}"
command -v node > /dev/null || { echo "RED: node is needed (host/mcp's tools)"; exit 1; }
node host/tests/pc/remote_desk_check.mjs "$(realpath "$RUN_DIR")" $((LO + 5))
for f in host/mcp/*.mjs host/remote-desk/*.mjs; do node --check "$f" || { echo "RED: $f does not parse"; exit 1; }; done
echo "pc 05-remote-desk: green"
