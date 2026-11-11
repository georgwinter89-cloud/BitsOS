#!/bin/bash
# LO-1: LibreOffice's Mini-PC run, by the desk against the real machine over MCP (TLS), never in QEMU (Georg 30.10.,
# CONTRACTS section 4). Usage: bash host/tests/minipc/lo.sh <host[:port]> --tls auto --token-file <F> [--desktop]
# Put \BITSOS2\libreoffice\ (with its PKGLIST.TXT) and \BITSOS2\bin\libreoffice on the stick first.
# What it checks: lo_check.py. What a person checks: plan/notes/lo-LO-1.md. Pictures and output: target/minipc/lo/.
set -euo pipefail
cd "$(dirname "$0")/../../.."
host="${1:?the Mini-PC: <host[:port]> --tls auto --token-file <F>}"
shift
exec python3 -I host/tests/minipc/lo_check.py "$host" target/minipc/lo "$@"
