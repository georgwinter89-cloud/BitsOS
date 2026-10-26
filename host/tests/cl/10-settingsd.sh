#!/bin/bash
# CL-1: settingsd host tests: every built-in key set/applied/stored/undone, R8 switches refused, CONFIG.TXT edits
# keep the guarded lines byte-identical (port of FS-7's config_guard), journal/undo/history, secrets, namespace
# owners, fallback without a stick and resync, a hung stick write (FIFO) times out, the real daemon and the
# `settings` command over the socket, hostile input, kill -9. settingsd is its own cargo workspace
# (user/services/settingsd) until the desk adds it to user/Cargo.toml (CL-1 CR1), so this script builds it.
set -euo pipefail
ROOT="${BITSOS_ROOT:-$(cd "$(dirname "$0")/../../.." && pwd)}"
cd "$ROOT/user/services/settingsd"
export CARGO_TARGET_DIR="$ROOT/target"
cargo test --offline 2>&1 | grep -E "^test |test result|panicked|error" | tail -n 60
exit "${PIPESTATUS[0]}"
