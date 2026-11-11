#!/usr/bin/env bash
# GM-2a: the staged steam tree (user/ports/steam/scripts/stage-v2.sh) on the build host, without running any of it: the pinned
# versions, every package by the manifest's size and sha2, no link (FAT has none), every library the client's ELF files name
# found where they run (scripts/verify-tree.py), and the launcher's tools take Valve's real files (BusyBox tar and unzip, as on V2).
# RED without a stage: a test that cannot run is not green (CONTRACTS section 4). 1.5 GB of scratch in the run directory, removed.
set -uo pipefail
here=$(cd "$(dirname "$0")" && pwd)
wt=${BITSOS_ROOT:-$(cd "$here/../../.." && pwd)}
S=${STEAM_STAGE:-/var/tmp/claude-v2/stage/steam}
RUN_DIR=${BITSOS_RUN_DIR:-$wt/target/sx/steam-stage}
BB=$wt/user/ports/busybox/out/busybox
[ -f "$S/VERSION.txt" ] || { echo "RED: no stage at $S (bash user/ports/steam/scripts/stage-v2.sh makes it)"; exit 1; }
[ -x "$BB" ] || V2_OFFLINE=1 sh "$wt/user/ports/busybox/build.sh" >/dev/null 2>&1 || true
[ -x "$BB" ] || { echo "RED: no BusyBox build ($BB)"; exit 1; }
rm -rf "$RUN_DIR/work" "$RUN_DIR/tools"; mkdir -p "$RUN_DIR/tools"
fails=0
check() { if eval "$2"; then echo "ok   $1"; else echo "FAIL $1"; fails=$((fails + 1)); fi; }
P=$wt/user/ports/steam/pins.txt
read -r _ deb deb_sha <<<"$(grep '^launcher ' "$P")"
read -r _ m_name m_ver m_sha <<<"$(grep '^manifest ' "$P")"
check "pins.txt names a launcher and a manifest" '[ -n "$deb_sha" ] && [ -n "$m_sha" ]'
check "VERSION.txt is the pinned launcher and client" 'grep -q "^steam-launcher $deb $deb_sha\$" "$S/VERSION.txt" && grep -q "^client publicbeta $m_ver $m_sha\$" "$S/VERSION.txt"'
check "the manifest in the tree is the pinned one" '[ "$(sha256sum "$S/package/$m_name.manifest" | cut -d" " -f1)" = "$m_sha" ]'
check "no credentials or home paths in the tree's text files" '! grep -qEi "password|/home/" "$S/VERSION.txt"'
# With the deb's and the manifest's pins this closes the chain: gfx.tar.gz by the sum VERSION.txt holds, realpath is the one in git.
check "the provider in the tree is the one VERSION.txt names" '[ "$(sha256sum "$S/gfx.tar.gz" | cut -d" " -f1)" = "$(sed -n "s/^gfx [^ ]* //p" "$S/VERSION.txt")" ]'
check "the realpath in the tree is the one in git" 'cmp -s "$S/bin/realpath" "$wt/user/ports/steam/files/realpath"'
python3 -I "$wt/user/ports/steam/scripts/verify-tree.py" "$S" "$wt/user/packaging/libset.txt" "$RUN_DIR/work" --report "$RUN_DIR/elf.txt" >"$RUN_DIR/verify.out" 2>&1
rc=$?
check "every library the client's ELF files name is there: $(tail -n 1 "$RUN_DIR/verify.out")" '[ $rc = 0 ]'
[ $rc = 0 ] || grep -E '^(missing|RED)' "$RUN_DIR/verify.out" | head -40
check "BusyBox tar unpacks Valve's bootstrap, steam.sh with the 64-bit branch" '"$BB" tar -xJf "$S/bootstraplinux_ubuntu12_32.tar.xz" -C "$RUN_DIR/tools" && grep -q "steamrt64/steam" "$RUN_DIR/tools/steam.sh" && grep -q ".steam-enable-steamrt64-client" "$RUN_DIR/tools/steam.sh"'
check "BusyBox unzip gives steamrt64/steam, an x86-64 ELF" '"$BB" unzip -o -q "$S"/package/steam_steamrt_ubuntu12.zip.* steamrt64/steam -d "$RUN_DIR/tools" && [ "$(head -c 5 "$RUN_DIR/tools/steamrt64/steam" | od -An -tx1 | tr -d " ")" = 7f454c4602 ]'
check "BusyBox tar unpacks the provider, with its links" 'mkdir -p "$RUN_DIR/tools/g" && "$BB" tar -xzf "$S/gfx.tar.gz" -C "$RUN_DIR/tools/g" && [ -f "$RUN_DIR/tools/g/gfx/usr/lib/libc.so.6" ] && [ -L "$RUN_DIR/tools/g/gfx/lib64" ]'
# Its own PATH: no desktop that happens to run on this machine is asked anything.
mkdir -p "$RUN_DIR/x"
for a in $("$BB" --list); do ln -sf "$BB" "$RUN_DIR/x/$a"; done
rm -f "$RUN_DIR/x/pidof"; printf '#!%s sh\nexit 0\n' "$BB" >"$RUN_DIR/x/pidof"; chmod +x "$RUN_DIR/x/pidof"
out=$(env -i PATH="$RUN_DIR/x" SB_PATH="$RUN_DIR/x" SB_TREE="$S" SB_BASH=/usr/bin/bash SB_X11_DIR=/nonexistent SB_MOUNTS=/dev/null SB_DATA_STATE=/nonexistent "$BB" sh "$wt/user/ports/steam/bitsos/steam" --check 2>&1)
check "the launcher finds this tree complete" 'grep -q "tree $S: ok, publicbeta $m_ver" <<<"$out"'
rm -rf "$RUN_DIR/tools" "$RUN_DIR/x"
[ $fails = 0 ] || { echo "RED: $fails"; exit 1; }
echo "GREEN: the steam stage"
