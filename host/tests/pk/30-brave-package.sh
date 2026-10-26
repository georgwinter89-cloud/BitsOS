#!/bin/bash
# slow
# PK-4: Brave as a package on CachyOS. user/tools/bitspkg/brave/make.sh builds it from BR-2's stick tree
# (/var/tmp/claude-v2/stage/brave, skipped when it is not there), then it is installed into a scratch root,
# verified (every hash, the store), its launcher checks the tree and every library from the package
# (`bitsos/run --check`: tree ok, libraries none missing; no window, no desktop needed), and `remove brave` leaves
# the scratch root as before. About 600 MB in target/, a few seconds.
set -euo pipefail
ROOT="${BITSOS_ROOT:-$(cd "$(dirname "$0")/../../.." && pwd)}"
RUN="${BITSOS_RUN_DIR:-$ROOT/target/xtask/runs/pk-30-brave-package}"
STAGE="${BRAVE_STAGE:-/var/tmp/claude-v2/stage/brave}"
[ -x "$STAGE/brave" ] || { echo "SKIP: no Brave stick tree at $STAGE (user/ports/brave-launcher/scripts/stage-v2.sh)"; exit 0; }
command -v ldd >/dev/null || { echo "SKIP: no ldd"; exit 0; }
rm -rf "$RUN"
mkdir -p "$RUN"
(cd "$ROOT/user" && env -u CARGO_TARGET_DIR -u RUSTUP_TOOLCHAIN cargo build -q --offline --release -p bitspkg --target-dir "$ROOT/target/user-host")
B="$ROOT/target/user-host/release/bitspkg"
BITSPKG=$B bash "$ROOT/user/tools/bitspkg/brave/make.sh" "$STAGE" "$RUN/brave" > "$RUN/make.log" 2>&1 || { tail -20 "$RUN/make.log"; echo "RED: make.sh"; exit 1; }
tail -2 "$RUN/make.log"
R="$RUN/root"
# Folder sizes are left out (btrfs counts the names in them, and apps/.next-id stays: ProgramIds are never reused).
list() { (cd "$R" && { find . -path ./apps/.next-id -prune -o -type f -printf '%p f %m %s\n'; find . ! -type f -printf '%p %y %m %l\n'; } | sort); }
"$B" --root "$R" gc > /dev/null
list > "$RUN/before.txt"
"$B" --root "$R" install "$RUN/brave/pkg" || { echo "RED: install"; exit 1; }
"$B" --root "$R" verify --store || { echo "RED: verify"; exit 1; }
"$B" --root "$R" info brave | head -12
mkdir -p "$RUN/fake"
echo "/dev/vdb $R/data ext4 rw,relatime 0 0" > "$RUN/fake/mounts"
set +e
# No desktop is looked at (never Georg's: WAYLAND_DISPLAY and XDG_RUNTIME_DIR are dropped).
env -u WAYLAND_DISPLAY -u XDG_RUNTIME_DIR BRAVE_DATA="$R/data" BRAVE_MOUNTS="$RUN/fake/mounts" BRAVE_USER_RUN="$RUN/fake/run" BRAVE_TMP="$RUN/fake" \
  "$R/apps/brave/current/bitsos/run" --check > "$RUN/check.txt" 2>&1
rc=$?
set -e
cat "$RUN/check.txt"
[ $rc = 0 ] || { echo "RED: brave --check from the package exited $rc"; exit 1; }
grep -q "^brave: tree $R/apps/brave/current: ok" "$RUN/check.txt" || { echo "RED: the launcher does not read the package"; exit 1; }
grep -q "none missing" "$RUN/check.txt" || { echo "RED: libraries"; exit 1; }
grep -q "^brave: profile: $R/data/appdata/brave (data volume)" "$RUN/check.txt" || { echo "RED: profile not in the package's data folder"; exit 1; }
grep -q '<dir>/apps/brave/current/fonts</dir>' "$R/apps/brave/current/etc/fonts/fonts.conf" || { echo "RED: fonts.conf"; exit 1; }
"$B" --root "$R" remove brave
list > "$RUN/after.txt"
diff "$RUN/before.txt" "$RUN/after.txt" || { echo "RED: remove brave left something"; exit 1; }
rm -rf "$RUN/brave" "$R"
echo "Brave package on CachyOS: green (built, installed, checked from the package, removed without leftovers)"
