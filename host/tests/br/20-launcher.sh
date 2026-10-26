#!/usr/bin/env bash
# BR-2: the `brave` launcher (user/ports/brave-launcher/bitsos/brave) under BusyBox's sh on the build host, with a
# fake tree (a fake brave that answers LD_TRACE_LOADED_OBJECTS and writes into its --user-data-dir, a fake ld.so), a
# fake desktop (a process with XDG_RUNTIME_DIR and a real Wayland-named socket), fake mount tables and a fake
# pidof. Checks --check, the libraries, the desktop, the user switch, the three profile places (data volume, tar
# on the stick with .old fallback and unclean ends, RAM), the environment and flags, arguments and the exit code.
# Host only, a few seconds; nothing outside the run directory, no Brave, no QEMU, no screen.
set -uo pipefail
here=$(cd "$(dirname "$0")" && pwd)
wt=${BITSOS_ROOT:-$(cd "$here/../../.." && pwd)}
R=${BITSOS_RUN_DIR:-$wt/target/br/launcher}/fake
L=$wt/user/ports/brave-launcher/bitsos/brave
BB=$wt/user/ports/busybox/out/busybox
if [ ! -x "$BB" ]; then
  V2_OFFLINE=1 sh "$wt/user/ports/busybox/build.sh" >/dev/null 2>&1 || true
fi
[ -x "$BB" ] || { echo "SKIP: no BusyBox build ($BB)"; exit 0; }
rm -rf "$R"; mkdir -p "$R/bin" "$R/rootbin" "$R/xdg" "$R/tmp" "$R/user" "$R/data"
fails=0
ok() { echo "ok   $1"; }
bad() { echo "FAIL $1"; fails=$((fails + 1)); }
check() { if eval "$2"; then ok "$1"; else bad "$1"; echo "     output: $(tr '\n' '|' <"$R/out")"; fi; }
pids=
trap 'for p in $pids; do kill "$p" 2>/dev/null; done' EXIT

"$BB" --install -s "$R/bin" 2>/dev/null || { for a in $("$BB" --list); do ln -sf "$BB" "$R/bin/$a"; done; }
rm -f "$R/bin/pidof"
printf '#!%s sh\n[ "$1" = brave ] && exec %s pidof brave\n[ "$1" = bitscomp ] && [ -n "$FAKE_BITSCOMP" ] && echo "$FAKE_BITSCOMP"\nexit 0\n' "$BB" "$BB" >"$R/bin/pidof"
printf '#!%s sh\necho "${FAKE_UID:-0}"\n' "$BB" >"$R/rootbin/id"
printf '#!%s sh\n[ "$1" = -u ] && shift 2 && FAKE_UID=1000 exec "$@"\nexit 111\n' "$BB" >"$R/rootbin/chpst.ok"
chmod +x "$R/bin/pidof" "$R/rootbin/id" "$R/rootbin/chpst.ok"

T=$R/mnt/BITSOS2/brave
mkdir -p "$T/lib" "$T/locales" "$T/etc/fonts" "$T/share/glib-2.0/schemas" "$T/fonts" "$T/fontcache"
for f in resources.pak icudtl.dat v8_context_snapshot.bin locales/en-US.pak lib/libgtk-3.so.0 lib/libsoftokn3.so \
    lib/libfreeblpriv3.so etc/fonts/fonts.conf share/glib-2.0/schemas/gschemas.compiled fonts/a.ttf fonts/b.ttf \
    fontcache/x-le64.cache-12; do : >"$T/$f"; done
cat >"$T/brave" <<EOF
#!$BB sh
if [ "\$LD_TRACE_LOADED_OBJECTS" = 1 ]; then
  printf '\tlibnss3.so => $T/lib/libnss3.so (0x1)\n\tlibc.so.6 => /usr/lib/libc.so.6 (0x2)\n'
  [ -z "\$FAKE_MISSING" ] || printf '\tlibcups.so.2 => not found\n'
  exit 0
fi
d=
for a in "\$@"; do case \$a in --user-data-dir=*) d=\${a#--user-data-dir=} ;; esac; done
# As Chromium: a live lock link (host-pid) means a running browser, which gets this start's arguments.
if [ -L "\$d/SingletonLock" ]; then
  l=\$(readlink "\$d/SingletonLock")
  if kill -0 "\${l#host-}" 2>/dev/null; then echo "\$*" >"$R/forward.txt"; exit 0; fi
  echo stale >"$R/lock.txt"
else echo none >"$R/lock.txt"; fi
env | sort >"$R/env.txt"; echo "\$*" >"$R/args.txt"
n=\$(cat "\$d/Cookies" 2>/dev/null || echo 0); echo \$((n + 1)) >"\$d/Cookies"
mkdir -p "\$d/Default/Code Cache/js" "\$d/GrShaderCache"
: >"\$d/Default/Code Cache/js/x"; : >"\$d/GrShaderCache/y"; : >"\$d/Default/Preferences"
ln -sf "host-\$\$" "\$d/SingletonLock"
while [ -e "$R/hold" ]; do sleep 0.1; done
# The launcher gives Brave a clean environment: crash and exit code come from files.
[ ! -e "$R/crash" ] || kill -SEGV \$\$
exit \$(cat "$R/rc" 2>/dev/null || echo 0)
EOF
# ld.so --library-path DIR --list FILE: one library each, libc shared with brave's list.
cat >"$R/ldso" <<EOF
#!$BB sh
printf '\t%s-dep.so => $T/lib/%s-dep.so (0x3)\n\tlibc.so.6 => /usr/lib/libc.so.6 (0x2)\n' "\${4##*/}" "\${4##*/}"
EOF
chmod +x "$T/brave" "$R/ldso"
echo "/dev/sda1 $R/mnt vfat rw,fmask=0022 0 0" >"$R/mounts.rw"
echo "/dev/sda1 $R/mnt vfat ro,fmask=0022 0 0" >"$R/mounts.ro"
{ cat "$R/mounts.rw"; echo "/dev/nvme0n1p1 $R/data ext4 rw,relatime 0 0"; } >"$R/mounts.data"
{ cat "$R/mounts.rw"; echo "/dev/sdb1 $R/data vfat rw 0 0"; } >"$R/mounts.datafat"
P=$R/mnt/BITSOS2/home
PROF=$P/brave_profile.tar
RUN=$R/xdg/brave
# The desktop: a process whose environment names the runtime directory, and its socket.
python3 -I -c 'import socket, sys; socket.socket(socket.AF_UNIX).bind(sys.argv[1])' "$R/xdg/wayland-bits-0"
python3 -I -c 'import socket, sys; socket.socket(socket.AF_UNIX).bind(sys.argv[1])' "$R/xdg/wayland-x"
env -i XDG_RUNTIME_DIR="$R/xdg" "$BB" sleep 120 & desk=$!; pids="$pids $desk"
env -i XDG_RUNTIME_DIR="$R/xdg" "$BB" sh -c 'sleep 120; :' bitscomp --socket wayland-x & desk2=$!; pids="$pids $desk2"
sleep 0.2

launch() { env -i BRAVE_MOUNT="$R/mnt" BRAVE_MOUNTS="$R/mounts.${MNT:-rw}" BRAVE_PATH="${XPATH:-$R/bin}" BRAVE_ALSA_SYS=/nonexistent \
  BRAVE_TMP="$R/tmp" BRAVE_USER_RUN="$R/user" BRAVE_DATA="$R/data" BRAVE_LDSO="$R/ldso" XDG_RUNTIME_DIR="$R/xdg" \
  BRAVE_DATA_STATE="${STATE_F:-$R/nostate/data-state}" ${BRAVE_DATA_WAIT:+BRAVE_DATA_WAIT=$BRAVE_DATA_WAIT} ${BRUN:+BRAVE_RUN=$BRUN} \
  ${WD:+WAYLAND_DISPLAY=$WD} FAKE_BITSCOMP="${DESK-$desk}" FAKE_MISSING="${FAKE_MISSING:-}" ${BRAVE_WAIT:+BRAVE_WAIT=$BRAVE_WAIT} ${THEME_T:+BRAVE_THEME=$THEME_T} LANG=de_DE.UTF-8 PATH="$R/bin" \
  "$BB" sh "$L" "$@" >"$R/${OUT:-out}" 2>&1; }
cookie() { "$BB" tar -xOf "${1:-$PROF}" profile/Cookies 2>/dev/null; }
has() { grep -q -- "$1" "$R/args.txt"; }
envv() { grep -qx -- "$1" "$R/env.txt"; }

launch --check; rc=$?
check "--check passes on a whole tree" '[ $rc = 0 ] && grep -qx "brave: tree $T: ok" "$R/out"'
check "--check counts distinct libraries over brave and the dlopened ones" 'grep -qx "brave: libraries: 5 found, none missing" "$R/out"'
check "--check: fonts, desktop, user, sound" 'grep -qx "brave: fonts: 2 in $T/fonts, 1 prebuilt cache file(s)" "$R/out" && grep -qx "brave: desktop: $R/xdg/wayland-bits-0" "$R/out" && grep -qx "brave: user: runs as uid $(id -u)" "$R/out" && grep -qx "brave: sound: no ALSA configuration (/nonexistent)" "$R/out"'
check "--check: no data volume, no home yet: RAM only" 'grep -q "^brave: profile: in RAM only ($RUN/profile): no data volume at $R/data and $P is not writable for uid" "$R/out"'
FAKE_MISSING=1 launch --check; rc=$?
check "--check fails on a missing library" '[ $rc = 1 ] && grep -qx "brave: libraries missing: libcups.so.2 => not found" "$R/out"'
mv "$T/lib/libgtk-3.so.0" "$R/gtk"; launch --check; rc=$?
check "--check fails on a missing file" '[ $rc = 1 ] && grep -q "^brave: tree $T: lib/libgtk-3.so.0 is missing" "$R/out"'
mv "$R/gtk" "$T/lib/libgtk-3.so.0"
DESK= launch --check; rc=$?
check "--check without a desktop still passes and says so" '[ $rc = 0 ] && grep -qx "brave: desktop: none runs (start the desktop first)" "$R/out"'
DESK="$desk $desk2" launch --check
check "two desktops are named, not guessed" 'grep -qx "brave: desktop: 2 desktops run" "$R/out"'
DESK=$desk2 launch --check
check "the desktop's --socket is read from its start line" 'grep -qx "brave: desktop: $R/xdg/wayland-x" "$R/out"'
DESK= WD=wayland-x launch --check
check "WAYLAND_DISPLAY of a terminal of the desktop is taken first" 'grep -qx "brave: desktop: $R/xdg/wayland-x" "$R/out"'
DESK= launch; rc=$?
check "no desktop: nothing starts" '[ $rc = 1 ] && grep -qx "brave: desktop: none runs (start the desktop first)" "$R/out" && [ ! -e "$R/args.txt" ]'

# Root: refused without an id change; with one, Brave's user is uid 1000.
XPATH="$R/rootbin:$R/bin" launch --check; rc=$?
check "as root without id change: --check says refused" '[ $rc = 1 ] && grep -q "^brave: user: runs as uid 0: refused, root cannot hand over to uid 1000" "$R/out"'
XPATH="$R/rootbin:$R/bin" launch; rc=$?
check "as root without id change: refused in German and English" '[ $rc = 1 ] && grep -q "^brave: Brave startet nicht als root: " "$R/out" && grep -q "^brave: Brave does not run as root: " "$R/out" && [ ! -e "$R/args.txt" ]'
cp "$R/rootbin/chpst.ok" "$R/rootbin/chpst"
XPATH="$R/rootbin:$R/bin" launch --check; rc=$?
check "as root with id change: --check says uid 1000, the stick's tar profile" '[ $rc = 0 ] && grep -qx "brave: user: runs as uid 1000 (started by root: chpst -u 1000:1000)" "$R/out" && grep -q "^brave: profile: $PROF (packed back" "$R/out"'
rm -f "$R/rootbin/chpst"

# The data volume: the profile on it, the page cache with it, Brave's environment and flags.
MNT=data launch http://127.0.0.1:31001/ -- --lang=de; rc=$?
check "data volume: the profile in appdata" '[ $rc = 0 ] && grep -qx "brave: profile: $R/data/appdata/brave" "$R/out" && has "--user-data-dir=$R/data/appdata/brave" && ! has "--disk-cache-dir" && [ "$(cat "$R/data/appdata/brave/Cookies")" = 1 ]'
check "BR-1's flags, the URL and the extra arguments" 'has "^--ozone-platform=wayland --disable-gpu --password-store=basic --no-sandbox --disable-breakpad --disable-crash-reporter --no-first-run --no-default-browser-check --autoplay-policy=no-user-gesture-required --user-data-dir=" && has " http://127.0.0.1:31001/ --lang=de$"'
check "a clean environment pointing into the tree" 'envv "LD_LIBRARY_PATH=$T/lib" && envv "FONTCONFIG_FILE=$T/etc/fonts/fonts.conf" && envv "WAYLAND_DISPLAY=wayland-bits-0" && envv "XDG_RUNTIME_DIR=$R/xdg" && envv "HOME=$RUN/home" && envv "GSETTINGS_SCHEMA_DIR=$T/share/glib-2.0/schemas" && envv "XCURSOR_PATH=$T/share/icons" && envv "LANG=de_DE.UTF-8" && ! grep -q "^FAKE_\|^BRAVE_" "$R/env.txt"'
check "light or unknown theme: no forced dark mode" '! has "--force-dark-mode"'
MNT=data launch; rc=$?
check "data volume: the lock link of a Brave that is gone is dropped" '[ $rc = 0 ] && [ "$(cat "$R/lock.txt")" = none ] && [ "$(cat "$R/data/appdata/brave/Cookies")" = 2 ] && [ ! -e "$RUN/.lock" ]'
THEME_T=dark MNT=data launch http://127.0.0.1:31001/; rc=$?
check "dark desktop theme: Brave and web pages dark (--force-dark-mode)" '[ $rc = 0 ] && has " --force-dark-mode"'
MNT=datafat launch --check
check "a vfat /data is no data volume" 'grep -q "^brave: profile: $PROF (packed back after a clean end; $R/data is vfat)" "$R/out" || grep -q "^brave: profile: in RAM only .*$R/data is vfat" "$R/out"'

# The stick: one tar file, renames, the copy before (.old), stale lock links dropped, unclean ends not saved.
mkdir -p "$P"
launch; rc=$?
check "stick: first start packs the profile" '[ $rc = 0 ] && grep -q "^brave: profile: new" "$R/out" && grep -q "^brave: profile saved on the stick" "$R/out" && [ "$(cookie)" = 1 ] && has "--disk-cache-dir=$RUN/cache" && has "--user-data-dir=$RUN/profile"'
rm -rf "$RUN"
launch; rc=$?
check "stick: taken back after RAM is gone, stale SingletonLock dropped" '[ $rc = 0 ] && grep -qx "brave: profile: taken from the stick (brave_profile.tar)" "$R/out" && [ "$(cat "$R/lock.txt")" = none ] && [ "$(cookie)" = 2 ] && [ "$(cookie "$PROF.old")" = 1 ]'
rm -f "$PROF"; rm -rf "$RUN"
launch; rc=$?
check "stick: the copy before is taken when the copy is gone" '[ $rc = 0 ] && grep -qx "brave: profile: taken from the stick (brave_profile.tar.old)" "$R/out" && [ "$(cookie)" = 2 ]'
: >"$R/crash"; launch; rc=$?; rm -f "$R/crash"
check "a crash keeps the stick's copy and marks RAM's as unsaved" '[ $rc = 139 ] && grep -q "^brave: profile not saved: Brave ended by signal 11" "$R/out" && [ "$(cookie)" = 2 ] && [ -e "$RUN/.unsaved" ]'
launch; rc=$?
check "the unsaved RAM profile is newer and is used" '[ $rc = 0 ] && grep -qx "brave: profile: the one from the last start in RAM (not on the stick)" "$R/out" && [ "$(cookie)" = 4 ] && [ ! -e "$RUN/.unsaved" ]'
echo 3 >"$R/rc"; launch; rc=$?; rm -f "$R/rc"
check "Brave's exit code is the launcher's" '[ $rc = 3 ] && grep -q "^brave: Brave ended (code 3)" "$R/out"'

# One Brave per profile: a second start hands its URL over and leaves the first one's profile alone.
: >"$R/hold"; rm -f "$R/forward.txt"
OUT=out1 launch & l1=$!; pids="$pids $l1"
# The fake Brave runs once its lock link names a live pid (the restored copy's link is a dead one).
live() { l=$(readlink "$RUN/profile/SingletonLock" 2>/dev/null) && kill -0 "${l#host-}" 2>/dev/null; }
n=0; until live || [ $n -gt 100 ]; do sleep 0.1; n=$((n + 1)); done
c1=$(cat "$RUN/profile/Cookies" 2>/dev/null)
launch http://127.0.0.1:31002/ -- --new-window; rc=$?
check "a second start hands its URL to the running Brave" '[ $rc = 0 ] && grep -q "^brave: Brave runs already (pid [0-9]*): http://127.0.0.1:31002/ goes to it (profile $RUN/profile)" "$R/out" && grep -q -- "--user-data-dir=$RUN/profile http://127.0.0.1:31002/ --new-window$" "$R/forward.txt"'
check "the second start leaves the running profile alone" '! grep -q "profile: taken from\|profile: new\|profile saved\|taken over" "$R/out" && [ -L "$RUN/profile/SingletonLock" ] && [ "$(cat "$RUN/profile/Cookies")" = "$c1" ] && [ -d "$RUN/.lock" ]'
launch --check
check "--check names the running Brave" 'grep -q "^brave: running: pid [0-9]* (profile $RUN/profile)" "$R/out"'
rm -f "$R/hold"; wait $l1; rc=$?
check "the first start alone packs the profile, without caches, and frees the lock" '[ $rc = 0 ] && grep -q "^brave: profile saved on the stick" "$R/out1" && [ ! -e "$RUN/.lock" ] && "$BB" tar -tf "$PROF" | grep -q "^profile/Default/Preferences$" && ! "$BB" tar -tf "$PROF" | grep -q "Code Cache\|GrShaderCache"'
# A lock whose start is gone is taken over; a live start without Brave makes the new one give up.
"$BB" sh -c 'exit 0' & dead=$!; wait $dead
mkdir -p "$RUN/.lock"; echo $dead >"$RUN/.lock/pid"
launch; rc=$?
check "a stale lock is taken over" '[ $rc = 0 ] && grep -qx "brave: the lock of a start that is gone (pid $dead) is taken over" "$R/out" && [ ! -e "$RUN/.lock" ]'
"$BB" sh -c 'sleep 120; :' "$L" & holder=$!; pids="$pids $holder"
mkdir -p "$RUN/.lock"; echo $holder >"$RUN/.lock/pid"
BRAVE_WAIT=1 launch; rc=$?
check "a live start without Brave: nothing started" '[ $rc = 1 ] && grep -qx "brave: another start (pid $holder) holds $RUN/.lock, but no Brave runs: nothing started" "$R/out"'
kill $holder; rm -rf "$RUN/.lock"
MNT=ro launch; rc=$?
check "a read-only stick: RAM only, nothing written" '[ $rc = 0 ] && grep -q "^brave: profile: in RAM only: no data volume at $R/data and the stick is not mounted writable" "$R/out" && ! grep -q "saved on the stick" "$R/out"'

# INT-W11: /data still coming (the kernel's state line), the stick's tar taken over once, BRAVE_RUN.
mkdir -p "$R/state"; echo creating >"$R/state/data-state"
STATE_F="$R/state/data-state" launch; rc=$?
check "data volume being made: no wait, the stick's profile" '[ $rc = 0 ] && grep -qx "brave: profile: data volume is being made; the stick'"'"'s profile is used this time" "$R/out" && grep -q "^brave: profile: taken from the stick" "$R/out"'
echo waiting >"$R/state/data-state"
STATE_F="$R/state/data-state" BRAVE_DATA_WAIT=2 launch; rc=$?
check "data volume waiting: a bounded wait, then the stick" '[ $rc = 0 ] && grep -qx "brave: profile: waiting for the data volume (waiting)" "$R/out" && grep -qx "brave: profile: /data is still not there (waiting); not waiting longer" "$R/out"'
rm -f "$R/state/data-state"
STATE_F="$R/state/data-state" BRAVE_DATA_WAIT=2 launch --check; rc=$?
check "--check never waits" '[ $rc = 0 ] && ! grep -q "waiting for the data volume" "$R/out"'
want=$(cookie)
rm -rf "$R/data/appdata/brave"; mkdir -p "$R/data/appdata/brave"
MNT=data launch; rc=$?
check "data volume: the stick's tar taken over once and renamed" '[ $rc = 0 ] && grep -qx "brave: profile: the stick'"'"'s brave_profile.tar taken over into $R/data/appdata/brave (now brave_profile.tar.imported)" "$R/out" && [ ! -e "$PROF" ] && [ -f "$PROF.imported" ] && [ "$(cat "$R/data/appdata/brave/Cookies")" = $((want + 1)) ] && [ ! -e "$R/data/appdata/brave/.import" ]'
MNT=data launch; rc=$?
check "data volume: a later start takes nothing over" '[ $rc = 0 ] && ! grep -q "taken over" "$R/out" && [ "$(cat "$R/data/appdata/brave/Cookies")" = $((want + 2)) ]'
MNT=data BRUN="$R/pkgrun" launch; rc=$?
check "BRAVE_RUN puts HOME, log and lock there" '[ $rc = 0 ] && envv "HOME=$R/pkgrun/home" && [ -f "$R/pkgrun/brave.log" ]'

if [ $fails = 0 ]; then echo "launcher: all checks passed"; rm -rf "$R"; exit 0; fi
echo "launcher: $fails checks failed (run dir $R)"; exit 1
