#!/bin/bash
# GS-1: where the time goes from the kernel's graphics start to the desktop's first picture, read after a start of the
# Mini-PC by the desk over MCP, never by a lane. Usage:
#   bash host/tests/minipc/gpu-start.sh <host[:port]> --tls <key store|auto> --token-file <file> [--out <dir>]
# Reads only (bitscomp ctl boot, /proc/bitsos/boot, settings_get, and events when kernel.loglevel lets Info through);
# works at loglevel error too. One PASS or FAIL per step, the table, exit 0 only when every step passed. What each step
# checks: gpu_start_check.py (`--selftest` checks its parsing).
set -euo pipefail
cd "$(dirname "$0")/../../.."
exec python3 -I host/tests/minipc/gpu_start_check.py "$@"
