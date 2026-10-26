#!/bin/bash
# RB-fl: the reload scenario's late checks (CONFIG, TSC re-check) read the new kernel's log over MCP. This checks
# their parsing offline against canned `log` answers in klogd's format, so a format change shows in the plain gate
# and not only in the slow 20-reload.sh run.
set -eu
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
python3 -I host/tests/rb/reload_scenario.py --selftest
