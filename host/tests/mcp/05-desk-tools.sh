#!/bin/bash
# MC-11: the desk's tools without QEMU (desk_tools_check.mjs): mcp-call.mjs --tls waits while the service says
# "grants not loaded yet" (right after a start), and the Python helpers' mutual TLS (mcp_tls.py, lan_rate.py)
# with the key store and the pin.
set -eu -o pipefail
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/mc-05-desk-tools}"
mkdir -p "$RUN_DIR"
LO="${BITSOS_PORT_LO:-25000}"
command -v node > /dev/null || { echo "RED: node is needed (host/mcp's tools)"; exit 1; }
node host/tests/mcp/desk_tools_check.mjs "$(realpath "$RUN_DIR")/keys" $((LO + 170))
python3 -I -c "import ast; ast.parse(open('host/tests/nw/lan_rate.py').read())" || { echo "RED: lan_rate.py does not parse"; exit 1; }
grep -q 'p.add_argument("--tls"' host/tests/nw/lan_rate.py || { echo "RED: lan_rate.py has no --tls"; exit 1; }
echo "mc 05-desk-tools: green (grant wait in mcp-call.mjs, Python mutual TLS with pin, lan_rate.py --tls)"
