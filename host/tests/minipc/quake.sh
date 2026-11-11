#!/bin/bash
# GM-8: milestone 3's Mini-PC run: vkQuake fullscreen on the real 780M through RADV, by the desk over MCP, never by a lane.
# Usage:
#   bash host/tests/minipc/quake.sh <host[:port]> --tls <key store> --token-file <file> [--out <dir>]
# The Mini-PC is TLS only (host/mcp/mcp-call.mjs --tls; the token stays in its file). The stick needs \BITSOS2\vkquake\,
# \BITSOS2\mesa\ and \BITSOS2\bin\vkquake, a render node (gx_bringup until=backend or GX-8c) and a desktop whose renderer
# takes RADV's images (WY-17b, or CONFIG bitscomp=gles). Silent: `snd` hdmi-off first, the game with -nosound.
# One PASS or FAIL per step, a summary, exit 0 only when every step passed. What each step checks: quake_check.py. Output
# of every program and the screenshots: target/minipc/quake/. Its logic is proven on made-up answers by
# host/tests/gm/tools/test_quake_check.py, the launcher and game on a headless bitscomp by host/tests/gm/51-vkquake-window.sh.
set -euo pipefail
cd "$(dirname "$0")/../../.."
exec python3 -I host/tests/minipc/quake_check.py "$@"
