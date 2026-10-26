#!/bin/bash
# slow
# UL-4a: fetch on V2 against the same partner as 30-fetch-linux.sh: host/tests/ul/fetch_server.py on 127.0.0.1:28540
# (HTTPS) and 28541 (HTTP), which the guest reaches as 10.0.2.2 over QEMU's user network on a virtio card. The
# ul.fetch ktests run every line of user/tools/fetch/tests/cases.txt; ul.fetch.local and ul.fetch.cases must pass
# (no skip); ul.fetch.bundle may skip only while the image has no CA bundle (change request UL-4a CR1).
# tz=UTC: QEMU's RTC holds UTC, V2 reads it as Europe/Berlin local time by default (the Mini-PC's Windows
# convention), which would put the guest's clock two hours behind the certificates the partner just made.
set -eu
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
for t in python3 openssl; do command -v $t >/dev/null || { echo "SKIP: $t missing"; exit 0; }; done
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/ul-31-fetch}"
rm -rf "$RUN_DIR"; mkdir -p "$RUN_DIR/srv"
RUN_DIR="$(realpath "$RUN_DIR")"
SPID=""
stop() { rc=$?; if [ -n "$SPID" ]; then kill "$SPID" 2>/dev/null || true; wait "$SPID" 2>/dev/null || true; fi; exit $rc; }
trap stop EXIT
python3 -I host/tests/ul/fetch_server.py "$RUN_DIR/srv" 28540 28541 2> "$RUN_DIR/server.log" &
SPID=$!
for _ in $(seq 1 100); do [ -f "$RUN_DIR/srv/ready" ] && break; sleep 0.1; done
[ -f "$RUN_DIR/srv/ready" ] || { echo "RED: fetch_server.py did not start"; cat "$RUN_DIR/server.log"; exit 1; }

cargo xtask image --name ul-fetch --ktest --config-extra "ktest=ul.fetch;loglevel=info;tz=UTC"
cargo xtask qemu --name ul-fetch --net virtio --timeout 600 --serial-log "$RUN_DIR/serial.log" || true
grep -a "KTEST\|fetch\.case" "$RUN_DIR/serial.log" || true
for t in local cases; do
    grep -aq "KTEST ok ul.fetch.$t " "$RUN_DIR/serial.log" || { echo "RED: ul.fetch.$t did not pass"; exit 1; }
done
if grep -aq "KTEST FAIL" "$RUN_DIR/serial.log"; then echo "RED: a fetch test failed"; exit 1; fi
if grep -a "KTEST skip" "$RUN_DIR/serial.log" | grep -qv "ul.fetch.bundle: no /etc/ssl"; then echo "RED: a fetch test skipped"; exit 1; fi
echo "fetch on V2: green (every case of cases.txt as on CachyOS: HTTPS and HTTP by length, chunked and until close, redirects, errors, exit codes)"
