#!/usr/bin/env bash
# slow
# SX-IPC: the abi-sysv-* programs on V2 against this CachyOS host, live (`cargo xtask abi-diff`, QEMU, uid 1000 on both
# sides). The same programs run inside the gate's ktest boot against the recorded transcripts (sx.ipc.programs).
# BitsOS pieces only: Steam itself never runs in QEMU (Georg 30.10.).
set -euo pipefail
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
LO="${BITSOS_PORT_LO:-32500}"
export BITSOS_LANE="${BITSOS_LANE:-sx}" BITSOS_PORT_LO="$LO" BITSOS_PORT_HI="${BITSOS_PORT_HI:-32767}"
cargo xtask abi-diff abi-sysv --port $((LO + 50)) --smp 4 --timeout 600
