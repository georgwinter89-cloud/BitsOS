#!/usr/bin/env bash
# SX-3b: records bubblewrap's suites on CachyOS with the port's own stage (out/: the same bwrap, bash, helpers and
# patched suite V2 gets), as the unprivileged user, into host/tests/sx/expected/<suite>.tap. Run after build.sh;
# CachyOS must allow unprivileged user namespaces (kernel.unprivileged_userns_clone=1). As on V2 (host/tests/sx/
# v2_bwrap.py) each suite runs inside one outer bwrap (--unshare-user, same uid, --dev-bind / /): V2 needs it for a
# distribution-like /usr/bin (see ../README.md); here it changes nothing but makes the nesting the same.
set -euo pipefail
here=$(cd "$(dirname "$0")/.." && pwd)
out=$here/out
exp=$here/../../../host/tests/sx/expected
[ "$(id -u)" != 0 ] || { echo "record-host: run as an unprivileged user" >&2; exit 1; }
cd "$out"
for s in test-run test-specifying-pidns test-specifying-userns test-utils; do
  if [ $s = test-utils ]; then cmd=("$out/tests/test-utils"); else cmd=("$out/bash" "$out/tests/$s.sh"); fi
  { echo "# bubblewrap 0.13.1 $s on $(uname -sr), recorded by user/ports/bubblewrap/tests/record-host.sh"
    env -i HOME="$HOME" USER="$USER" PATH="$out:/usr/bin:/bin" BWRAP="$out/bwrap" G_TEST_SRCDIR="$out" "$out/bwrap" --unshare-user --uid "$(id -u)" --gid "$(id -g)" --dev-bind / / -- "${cmd[@]}" 2>/dev/null |
      grep -E '^(ok |not ok |1\.\.)'; } > "$exp/$s.tap"
  echo "record-host: $s: $(grep -c '^ok ' "$exp/$s.tap") ok lines"
done
