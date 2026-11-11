#!/bin/bash
# WY-3b: X11 programs on the real 780M through Xwayland's glamor and DRI3, by the desk over MCP, never by a lane. Usage:
#   bash host/tests/minipc/x11gpu.sh <host[:port]> --tls <key store> --token-file <file> [--out <dir>]
# The Mini-PC is TLS only (host/mcp/mcp-call.mjs --tls; the token stays in its file, it is never printed). The stick needs
# \BITSOS2\xwayland\ (user/ports/xwayland/scripts/build-v2.sh, with glamor), \BITSOS2\mesa\ and \BITSOS2\bin\bitscomp-gpu;
# the desktop must draw on the chip (CONFIG bitscomp=gles or auto, gpu=on), and no X11 program may have started since the
# chip came up: an Xwayland already running in software stays so. One PASS or FAIL per step, a summary, exit 0 only when
# every step passed. What each step checks: x11gpu_check.py. Output of every program: target/minipc/x11gpu/.
# host/tests/wy/47-v2-xwayland-gpu.sh runs this same script against QEMU's fake node, where only the steps that need
# real drawing and real jobs may fail, and must. Nothing here touches the mode (GX-D9): the render node only.
set -euo pipefail
cd "$(dirname "$0")/../../.."
exec python3 -I host/tests/minipc/x11gpu_check.py "$@"
