#!/bin/bash
# PK-1: bitspkg on CachyOS. The host tests (user/tools/bitspkg/tests): install/upgrade/rollback/remove round
# trips, a cut before every step of every changing command leaves the old version working, hostile manifests and
# package folders, content sharing, verify, the lock. Then the static musl binary (the one V2 gets in /bin) runs
# its own round trip (`bitspkg selftest`) in the run folder. Nothing here needs QEMU, the stick or root.
set -euo pipefail
ROOT="${BITSOS_ROOT:-$(cd "$(dirname "$0")/../../.." && pwd)}"
RUN="${BITSOS_RUN_DIR:-$ROOT/target/xtask/runs/pk-10-bitspkg-linux}"
mkdir -p "$RUN"
cd "$ROOT/user"
T="$ROOT/target/user-scenario"
set +e
env -u CARGO_TARGET_DIR -u RUSTUP_TOOLCHAIN cargo test --offline -p bitspkg --target-dir "$ROOT/target/user-host" > "$RUN/test.log" 2>&1
rc=$?
set -e
grep -E "^test |test result|panicked" "$RUN/test.log" | tail -n 40
[ $rc = 0 ] || { echo "RED: bitspkg host tests (log $RUN/test.log)"; tail -n 30 "$RUN/test.log"; exit 1; }
env -u CARGO_TARGET_DIR -u RUSTUP_TOOLCHAIN cargo build -q --offline --release --target x86_64-unknown-linux-musl --target-dir "$T" -p bitspkg
BIN="$T/x86_64-unknown-linux-musl/release/bitspkg"
file "$BIN" | grep -q "static" || { echo "RED: $BIN is not static"; exit 1; }
"$BIN" selftest "$RUN/selftest" || { echo "RED: bitspkg selftest (static musl) on CachyOS"; exit 1; }
echo "bitspkg on CachyOS: green"
