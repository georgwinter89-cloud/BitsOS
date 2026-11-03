#!/bin/bash
# LO-1: LibreOffice's Mini-PC run, by the desk against the real machine over MCP, never in QEMU (Georg 30.10., CONTRACTS
# section 4). Usage: MINIPC_HOST=<address> MINIPC_PORT=<mcp port> MINIPC_TOKEN=<token> bash host/tests/minipc/lo.sh [--desktop]
# Put \BITSOS2\libreoffice\ and \BITSOS2\bin\libreoffice on the stick first.
# What it checks: lo_check.py. What a person checks: plan/notes/lo-LO-1.md. Pictures and output: target/minipc/lo/.
set -euo pipefail
cd "$(dirname "$0")/../../.."
: "${MINIPC_HOST:?the Mini-PC address}" "${MINIPC_PORT:?its MCP port}" "${MINIPC_TOKEN:?its MCP token}"
exec python3 -I host/tests/minipc/lo_check.py "$MINIPC_HOST" "$MINIPC_PORT" "$MINIPC_TOKEN" target/minipc/lo "$@"
