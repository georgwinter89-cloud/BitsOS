#!/bin/bash
# GM-1d: milestone 1's Mini-PC run: Vulkan on the real 780M through RADV, by the desk over MCP after GX-8b, never by a
# lane. Usage:
#   bash host/tests/minipc/vk.sh <host[:port]> --tls <key store> --token-file <file> [--out <dir>]
# The Mini-PC is TLS only (host/mcp/mcp-call.mjs --tls; the token stays in its file, it is never printed). The stick
# needs \BITSOS2\mesa\, \BITSOS2\deqp\ and \BITSOS2\bin\bitscomp-gpu, the desktop must run with a renderer that takes
# RADV's images (CONFIG bitscomp=gles, or CR-GM1c-2 done).
# One PASS or FAIL per step, a summary, exit 0 only when every step passed. What each step checks: vk_check.py. Output
# of every program: target/minipc/vk/. host/tests/gm/40-vk-proof.sh runs this same script against QEMU (`--plain
# --profile qemu`: RADV on the fake node, the window and deqp on lavapipe).
set -euo pipefail
cd "$(dirname "$0")/../../.."
exec python3 -I host/tests/minipc/vk_check.py "$@"
