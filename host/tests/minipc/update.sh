#!/bin/bash
# UPD-4: after an update with parts on the real machine (Mini-PC or Gaming-PC: the same releases), the running
# version runs with its own \BITSOS2\bin and trees; with --back, the previous version with the medium's own folders.
# Usage: bash host/tests/minipc/update.sh <host[:port]> <release dir> --tls auto --token-file <F> [--back]
# Read only over MCP (the update node refuses MCP); what it checks: update_check.py.
set -euo pipefail
cd "$(dirname "$0")/../../.."
host="${1:?the machine: <host[:port]> <release dir> --tls auto --token-file <F>}"
rel="${2:?the release folder update-release wrote}"
shift 2
exec python3 -I host/tests/minipc/update_check.py "$host" "$rel" "$@"
