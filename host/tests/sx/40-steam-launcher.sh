#!/usr/bin/env bash
# GM-2a: the `steam` launcher (user/ports/steam/bitsos/steam) under BusyBox's sh on the build host: a fake tree (a fake steam.sh
# in a real bootstrap tar, a real zip, a real provider tar), fake mount tables, a fake desktop and a fake /proc. --env (the pure
# environment), --check and what it names, root's hand-over, the first and second start with their stages and exit code,
# --status, one start at a time, the end after a signal, --stop; GM-2b: the stand-in over steamrt64/reaper, on the fake tree (a fake
# stand-in: the real one has its own tests, user/ports/steam/reaper/tests); files/realpath; then steam_check.py against a stand-in
# for the machine (host/tests/sx/steam_stand_in.py); GM-PROTON: PROTON_USE_WOW64=1 in the environment, --check's Proton lines.
# Host only, a few seconds; nothing of Steam runs (Georg 30.10.), no QEMU, no screen. Red when it cannot do its work.
set -uo pipefail
here=$(cd "$(dirname "$0")" && pwd)
wt=${BITSOS_ROOT:-$(cd "$here/../../.." && pwd)}
RUN_DIR=${BITSOS_RUN_DIR:-$wt/target/sx/steam-launcher}
R=$RUN_DIR/fake
L=$wt/user/ports/steam/bitsos/steam
BB=$wt/user/ports/busybox/out/busybox
if [ ! -x "$BB" ]; then
  V2_OFFLINE=1 sh "$wt/user/ports/busybox/build.sh" >/dev/null 2>&1 || true
fi
[ -x "$BB" ] || { echo "RED: no BusyBox build ($BB): user/ports/busybox/build.sh"; exit 1; }
for t in xz python3; do command -v $t >/dev/null || { echo "RED: no $t on this host"; exit 1; }; done
rm -rf "$R"; mkdir -p "$R/bin" "$R/rootbin" "$R/run" "$R/data/home/1000" "$R/proc" "$R/x11" "$R/xdg" "$R/user"
fails=0
check() { if eval "$2"; then echo "ok   $1"; else echo "FAIL $1"; fails=$((fails + 1)); echo "     output: $(tr '\n' '|' <"$R/out" | cut -c1-600)"; fi; }

for a in $("$BB" --list); do ln -sf "$BB" "$R/bin/$a"; done
rm -f "$R/bin/pidof" "$R/bin/id" "$R/bin/chpst" "$R/bin/df"
printf '#!%s sh\n[ "$1" = bitscomp ] && [ -n "$FAKE_BITSCOMP" ] && echo "$FAKE_BITSCOMP"\nexit 0\n' "$BB" >"$R/bin/pidof"
printf '#!%s sh\n[ "$1" != -G ] || { echo "${FAKE_GROUPS:-1000}"; exit 0; }\necho "${FAKE_UID:-1000}"\n' "$BB" >"$R/bin/id"
printf '#!%s sh\n[ -z "$FAKE_NO_DROP" ] || exit 111\n[ "$1" = -u ] && shift 2 && FAKE_UID=1000 exec "$@"\nexit 111\n' "$BB" >"$R/bin/chpst"
printf '#!%s sh\necho "Filesystem 1K-blocks Used Available Use%% Mounted"\necho "/dev/x 99999999 1 ${FAKE_FREE:-90000000} 1%% /data"\n' "$BB" >"$R/bin/df"
printf '#!%s sh\ncase "$*" in\n  "ctl xwayland") echo "${FAKE_XW:-xwayland display=:0 state=idle pid=0 starts=0}" ;;\n  "ctl list") echo "toplevel app_id=steam x11=0x1" ;;\n  *) exit 2 ;;\nesac\n' "$BB" >"$R/bin/bitscomp"
printf '#!%s sh\nexec %s sh "$@"\n' "$BB" "$BB" >"$R/bash"
chmod +x "$R/bin/pidof" "$R/bin/id" "$R/bin/chpst" "$R/bin/df" "$R/bin/bitscomp" "$R/bash"
# The desktop's X socket and (below) its Wayland socket: real sockets, as the launcher's -S wants.
python3 -I - "$R" <<'PY'
import socket, sys
s = socket.socket(socket.AF_UNIX)
s.bind(sys.argv[1] + "/x11/X0")
PY

# The fake tree: real archive formats, fake contents.
T=$R/mnt/BITSOS2/steam
X=$R/src
mkdir -p "$T/package" "$T/bin" "$X/boot/ubuntu12_32" "$X/zip/steamrt64" "$X/gfx/gfx/usr/lib"
# A fake dbus-daemon behind a fake Mesa ld.so: it makes the socket it is told to and stays.
mkdir -p "$R/mesa/lib" "$T/dbus"
cat >"$R/mesa/lib/ld-linux-x86-64.so.2" <<'FAKE'
#!/usr/bin/python3 -I
import signal, socket, sys
a = [x for x in sys.argv if x.startswith("--address=unix:path=")][0].split("=", 2)[2]
socket.socket(socket.AF_UNIX).bind(a)
signal.pause()
FAKE
chmod 755 "$R/mesa/lib/ld-linux-x86-64.so.2"
echo fake >"$T/dbus/dbus-daemon"
echo fake >"$T/dbus/session.conf"
cat >"$X/boot/steam.sh" <<'S'
if [ -e "$PWD/fake-hold" ]; then
  # A Steam that takes a moment to end, and says when it has.
  trap 'sleep 1; touch "$PWD/term-done"; exit 9' TERM
  echo $$ >"$PWD/up.new"; mv "$PWD/up.new" "$PWD/up"
  n=0; while [ $n -lt 300 ]; do sleep 0.1; n=$((n + 1)); done
  exit 3
fi
# A client whose web helper comes up: the launcher's moment for the stand-in.
if [ -e "$PWD/fake-proc" ]; then
  P=$(cat "$PWD/fake-proc")
  mkdir -p "$P/7777"; echo steamwebhelper >"$P/7777/comm"
  sleep 1
  rm -rf "$P/7777"
fi
echo "fake steam.sh args: $*"
env | sort > "$PWD/env-steam-saw"
touch "$PWD/ran-$$"
exit "$(cat "$PWD/fake-rc" 2>/dev/null || echo 0)"
S
echo fake >"$X/boot/ubuntu12_32/steam"
tar -C "$X/boot" -cJf "$T/bootstraplinux_ubuntu12_32.tar.xz" .
printf 'fake elf\n' >"$X/zip/steamrt64/steam"
python3 -I -c 'import sys, zipfile; z = zipfile.ZipFile(sys.argv[1], "w", zipfile.ZIP_DEFLATED); z.write(sys.argv[2], "steamrt64/steam"); z.close()' "$T/package/steam_steamrt_ubuntu12.zip.abc" "$X/zip/steamrt64/steam"
echo other >"$T/package/bins_steamrt_ubuntu12.zip.def"
echo manifest >"$T/package/steam_client_publicbeta_ubuntu12.manifest"
echo libc >"$X/gfx/gfx/usr/lib/libc.so.6"
tar -C "$X/gfx" -czf "$T/gfx.tar.gz" gfx
cp "$wt/user/ports/steam/files/realpath" "$T/bin/realpath"
printf 'steam-launcher d 1\nclient publicbeta 123 abc\nruntime steamrt3c 3c\ngfx user/ports/steamrt sum1\n' >"$T/VERSION.txt"

# The stand-in's source, as /bin/reaper on the image; what Valve's and another 64-bit file begin with.
printf '\177ELF\002 the stand-in\n' >"$R/standin"; chmod 644 "$R/standin"
printf '\177ELF\001 valve reaper\n' >"$R/original"
printf '\177ELF\002 older stand-in\n' >"$R/older"
M_EXT4=$R/mounts.ext4 M_VFAT=$R/mounts.vfat M_NONE=$R/mounts.none M_RO=$R/mounts.ro
echo "/dev/x $R/data ext4 rw,relatime 0 0" >"$M_EXT4"
echo "/dev/x $R/data vfat rw,relatime 0 0" >"$M_VFAT"
echo "rootfs / tmpfs rw 0 0" >"$M_NONE"
echo "/dev/x $R/data ext4 ro,relatime 0 0" >"$M_RO"
python3 -I - "$R/xdg/wayland-bits-0" <<'PY'
import socket, sys
s = socket.socket(socket.AF_UNIX)
s.bind(sys.argv[1])
PY
launch() {
  exec env -i PATH="$R/bin" SB_TREE="$T" SB_DATA="$R/data" SB_MOUNTS="${MOUNTS:-$M_EXT4}" SB_DATA_STATE="$R/run/data-state" SB_USER_RUN="$R/user" \
    SB_BASH="$R/bash" SB_X11_DIR="$R/x11" SB_PATH="$R/bin" SB_PROC="$R/proc" SB_REAPER="$R/standin" SB_REAPER_WAIT=0.1 STEAM_SILENT=1 SB_MESA="$R/mesa" XDG_RUNTIME_DIR="$R/xdg" WAYLAND_DISPLAY=wayland-bits-0 \
    "$@" "$BB" sh "$L" $ARGS
}
run() { # FAKE_* and SB_* in the environment; output in $R/out, code in $rc
  (launch "$@") >"$R/out" 2>&1
  rc=$?
}
e() { grep -q "^$1\$" "$R/out"; }

ARGS=--env; run
check "--env: HOME on the data volume, no start" 'e "HOME=$R/data/home/1000" && [ ! -e "$R/data/home/1000/.local" ]'
check "--env: DISPLAY=:0, X11 only, no Wayland" 'e "DISPLAY=:0" && e "XDG_SESSION_TYPE=x11" && ! grep -q "^WAYLAND_DISPLAY=" "$R/out"'
check "--env: the provider on the data volume, x86_64 only" 'e "PRESSURE_VESSEL_GRAPHICS_PROVIDER=$R/data/home/1000/.local/share/bitsos-steam/gfx" && e "PRESSURE_VESSEL_ARCHITECTURES=x86_64-linux-gnu"'
check "--env: Proton's 64-bit-only Wine for every game" 'e "PROTON_USE_WOW64=1"'
check "--env: the tree's realpath first on PATH" 'e "PATH=$T/bin:/bin:/sbin:/usr/bin"'
check "--env: no sound, the session bus, no system bus" '! e "LIBGL_ALWAYS_SOFTWARE=1" && e "SDL_AUDIODRIVER=dummy" && e "ALSA_CONFIG_PATH=$R/data/home/1000/.local/share/bitsos-steam/asound.conf" && e "DBUS_SESSION_BUS_ADDRESS=unix:path=$R/xdg/bus" && e "DBUS_SYSTEM_BUS_ADDRESS=disabled:"'
check "--env: nothing else from outside, no credential, no library path" '! grep -qiE "^(LD_LIBRARY_PATH|.*(PASSWORD|TOKEN|SECRET|CREDENTIAL).*)=" "$R/out" && [ "$(wc -l <"$R/out")" -le 20 ]'
check "--env: not verbose unless asked" '! grep -q VERBOSE "$R/out"'
run SB_VERBOSE=1
check "--env: verbose when asked" 'e "PRESSURE_VESSEL_VERBOSE=1"'

ARGS=--check; run
check "--check green with everything there" '[ $rc = 0 ] && grep -q "tree $T: ok, publicbeta 123" "$R/out" && grep -q "bash: $R/bash" "$R/out" && grep -q "display=:0" "$R/out" && grep -q "on ext4" "$R/out" && grep -q "installed: not yet" "$R/out" && grep -q "reaper: none yet at .*steamrt64/reaper: no game can start" "$R/out"'
check "--check writes nothing" '[ ! -e "$R/data/home/1000/.local" ] && [ ! -e "$R/user/steam" ]'
check "--check: no Proton yet, said, stays green" '[ $rc = 0 ] && grep -q "proton: none in .*/steamapps/common yet" "$R/out"'
run SB_BASH=/nonexistent/bash
check "--check names a missing /bin/bash and the manifest line" '[ $rc = 1 ] && grep -q "initrd bash host:/usr/bin/bash" "$R/out"'
mv "$T/gfx.tar.gz" "$T/gfx.moved"; run; mv "$T/gfx.moved" "$T/gfx.tar.gz"
check "--check names a missing file of the tree" '[ $rc = 1 ] && grep -q "gfx.tar.gz is missing" "$R/out"'
mv "$T/package/steam_steamrt_ubuntu12.zip.abc" "$R/moved.zip"; run; mv "$R/moved.zip" "$T/package/steam_steamrt_ubuntu12.zip.abc"
check "--check names a missing 64-bit client" '[ $rc = 1 ] && grep -q "the 64-bit client" "$R/out"'
run FAKE_XW="xwayland off"
check "--check: no display on the desktop" '[ $rc = 1 ] && grep -q "holds no display :0" "$R/out"'
run SB_X11_DIR=/nonexistent
check "--check: no X socket" '[ $rc = 1 ] && grep -q "X0 is not there" "$R/out"'
MOUNTS=$M_VFAT run
check "--check: /data on FAT is refused with the reason" '[ $rc = 1 ] && grep -q "is vfat" "$R/out"'
MOUNTS=$M_NONE run
check "--check: no data volume is refused with the reason" '[ $rc = 1 ] && grep -q "no data volume" "$R/out"'
MOUNTS=$M_RO run
check "--check: a read-only volume is refused" '[ $rc = 1 ] && grep -q "no data volume" "$R/out"'
run FAKE_FREE=100
check "--check: too little room is refused with the numbers" '[ $rc = 1 ] && grep -q "has 100 KiB free" "$R/out"'
run FAKE_UID=0
check "root hands over to uid 1000" '[ $rc = 0 ] && grep -q "runs as uid 1000" "$R/out"'
run FAKE_UID=0 FAKE_NO_DROP=1
check "root that cannot hand over refuses" '[ $rc = 1 ] && grep -q "does not run as root" "$R/out"'
run
check "--check names the groups" 'grep -q "runs as uid 1000, groups 1000" "$R/out"'
run FAKE_GROUPS="1000 0"
check "--check: root's group left over is refused" '[ $rc = 1 ] && grep -q "still has group 0 (1000 0)" "$R/out"'

RT=$R/data/home/1000/.local/share/Steam
BITS=$R/data/home/1000/.local/share/bitsos-steam
ST=$R/user/steam/stages
ARGS="-cef-disable-sandbox"; run
check "first start: Steam's code comes back" '[ $rc = 0 ]'
check "first start: the stages, in order" '[ "$(sed -E "s/^[0-9]+ stage=([a-z-]+).*/\1/" "$ST" | tr "\n" " ")" = "start xwayland data install provider bus steam ended " ]'
check "first start: the bootstrap and ~/.steam links" '[ -f "$RT/steam.sh" ] && [ "$(readlink "$R/data/home/1000/.steam/steam")" = "$RT" ] && [ "$(readlink "$R/data/home/1000/.steam/root")" = "$RT" ]'
check "first start: package/ seeded, beta and 64-bit opt-in" '[ -f "$RT/package/steam_steamrt_ubuntu12.zip.abc" ] && [ -f "$RT/package/steam_client_publicbeta_ubuntu12.manifest" ] && [ "$(cat "$RT/package/beta")" = publicbeta ] && [ -e "$RT/.steam-enable-steamrt64-client" ]'
check "first start: steamrt64/steam out of its zip, executable" '[ -x "$RT/steamrt64/steam" ] && grep -q "fake elf" "$RT/steamrt64/steam"'
check "first start: no copy left half-done" '[ -z "$(find "$RT/package" -name "*.part")" ]'
check "first start: the provider unpacked, the null sound device" '[ -f "$BITS/gfx/usr/lib/libc.so.6" ] && [ "$(cat "$BITS/gfx/.bitsos-version")" = sum1 ] && grep -q "type null" "$BITS/asound.conf"'
check "steam.sh got the arguments, no software flags" 'grep -q "fake steam.sh args: -cef-disable-sandbox$" "$R/user/steam/steam.log"'
check "steam.sh ran in Steam's root with the clean environment" 'grep -q "^HOME=$R/data/home/1000$" "$RT/env-steam-saw" && grep -q "^DISPLAY=:0$" "$RT/env-steam-saw" && ! grep -q "^WAYLAND_DISPLAY" "$RT/env-steam-saw"'
check "the log holds stages and Steam's output" 'grep -q "stage=steam" "$R/user/steam/steam.log" && grep -q "stage=ended code 0" "$R/user/steam/steam.log"'
echo steam-updated-it >"$RT/steamrt64/steam"
touch "$BITS/gfx/stays"
echo 7 >"$RT/fake-rc"
run
rm -f "$RT/fake-rc"
check "second start: Steam's exit code comes back" '[ $rc = 7 ] && grep -q "stage=ended code 7" "$ST"'
check "second start: installs nothing again" 'grep -q "stage=install already, publicbeta 123" "$ST" && grep -q "stage=provider already" "$ST" && [ -e "$BITS/gfx/stays" ] && grep -q steam-updated-it "$RT/steamrt64/steam"'
sed -i 's/client publicbeta 123 abc/client publicbeta 124 abd/; s/ sum1/ sum2/' "$T/VERSION.txt"
run
check "a new tree seeds again, keeps what Steam updated, replaces the provider" '[ $rc = 0 ] && grep -q "stage=install seeding" "$ST" && grep -q steam-updated-it "$RT/steamrt64/steam" && [ ! -e "$BITS/gfx/stays" ] && [ "$(cat "$BITS/gfx/.bitsos-version")" = sum2 ]'
# GM-2b: the stand-in over steamrt64/reaper, once Steam's file check is over (the web helper runs).
echo "$R/proc" >"$RT/fake-proc"
RP_=$RT/steamrt64/reaper
reaper_stage() { grep "stage=reaper" "$ST" | tail -n 1 | grep -q "stage=reaper $1"; }
no_part() { [ -z "$(find "$RT/steamrt64" -name "*.part")" ]; }
ARGS=
rm -f "$RP_"; run
check "reaper: none there, the stand-in is put, says so" '[ $rc = 0 ] && cmp -s "$R/standin" "$RP_" && reaper_stage "stand-in, over: none yet" && no_part'
check "reaper: a real executable file, not a link" '[ -f "$RP_" ] && [ ! -L "$RP_" ] && [ -x "$RP_" ] && [ -w "$RP_" ]'
run
check "reaper: already there, no new stage" '[ $rc = 0 ] && cmp -s "$R/standin" "$RP_" && ! grep -q "stage=reaper" "$ST"'
check "reaper: nothing before Steam's file check" '[ "$(sed -E "s/^[0-9]+ stage=([a-z-]+).*/\1/" "$ST" | grep -c "^reaper$")" = 0 ]'
ARGS=--check; run
check "reaper: --check says stand-in" '[ $rc = 0 ] && grep -q "reaper: stand-in at $RP_" "$R/out"'
cp "$R/original" "$RP_"
run
check "reaper: --check says original, changes nothing, stays green" '[ $rc = 0 ] && grep -q "reaper: original (i386) at $RP_: no game can start through it until the next steam start" "$R/out" && cmp -s "$R/original" "$RP_"'
ARGS=; run
check "reaper: Steam restored the original, the next start replaces it and says so" '[ $rc = 0 ] && cmp -s "$R/standin" "$RP_" && [ -x "$RP_" ] && reaper_stage "stand-in, over: original (i386)" && no_part'
cp "$R/older" "$RP_"
ARGS=--check; run
check "reaper: --check tells another file from the original" '[ $rc = 0 ] && grep -q "reaper: another file at $RP_" "$R/out"'
ARGS=; run
check "reaper: an older stand-in is replaced too" '[ $rc = 0 ] && cmp -s "$R/standin" "$RP_" && reaper_stage "stand-in, over: another file"'
mv "$RT/steamrt64" "$RT/steamrt64.moved"
rm -f "$RT/steamrt64.moved/reaper"
run
check "reaper: makes the folder when Steam's tree has none" '[ $rc = 0 ] && cmp -s "$R/standin" "$RP_"'
rm -rf "$RT/steamrt64"; mv "$RT/steamrt64.moved" "$RT/steamrt64"
cp "$R/original" "$RP_"
mv "$R/standin" "$R/standin.moved"
ARGS=--check; run
check "reaper: --check is red without the stand-in on the image" '[ $rc = 1 ] && grep -q "the stand-in $R/standin is missing" "$R/out"'
ARGS=; run
check "reaper: Steam runs on without the stand-in, its stage says why" 'grep -q "stage=reaper-failed the stand-in $R/standin is missing" "$ST" && cmp -s "$R/original" "$RP_"'
mv "$R/standin.moved" "$R/standin"
mv "$R/bin/cp" "$R/cp.real"
printf '#!%s sh\necho "cp: No space left on device" >&2\nexit 1\n' "$BB" >"$R/bin/cp"; chmod +x "$R/bin/cp"
run
rm "$R/bin/cp"; mv "$R/cp.real" "$R/bin/cp"
check "reaper: a copy that fails says so at its stage, leaves what was there, no half file" 'grep -q "stage=reaper-failed cannot put the stand-in .*there is still: original (i386)" "$ST" && cmp -s "$R/original" "$RP_" && no_part'
run
check "reaper: the start after that is whole" '[ $rc = 0 ] && cmp -s "$R/standin" "$RP_" && reaper_stage "stand-in, over: original (i386)"'
check "Steam itself gets PROTON_USE_WOW64=1" 'grep -q "^PROTON_USE_WOW64=1$" "$RT/env-steam-saw"'
PC=$RT/steamapps/common
mkdir -p "$PC/Proton 10.0/files/bin-wow64" "$PC/Proton 9.0 (Beta)/files/bin" "$PC/Proton - Experimental/files/bin-wow64" "$PC/Proton Hotfix"
for p_ in "Proton 10.0" "Proton 9.0 (Beta)" "Proton - Experimental"; do echo fake >"$PC/$p_/proton"; done
echo fake >"$PC/Proton 10.0/files/bin-wow64/wine"; echo fake >"$PC/Proton - Experimental/files/bin-wow64/wine"; echo fake >"$PC/Proton 9.0 (Beta)/files/bin/wine"
ARGS=--check; run
check "--check: each Proton with its WoW64 Wine, green" '[ $rc = 0 ] && grep -q "proton: Proton 10.0: files/bin-wow64/wine there (WoW64, runs on V2)" "$R/out" && grep -q "proton: Proton - Experimental: files/bin-wow64/wine there" "$R/out"'
check "--check: a Proton without it is named, still green" '[ $rc = 0 ] && grep -q "proton: Proton 9.0 (Beta): no files/bin-wow64/wine: its classic Wine is 32-bit" "$R/out"'
check "--check: a folder without the proton script is no Proton" '! grep -q "Proton Hotfix" "$R/out" && ! grep -q "proton: none" "$R/out"'
rm -rf "$PC"
ARGS=

rm -f "$RT/fake-proc"
MOUNTS=$M_VFAT run
check "a refused start says the stage" '[ $rc = 1 ] && tail -n 1 "$ST" | grep -q "stage=data-failed data: .* is vfat"'
run FAKE_XW="xwayland off"
check "a start without X says the stage" '[ $rc = 1 ] && tail -n 1 "$ST" | grep -q "stage=xwayland-failed"'

mkdir -p "$R/proc/4242" "$R/proc/4343"
ln -s "$RT/steamrt64/steam" "$R/proc/4242/exe"
echo steamwebhelper >"$R/proc/4343/comm"
ARGS=--status; run
check "--status: last stage, programs by exe and by comm, windows, logs" 'grep -q "last stage: .*stage=xwayland-failed" "$R/out" && grep -q "process steam: 4242" "$R/out" && grep -q "process steamwebhelper: 4343" "$R/out" && grep -q "window: toplevel app_id=steam x11=0x1" "$R/out" && grep -q "log: $R/user/steam/steam.log" "$R/out"'

# One start at a time, and the end after a signal. A launcher left alone runs on: every path below ends it, this is the safety net.
rm -rf "$R/proc/4242" "$R/proc/4343"
BG= HELPER=
cleanup() { for p in $BG $HELPER $(cat "$RT/up" 2>/dev/null); do kill -KILL "$p" 2>/dev/null; done; }
trap cleanup EXIT
upto() { n=0; while [ $n -lt 100 ]; do eval "$1" && return 0; sleep 0.1; n=$((n + 1)); done; return 1; }
start_held() { # a launcher in the background whose Steam stays up; sets BG
  rm -f "$RT/up" "$RT/term-done"; touch "$RT/fake-hold"
  ARGS= launch >"$R/bg.out" 2>&1 &
  BG=$!
  upto '[ -e "$RT/up" ]' || echo "FAIL the held Steam does not come up: $(cat "$R/bg.out")"
  mkdir -p "$R/proc/$BG"; printf 'sh\0%s\0' "$L" >"$R/proc/$BG/cmdline"
}
# A program of Steam's (steamwebhelper unless named) that the fake /proc shows and that TERM really ends; sets HELPER.
start_helper() {
  "$BB" sh -c 'mkdir -p "$1/proc/$$"; echo "$2" >"$1/proc/$$/comm"; trap "rm -rf \"\$1/proc/\$\$\"; exit 0" TERM; n=0; while [ $n -lt 300 ]; do sleep 0.1; n=$((n + 1)); done' x "$R" "${1:-steamwebhelper}" &
  HELPER=$!
  upto '[ -e "$R/proc/$HELPER/comm" ]' || echo "FAIL the fake helper does not come up"
}
start_held
check "the lock names the launcher" '[ "$(cat "$R/user/steam/lock")" = "$BG" ]'
cp "$ST" "$R/stages.before"; cp "$R/user/steam/steam.log" "$R/log.before"; ls -lR "$RT" >"$R/tree.before"
ARGS=; run
check "a second start is refused and says who runs" '[ $rc = 1 ] && grep -q "already running (launcher $BG, .*stage=steam" "$R/out"'
check "a second start changes nothing: stages, log, tree" 'cmp -s "$ST" "$R/stages.before" && cmp -s "$R/user/steam/steam.log" "$R/log.before" && { ls -lR "$RT" | cmp -s - "$R/tree.before"; } && [ "$(cat "$R/user/steam/lock")" = "$BG" ]'
ARGS=--check; run
check "--check says a launcher runs, and stays green" '[ $rc = 0 ] && grep -q "running: launcher $BG, .*stage=steam" "$R/out"'
ARGS=--status; run
check "--status names the launcher" 'grep -q "launcher: $BG" "$R/out"'
start_helper
kill -TERM $BG
wait $BG
bgrc=$?
check "TERM: the launcher ends only after Steam has, with Steam's own code" '[ $bgrc = 9 ] && [ -e "$RT/term-done" ] && tail -n 1 "$ST" | grep -q "stage=ended code 9"'
check "TERM: what is left of Steam goes too" '! kill -0 $HELPER 2>/dev/null && [ ! -e "$R/proc/$HELPER" ]'
ARGS=--status; run
check "--status after the end: no launcher" 'grep -q "launcher: none" "$R/out"'
start_helper
ARGS=; run
check "a start with Steam's programs left and no launcher is refused" '[ $rc = 1 ] && grep -q "still running without a launcher: $HELPER" "$R/out" && tail -n 1 "$ST" | grep -q "stage=ended code 9"'
ARGS=--check; run
check "--check names them too" '[ $rc = 1 ] && grep -q "still running without a launcher: $HELPER" "$R/out"'
kill -TERM $HELPER; wait $HELPER 2>/dev/null
start_held
start_helper reaper
ARGS=--status; run
check "--status names a running reaper" 'grep -q "process reaper: $HELPER" "$R/out"'
kill -TERM $BG
wait $BG 2>/dev/null
check "TERM: a game's reaper gets it too (it hands it to the game)" '! kill -0 $HELPER 2>/dev/null && [ ! -e "$R/proc/$HELPER" ]'
start_held
kill -KILL $BG
wait $BG 2>/dev/null
orphan=$(cat "$RT/up")
rm -f "$RT/fake-hold"
ARGS=; run
check "a killed launcher leaves no lock, though its Steam lives on" '[ $rc = 0 ] && kill -0 "$orphan" 2>/dev/null && tail -n 1 "$ST" | grep -q "stage=ended code 0"'
kill -KILL "$orphan" 2>/dev/null
start_held
ARGS=--stop; run
wait $BG 2>/dev/null
check "--stop before Steam answers ends the launcher, says stopped only then" '[ $rc = 0 ] && grep -q "steam: stopped" "$R/out" && ! kill -0 $BG 2>/dev/null'
rm -f "$RT/fake-hold"
mv "$R/bin/flock" "$R/flock.real"
printf '#!%s sh\necho "flock: Function not implemented" >&2\nexit 1\n' "$BB" >"$R/bin/flock"; chmod +x "$R/bin/flock"
ARGS=; run
rm "$R/bin/flock"; mv "$R/flock.real" "$R/bin/flock"
check "a lock that cannot be taken says why, starts nothing" '[ $rc = 1 ] && grep -q "cannot take .*lock: flock: Function not implemented" "$R/out"'
run FAKE_GROUPS="0 1000"
check "a start with group 0 says the stage" '[ $rc = 1 ] && tail -n 1 "$ST" | grep -q "stage=user-failed"'
trap - EXIT
cleanup

# files/realpath: lexical with --no-symlinks, as GNU's.
ln -s /nonexistent "$R/link"
RP() { (cd "$R" && env PATH="$R/bin" "$BB" sh "$wt/user/ports/steam/files/realpath" "$@"); }
check "realpath --no-symlinks: dots and .. go, the link stays" '[ "$(RP --no-symlinks /a/./b/../c//d/)" = /a/c/d ] && [ "$(RP --no-symlinks "$R/link")" = "$R/link" ]'
check "realpath --no-symlinks: a relative name from the cwd; the root" '[ "$(RP --no-symlinks link)" = "$R/link" ] && [ "$(RP -s /..)" = / ]'
check "realpath: two names" '[ "$(RP --no-symlinks /a /b | tr "\n" " ")" = "/a /b " ]'

python3 -I "$here/steam_stand_in.py" "$RUN_DIR/check" >"$R/out" 2>&1
rc=$?
check "steam_check.py against the stand-in" '[ $rc = 0 ]'
cat "$R/out" | grep -E '^(ok|RED)' || true

[ $fails = 0 ] || { echo "RED: $fails"; exit 1; }
echo "GREEN: the steam launcher"
