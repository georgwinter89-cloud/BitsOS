#!/bin/bash
# slow
# Run: `cargo xtask fullrun` (no ktest class names it, so the plain gate does not start it); by hand `cargo xtask scenario cx/10-ctxd`.
# CX-1: ctxd host tests (unit + integration): recall@10 on host/cx-eval, kill -9 during indexing and commit,
# private roots never in MCP output, snippet limit, MCP/socket bad input, hostile files, rename identity.
# ctxd is its own cargo workspace (user/services/ctxd), so `cargo xtask test` does not build it; this does.
# The tests with the real model need user/services/ctxd/third_party/ (gitignored downloads and the llama.cpp build,
# one copy per machine is enough): CTXD_THIRD_PARTY=<dir>, else this worktree's, else lane CX's, linked in here.
# Without it this script is RED and CTXD_REQUIRE_MODEL=1 makes each model test fail instead of passing unrun.
# Ports: 29541-29548.
set -euo pipefail
ROOT="${BITSOS_ROOT:-$(cd "$(dirname "$0")/../../.." && pwd)}"
cd "$ROOT/user/services/ctxd"
export CARGO_TARGET_DIR="$ROOT/target"
export CTXD_REQUIRE_MODEL=1
has_model() { [ -f "$1/models/multilingual-e5-small-q8_0.gguf" ] && [ -x "$1/build/llama.cpp/bin/llama-server" ]; }
if ! has_model third_party; then
    # A real folder without the whole model is someone's download or build in progress: never replaced.
    if [ -e third_party ] && [ ! -L third_party ]; then
        echo "RED: user/services/ctxd/third_party is there but lacks models/multilingual-e5-small-q8_0.gguf or build/llama.cpp/bin/llama-server"
        echo "     finish it or move it away; CTXD_THIRD_PARTY and lane CX's copy are only linked where no such folder is"
        exit 1
    fi
    found=""
    for d in "${CTXD_THIRD_PARTY:-}" /var/tmp/claude-v2/cx/user/services/ctxd/third_party; do
        if [ -n "$d" ] && has_model "$d"; then found=$d; break; fi
    done
    if [ -z "$found" ]; then
        echo "RED: no ctxd model: third_party/models/multilingual-e5-small-q8_0.gguf and third_party/build/llama.cpp/bin/llama-server"
        echo "     are in neither user/services/ctxd/third_party, CTXD_THIRD_PARTY nor lane CX's worktree (DESIGN.md names the downloads)"
        exit 1
    fi
    echo "model and llama-server from $found"
    rm -f third_party
    ln -s "$found" third_party
    # Gone again after the run: git's ignore line takes directories only, and a link is none.
    trap 'rm -f third_party' EXIT
fi
cargo test --offline 2>&1 | tail -n 60
