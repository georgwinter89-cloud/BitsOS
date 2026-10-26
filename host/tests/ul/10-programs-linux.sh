#!/bin/bash
# UL-2: UL's programs on CachyOS, where Linux is the oracle. Builds the user workspace (stable, static musl,
# x86_64-unknown-linux-musl) and runs its host tests: usys, init (incl. `init --once` under a scratch root with a
# DHCP view file), stress, and the abi-a/b/c transcripts against user/abi-tests/expected/wave-*.txt (all three
# waves must equal CachyOS exactly here; on BitsOS the kernel's ul.abi.* ktests compare the same files with the
# known-difference lists). Then stress for one second with every CPU.
set -eu
BITSOS_ROOT="${BITSOS_ROOT:-$(cd "$(dirname "$0")/../../.." && pwd)}"
cd "$BITSOS_ROOT/user"
T="$BITSOS_ROOT/target/user-scenario"
echo "== cargo test (user workspace, musl)"
env -u CARGO_TARGET_DIR -u RUSTUP_TOOLCHAIN cargo test --offline --release --target x86_64-unknown-linux-musl --target-dir "$T" 2>&1
echo "== stress -s 1"
out=$(env -u CARGO_TARGET_DIR -u RUSTUP_TOOLCHAIN cargo run -q --offline --release --target x86_64-unknown-linux-musl --target-dir "$T" -p stress -- -s 1 -q)
echo "$out"
echo "$out" | grep -q "workers (threads)"
echo "UL programs on CachyOS: green"
