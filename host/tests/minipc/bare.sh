#!/bin/bash
# TS-1: the Mini-PC's ktest run for the tests that want the screen and the input to themselves: the script ends the
# desktop (`bitscomp ctl quit`, no reboot, no setting on the stick) and starts it again afterwards.
# Usage: bash host/tests/minipc/bare.sh <host[:port]> --tls auto --token-file <F> [--generation N] [--record]
# What it does and what is green: ktests.py.
set -euo pipefail
cd "$(dirname "$0")/../../.."
exec python3 -I host/tests/minipc/ktests.py bare "$@"
