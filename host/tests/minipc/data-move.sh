#!/bin/bash
# NV-2p: copies the stick's data.img (read-only at /stickdata) to the new /data on the SSD once, with links, modes,
# owners and times kept, checks the copy, and writes a marker; a second run refuses. The stick's image is never
# changed or deleted. By the desk over MCP, never from a program on the machine; the keys in CONFIG.TXT stay Georg's.
# Usage: bash host/tests/minipc/data-move.sh <host[:port]> --tls auto --token-file <F>
# What it does: data_move.py.
set -euo pipefail
cd "$(dirname "$0")/../../.."
host="${1:?the Mini-PC: <host[:port]> --tls auto --token-file <F>}"
shift
exec python3 -I host/tests/minipc/data_move.py "$host" target/minipc/data-move "$@"
