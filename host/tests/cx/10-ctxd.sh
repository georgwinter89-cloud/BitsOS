#!/bin/bash
# slow
# CX-1: ctxd host tests (unit + integration): recall@10 on host/cx-eval, kill -9 during indexing and commit,
# private roots never in MCP output, snippet limit, MCP/socket bad input, hostile files, rename identity.
# ctxd is its own cargo workspace (user/services/ctxd), so `cargo xtask test` does not build it; this does.
# Tests with the real model skip when third_party/ (gitignored downloads) is missing; each skip is printed
# below as a SKIP line (CTXD_REQUIRE_MODEL=1 turns a skip into a failure). Ports: 29541-29548.
set -euo pipefail
cd "${BITSOS_ROOT:-$(dirname "$0")/../../..}/user/services/ctxd"
export CARGO_TARGET_DIR="${BITSOS_ROOT:-$PWD/../../..}/target"
rm -f "$CARGO_TARGET_DIR/tmp/cx-skips.txt"
status=0
cargo test --offline 2>&1 | tail -n 60 || status=$?
[ -f "$CARGO_TARGET_DIR/tmp/cx-skips.txt" ] && cat "$CARGO_TARGET_DIR/tmp/cx-skips.txt"
exit "$status"
