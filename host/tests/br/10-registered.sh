#!/usr/bin/env bash
# BR-1: the syscalls Brave calls (host/tests/br/ref, recorded on CachyOS) against V2's registrations.
# Host only, about a second, no Brave and no QEMU needed. Fails when a call the forecast counts as registered
# lost its registration; names that got registered since only print a hint to update plan/notes/br-BR-1.md.
set -euo pipefail
ROOT=${BITSOS_ROOT:-$(cd "$(dirname "$0")/../../.." && pwd)}
exec python3 -I "$ROOT/host/tests/br/tools/check-registered.py" "$ROOT"
