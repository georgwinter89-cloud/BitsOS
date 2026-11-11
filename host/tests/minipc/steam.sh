#!/bin/bash
# GM-2a: the Steam client up to its login window, by the desk against the real machine over MCP (TLS), never in QEMU and
# never on the build machine (Georg 30.10., CONTRACTS section 4).
# Usage: bash host/tests/minipc/steam.sh <host[:port]> --tls auto --token-file <F> [--secs N] [--steam-args "A B"] [--stop]
# Put \BITSOS2\steam\ (with its PKGLIST.TXT) and \BITSOS2\bin\steam on the stick first; the data volume must be mounted, the
# desktop running. No credentials: it stops at the login window, Georg logs in himself. Expect the first runs to fail: it
# says the stage reached and why. What it does: steam_check.py. Pictures and logs: target/minipc/steam/.
set -euo pipefail
cd "$(dirname "$0")/../../.."
host="${1:?the Mini-PC: <host[:port]> --tls auto --token-file <F>}"
shift
exec python3 -I host/tests/minipc/steam_check.py "$host" target/minipc/steam "$@"
