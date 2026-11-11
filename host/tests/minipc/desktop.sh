#!/bin/bash
# TS-1: the Mini-PC's ktest run with the desktop running. The desk against the real machine, never by a gate.
# Usage: bash host/tests/minipc/desktop.sh <host[:port]> --tls auto --token-file <F> [--generation N] [--record]
# What it does and what is green: ktests.py.
set -euo pipefail
cd "$(dirname "$0")/../../.."
exec python3 -I host/tests/minipc/ktests.py desktop "$@"
