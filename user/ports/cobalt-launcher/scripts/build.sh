#!/usr/bin/env bash
# Incremental build in the lane's container (cobalt-yt-v2, the copy mounted
# at /work). At most 8 jobs, low priority. Usage: build.sh [targets...]
# (default: loader_app). Log: <copy>/logs/build-yt-<date>.log
set -euo pipefail
targets=${*:-loader_app}
log=/work/logs/build-yt-$(date +%m%d-%H%M%S).log
podman start cobalt-yt-v2 >/dev/null
podman exec cobalt-yt-v2 bash -c ". /work/env.sh; cd /work/cobalt/src; \
  nice -n 10 autoninja -C out/evergreen-x64-bitsos_qa -j8 $targets > $log 2>&1; rc=\$?; \
  tail -3 $log; echo BUILD_RC=\$rc; exit \$rc"
