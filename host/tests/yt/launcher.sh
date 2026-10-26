#!/usr/bin/env bash
# YT-2: the `cobalt` launcher (user/ports/cobalt-launcher/bitsos/cobalt) under BusyBox's sh on the build host,
# with a fake tree, a fake loader_app (a script that answers LD_TRACE_LOADED_OBJECTS and plays a Cobalt that
# writes its profile), a fake desktop (a script named bitscomp that takes `ctl ... quit`), a fake mount table and a
# fake pidof. Checks --check, the user switch (the real BusyBox chpst), the desktop handover, profile restore and
# save (one tar file, renames, .old fallback, unclean ends, a failing rename), the read-only case, Cobalt's exit
# code, the environment and SIGTERM. No QEMU, no screen, no sound device, nothing outside the run directory.
set -uo pipefail
here=$(cd "$(dirname "$0")" && pwd)
wt=${BITSOS_ROOT:-$(cd "$here/../../.." && pwd)}
R=${BITSOS_RUN_DIR:-$wt/target/yt/launcher}/fake
L=$wt/user/ports/cobalt-launcher/bitsos/cobalt
BB=$wt/user/ports/busybox/out/busybox
if [ ! -x "$BB" ]; then
  V2_OFFLINE=1 sh "$wt/user/ports/busybox/build.sh" >/dev/null 2>&1 || true
fi
[ -x "$BB" ] || { echo "SKIP: no BusyBox build ($BB)"; exit 0; }
rm -rf "$R"; mkdir -p "$R/bin" "$R/rootbin" "$R/run" "$R/tmp" "$R/user"
fails=0
ok() { echo "ok   $1"; }
bad() { echo "FAIL $1"; fails=$((fails + 1)); }
check() { if eval "$2"; then ok "$1"; else bad "$1"; echo "     output: $(tr '\n' '|' <"$R/out")"; fi; }

# BusyBox's applets, then the fakes over them. A fake replaces a link to BusyBox only after `rm -f`: writing
# through the link would overwrite BusyBox itself.
"$BB" --install -s "$R/bin" 2>/dev/null || { for a in $("$BB" --list); do ln -sf "$BB" "$R/bin/$a"; done; }
rm -f "$R/bin/pidof" "$R/bin/mv"
cat >"$R/bin/pidof" <<EOF
#!$BB sh
[ "\$1" = bitscomp ] || exit 0
if [ -n "\$FAKE_BITSCOMP" ]; then echo "\$FAKE_BITSCOMP"; exit 0; fi
p=\$(cat "$R/desk.pid" 2>/dev/null)
[ -n "\$p" ] && kill -0 "\$p" 2>/dev/null && ! grep -q '^State:[[:space:]]*Z' "/proc/\$p/status" && echo "\$p"
exit 0
EOF
# mv that fails for one source path (FAKE_MV_FAIL), else BusyBox's.
cat >"$R/bin/mv" <<EOF
#!$BB sh
[ -z "\$FAKE_MV_FAIL" ] || [ "\$1" != "\$FAKE_MV_FAIL" ] || { echo "mv: fake failure" >&2; exit 1; }
exec "$BB" mv "\$@"
EOF
# The desktop: `ctl --socket S quit` ends the running one; else it runs until SIGTERM.
cat >"$R/bin/bitscomp" <<EOF
#!$BB sh
if [ "\$1" = ctl ]; then
  echo "\$* xdg=\$XDG_RUNTIME_DIR" >>"$R/ctl.txt"
  [ "\$4" != quit ] || kill -TERM "\$(cat "$R/desk.pid")"
  exit 0
fi
echo \$\$ >"$R/desk.pid"; echo "\$*" >>"$R/desk.starts"; env | grep '^DESK_MARK=' >>"$R/desk.marks"
trap 'kill \$s 2>/dev/null; exit 0' TERM
sleep 60 & s=\$!
wait
EOF
# As root: `id -u` says 0, or 1000 under chpst.ok (chpst as BusyBox answers on V2 since AB-13: the ids change).
printf '#!%s sh\necho "${FAKE_UID:-0}"\n' "$BB" >"$R/rootbin/id"
printf '#!%s sh\n[ "$1" = -u ] && shift 2 && FAKE_UID=1000 exec "$@"\nexit 111\n' "$BB" >"$R/rootbin/chpst.ok"
chmod +x "$R/bin/pidof" "$R/bin/mv" "$R/bin/bitscomp" "$R/rootbin/id" "$R/rootbin/chpst.ok"

T=$R/mnt/BITSOS2/cobalt
mkdir -p "$T/app/cobalt/lib" "$T/app/cobalt/content/ssl/certs" "$T/lib"
: >"$T/app/cobalt/lib/libcobalt.so"; : >"$T/app/cobalt/content/cobalt_shell.pak"
cat >"$T/loader_app" <<EOF
#!$BB sh
if [ "\$LD_TRACE_LOADED_OBJECTS" = 1 ]; then
  printf '\tlibX11.so.6 => $T/lib/libX11.so.6 (0x1)\n\tlibc.so.6 => /usr/lib/libc.so.6 (0x2)\n'
  [ -z "\$FAKE_MISSING" ] || printf '\tlibasound.so.2 => not found\n'
  exit 0
fi
env | sort >"\$HOME/../env.txt"; echo "\$*" >"\$HOME/../args.txt"
[ -z "\$BITSOS_AV_STATS" ] || printf 'bitsos-av 1\nframes_shown=7\n' >"\$BITSOS_AV_STATS"
d=\$HOME/.cobalt_files/aHR0cHM6Ly93d3cueW91dHViZS5jb20vdHY=
mkdir -p "\$d"; n=\$(cat "\$d/Cookies" 2>/dev/null || echo 0); echo \$((n + 1)) >"\$d/Cookies"
trap 'echo term >"\$HOME/../term.txt"; exit \${FAKE_TERM_RC:-143}' TERM
[ -z "\$FAKE_CRASH" ] || kill -SEGV \$\$
[ -z "\$FAKE_SLEEP" ] || { sleep "\$FAKE_SLEEP" & wait; }
exit \${FAKE_RC:-0}
EOF
chmod +x "$T/loader_app"
echo "/dev/sda1 $R/mnt vfat rw,fmask=0022,dmask=0022 0 0" >"$R/mounts.rw"
echo "/dev/sda1 $R/mnt vfat ro,fmask=0022,dmask=0022 0 0" >"$R/mounts.ro"
P=$R/mnt/BITSOS2/home
PROF=$P/cobalt_files.tar
RUN=$R/run/cobalt
launch() { launch_bg "$@"; wait $lp; }
launch_bg() { env -i COBALT_MOUNT="$R/mnt" COBALT_MOUNTS="$R/mounts.${MNT:-rw}" COBALT_PATH="${XPATH:-$R/bin}" COBALT_ALSA_SYS=/nonexistent \
  COBALT_TMP="$R/tmp" COBALT_USER_RUN="$R/user" XDG_RUNTIME_DIR="$R/run" FAKE_BITSCOMP="${FAKE_BITSCOMP:-}" \
  FAKE_MISSING="${FAKE_MISSING:-}" FAKE_RC="${FAKE_RC:-}" FAKE_SLEEP="${FAKE_SLEEP:-}" FAKE_CRASH="${FAKE_CRASH:-}" \
  FAKE_MV_FAIL="${FAKE_MV_FAIL:-}" FAKE_TERM_RC="${FAKE_TERM_RC:-}" ${COBALT_AV_TRACE:+COBALT_AV_TRACE=1} \
  ${COBALT_ALLOW_ROOT:+COBALT_ALLOW_ROOT=$COBALT_ALLOW_ROOT} PATH="$R/bin" "$BB" sh "$L" "$@" >"$R/out" 2>&1 & lp=$!; }
cookie() { "$BB" tar -xOf "${1:-$PROF}" .cobalt_files/aHR0cHM6Ly93d3cueW91dHViZS5jb20vdHY=/Cookies 2>/dev/null; }
ram_cookie() { cat "$RUN/home/.cobalt_files/aHR0cHM6Ly93d3cueW91dHViZS5jb20vdHY=/Cookies" 2>/dev/null; }

launch --check; rc=$?
check "--check passes on a whole tree" '[ $rc = 0 ] && grep -qx "cobalt: tree $T: ok" "$R/out"'
check "--check counts the libraries" 'grep -qx "cobalt: libraries: 2 found, none missing" "$R/out"'
check "--check: screen free, user, own ALSA" 'grep -qx "cobalt: screen: free" "$R/out" && grep -q "^cobalt: user: runs as uid " "$R/out" && grep -qx "cobalt: sound: own ALSA configuration (card 0 through plug)" "$R/out"'
check "--check: no home yet means RAM only" 'grep -q "^cobalt: profile: in RAM only .*is not writable for uid" "$R/out"'
FAKE_MISSING=1 launch --check; rc=$?
check "--check fails on a missing library" '[ $rc = 1 ] && grep -q "^cobalt: libraries missing: libasound.so.2 => not found" "$R/out"'
mv "$T/app/cobalt/lib/libcobalt.so" "$R/libcobalt.so"; launch --check; rc=$?
check "--check fails on a missing file" '[ $rc = 1 ] && grep -q "^cobalt: tree $T: app/cobalt/lib/libcobalt.so is missing" "$R/out"'
mv "$R/libcobalt.so" "$T/app/cobalt/lib/libcobalt.so"

# The user switch: the real BusyBox chpst takes `-u 1000:1000` (setpriv has no --reuid in BusyBox 1.36.1).
out=$("$BB" chpst -u 1000:1000 true 2>&1); crc=$?
check "BusyBox chpst -u uid:gid reaches the id change" '[ $crc = 0 ] || [ "$out" = "chpst: setgroups: Operation not permitted" ]'
# YT-6: Cobalt never runs as root. Without an id change (the host's chpst cannot setgroups) root is refused.
XPATH="$R/rootbin:$R/bin" launch --check; rc=$?
check "as root without id change: --check says refused" '[ $rc = 1 ] && grep -q "^cobalt: user: runs as uid 0: refused, root cannot hand over to uid 1000 (chpst -u 1000:1000: " "$R/out" && grep -q "^cobalt: profile: $PROF (packed back" "$R/out"'
XPATH="$R/rootbin:$R/bin" launch; rc=$?
check "as root without id change: refused in German and English, nothing starts" '[ $rc = 1 ] && grep -q "^cobalt: Cobalt startet nicht als root: root cannot hand over" "$R/out" && grep -q "^cobalt: Cobalt does not run as root: " "$R/out" && ! grep -q "^cobalt: starting" "$R/out" && [ ! -e "$R/tmp/cobalt-0" ]'
COBALT_ALLOW_ROOT=1 XPATH="$R/rootbin:$R/bin" launch --check; rc=$?
check "COBALT_ALLOW_ROOT=1: root allowed for diagnosis, said so" '[ $rc = 0 ] && grep -q "^cobalt: user: runs as uid 0 (COBALT_ALLOW_ROOT=1, not supported: " "$R/out"'
mv "$R/rootbin/chpst.ok" "$R/rootbin/chpst"
XPATH="$R/rootbin:$R/bin" launch --check; rc=$?
check "as root with id change: --check says uid 1000" '[ $rc = 0 ] && grep -qx "cobalt: user: runs as uid 1000 (started by root: chpst -u 1000:1000)" "$R/out"'
rm -rf "$R/user"; mkdir -p "$P"
XPATH="$R/rootbin:$R/bin" launch; rc=$?
U=$R/user/cobalt
check "as root: Cobalt starts as uid 1000 and says so" '[ $rc = 0 ] && grep -qx "cobalt: started by root: Cobalt runs as uid 1000 (chpst -u 1000:1000)" "$R/out" && grep -qx "FAKE_UID=1000" "$U/env.txt"'
check "as root: user 1000's runtime directory (made 700 when missing), HOME and XDG_RUNTIME_DIR there" '[ "$(stat -c %a "$R/user")" = 700 ] && grep -qx "HOME=$U/home" "$U/env.txt" && grep -qx "XDG_RUNTIME_DIR=$R/user" "$U/env.txt" && grep -q "^cobalt: profile saved on the stick" "$R/out"'
XPATH="$R/rootbin:$R/bin" launch --stats; rc=$?
check "as root: --stats reads user 1000's numbers" '[ $rc = 0 ] && grep -q "^cobalt: $U/av-stats.txt" "$R/out"'
# Root inside a desktop of user 1000: handed over as before, but the desktop's start line (what root runs again)
# and its log stay in a fresh root-only directory, never in user 1000's run directory.
(cd "$R" && exec env -i PATH="$R/bin" DESK_MARK=1 XDG_RUNTIME_DIR="$R/user" "$R/bin/bitscomp" --socket wayland-r >/dev/null 2>&1) &
for _ in $(seq 1 50); do [ -s "$R/desk.pid" ] && break; sleep 0.1; done
d1=$(cat "$R/desk.pid"); rm -f "$U/launcher.log"
XPATH="$R/rootbin:$R/bin" launch; rc=$?
for _ in $(seq 1 100); do grep -q "desktop started again" "$U/launcher.log" 2>/dev/null && break; sleep 0.1; done
d2=$(cat "$R/desk.pid")
check "as root in a desktop: handed over, Cobalt as uid 1000, the desktop again" '[ $rc = 0 ] && [ "$d2" != "$d1" ] && kill -0 $d2 2>/dev/null && grep -q "^cobalt: profile saved on the stick" "$U/launcher.log" && grep -qx "FAKE_UID=1000" "$U/env.txt"'
dd=$(ls -d "$R"/tmp/cobalt-desk.* 2>/dev/null)
check "as root: the desktop's start line not in user 1000's directory, a 700 directory of its own" '! ls -a "$U" | grep -q "^desk\." && [ -n "$dd" ] && [ "$(stat -c %a $dd)" = 700 ] && [ -e "$dd/bitscomp.log" ] && [ ! -e "$dd/desk.cmd" ] && [ ! -e "$dd/desk.env" ]'
kill -TERM "$d2" 2>/dev/null
for _ in $(seq 1 50); do kill -0 "$d2" 2>/dev/null || break; sleep 0.1; done
rm -rf "$R"/tmp/cobalt-desk.* "$R/desk.pid" "$R/desk.starts" "$R/desk.marks" "$R/ctl.txt"
# Root writes nothing into user 1000's directory itself (YT-6 fixer): no chown there, no redirection into it.
check "root never chowns or redirects into the run directory" '! grep -nE "chown -[a-zA-Z]*R|>>?\"?\\\$(RUN|H|LOG|AV|USER_RUN)\b" "$L" >"$R/out"'
printf '#!%s sh\necho ""\n' "$BB" >"$R/rootbin/id.bad"; mv "$R/rootbin/id" "$R/rootbin/id.ok"; mv "$R/rootbin/id.bad" "$R/rootbin/id"; chmod +x "$R/rootbin/id"
XPATH="$R/rootbin:$R/bin" launch; rc=$?
check "no uid from id: nothing starts (root must not pass for another user)" '[ $rc = 1 ] && grep -q "^cobalt: id -u gave no uid" "$R/out"'
mv "$R/rootbin/id.ok" "$R/rootbin/id"
rm -f "$R/rootbin/chpst" "$PROF" "$PROF.old"; rm -rf "$R/user"; mkdir -p "$R/user"; rmdir "$P"

FAKE_BITSCOMP="42 43" launch; rc=$?
check "two desktops: nothing starts" '[ $rc = 1 ] && grep -q "cannot be handed over: 2 desktops run" "$R/out" && [ ! -e "$RUN/args.txt" ]'

mkdir -p "$P"
launch; rc=$?
check "first start: new profile, saved on the stick" '[ $rc = 0 ] && grep -qx "cobalt: profile: new (sign in once)" "$R/out" && grep -q "^cobalt: profile saved on the stick" "$R/out" && [ "$(cookie)" = 1 ]'
check "no .new or .old left behind" '[ ! -e "$PROF.new" ] && [ ! -e "$PROF.old" ] && [ ! -e "$RUN/home/.unsaved" ]'
check "environment and arguments, no LD_LIBRARY_PATH" 'grep -qx "HOME=$RUN/home" "$RUN/env.txt" && grep -qx "BITSOS_BACKEND=fb" "$RUN/env.txt" && grep -qx "BITSOS_FB=/dev/fb0" "$RUN/env.txt" && ! grep -q "^LD_LIBRARY_PATH=" "$RUN/env.txt" && grep -qx "ALSA_CONFIG_PATH=$RUN/asound.conf" "$RUN/env.txt" && grep -qx -- "--evergreen_lite --disable-dev-shm-usage --url=https://www.youtube.com/tv" "$RUN/args.txt"'
check "own ALSA file names card 0" 'grep -q "type hw card 0 device 0" "$RUN/asound.conf"'
launch https://example.org/ -- --foo; rc=$?
check "a URL and loader_app arguments" '[ $rc = 0 ] && grep -qx -- "--evergreen_lite --disable-dev-shm-usage --url=https://example.org/ --foo" "$RUN/args.txt"'
check "second start takes the profile from the stick, keeps the one before" 'grep -qx "cobalt: profile: taken from the stick (cobalt_files.tar)" "$R/out" && [ "$(cookie)" = 2 ] && [ "$(cookie "$PROF.old")" = 1 ]'

echo garbage >"$PROF"
launch; rc=$?
check "an unreadable copy: the one before is taken" '[ $rc = 0 ] && grep -q "^cobalt: profile: $PROF cannot be read" "$R/out" && grep -qx "cobalt: profile: taken from the stick (cobalt_files.tar.old)" "$R/out" && [ "$(cookie)" = 2 ]'
echo garbage >"$PROF"; echo garbage >"$PROF.old"; rm -rf "$RUN/home"
launch; rc=$?
check "both unreadable: a new profile, Cobalt starts" '[ $rc = 0 ] && grep -qx "cobalt: profile: new (sign in once)" "$R/out" && [ "$(cookie)" = 1 ]'
rm -f "$PROF.old"

FAKE_CRASH=1 launch; rc=$?
check "a crash: profile not saved, the stick keeps its copy" '[ $rc = 139 ] && grep -q "^cobalt: profile not saved: Cobalt ended by signal 11" "$R/out" && [ "$(cookie)" = 1 ] && [ "$(ram_cookie)" = 2 ] && [ -e "$RUN/home/.unsaved" ]'
launch; rc=$?
check "after a crash the newer RAM profile is kept and saved" 'grep -qx "cobalt: profile: the one from the last start in RAM (not on the stick)" "$R/out" && [ "$(cookie)" = 3 ] && [ ! -e "$RUN/home/.unsaved" ]'

FAKE_MV_FAIL="$PROF" launch; rc=$?
check "the first rename fails: not saved, the copy intact" '[ $rc = 0 ] && grep -q "^cobalt: profile not saved: $PROF cannot be moved aside" "$R/out" && [ "$(cookie)" = 3 ] && [ ! -e "$PROF.new" ] && [ -e "$RUN/home/.unsaved" ]'
FAKE_MV_FAIL="$PROF.new" launch; rc=$?
check "the second rename fails: the copy before is put back" 'grep -q "^cobalt: profile not saved: $PROF.new cannot be renamed" "$R/out" && [ "$(cookie)" = 3 ] && [ ! -e "$PROF.new" ]'
launch; rc=$?
check "the next clean end saves the RAM profile" 'grep -q "^cobalt: profile saved on the stick" "$R/out" && [ "$(cookie)" = 6 ]'

MNT=ro FAKE_RC=3 launch; rc=$?
check "read-only stick: Cobalt's code, profile not kept" '[ $rc = 3 ] && grep -q "^cobalt: Cobalt ended (code 3)" "$R/out" && grep -q "^cobalt: sign-in not kept on the stick: the stick is not mounted writable" "$R/out" && [ "$(cookie)" = 6 ]'
launch >/dev/null; rc=$?
check "and the next writable start saves it" '[ "$(cookie)" = 8 ]'

FAKE_SLEEP=30 FAKE_TERM_RC=0 launch_bg
for _ in $(seq 1 50); do [ -e "$RUN/args.txt" ] && [ "$(ram_cookie)" = 9 ] && break; sleep 0.1; done
rm -f "$RUN/term.txt"; sleep 0.3; kill -TERM $lp; wait $lp; rc=$?
check "SIGTERM: Cobalt is ended, its own code kept, the profile saved" '[ $rc = 0 ] && [ -e "$RUN/term.txt" ] && grep -q "^cobalt: Cobalt ended (code 0)" "$R/out" && grep -q "^cobalt: profile saved on the stick" "$R/out" && [ "$(cookie)" = 9 ]'

# The desktop handover: cobalt started while bitscomp runs.
rm -f "$RUN/args.txt"
(cd "$R" && exec env -i PATH="$R/bin" DESK_MARK=1 XDG_RUNTIME_DIR="$R/run" "$R/bin/bitscomp" --socket wayland-t >/dev/null 2>&1) &
for _ in $(seq 1 50); do [ -s "$R/desk.pid" ] && break; sleep 0.1; done
d1=$(cat "$R/desk.pid")
launch --check
check "--check: the desktop is handed over" 'grep -qx "cobalt: screen: the desktop has it (bitscomp, pid $d1); cobalt ends it for the run and starts it again afterwards" "$R/out"'
launch; rc=$?
check "inside the desktop: the launcher goes on detached" '[ $rc = 0 ] && grep -q "^cobalt: the desktop ends for Cobalt and starts again when Cobalt ends" "$R/out"'
for _ in $(seq 1 100); do grep -q "desktop started again" "$RUN/launcher.log" 2>/dev/null && break; sleep 0.1; done
d2=$(cat "$R/desk.pid")
check "the desktop was ended through its socket" 'grep -qx "ctl --socket wayland-t quit xdg=$R/run" "$R/ctl.txt" && ! kill -0 $d1 2>/dev/null'
check "Cobalt ran and the profile was saved" '[ -e "$RUN/args.txt" ] && grep -q "^cobalt: profile saved on the stick" "$RUN/launcher.log" && [ "$(cookie)" = 10 ]'
check "the desktop runs again: same arguments, same environment" '[ "$d2" != "$d1" ] && kill -0 $d2 2>/dev/null && [ "$(sort -u "$R/desk.starts")" = "--socket wayland-t" ] && [ "$(wc -l <"$R/desk.starts")" = 2 ] && [ "$(wc -l <"$R/desk.marks")" = 2 ] && grep -qx "cobalt: desktop started again (pid $d2)" "$RUN/launcher.log"'
kill -TERM "$d2" 2>/dev/null
for _ in $(seq 1 50); do kill -0 "$d2" 2>/dev/null || break; sleep 0.1; done

# YT-5: the A/V numbers file and `cobalt --stats` (the fake Cobalt writes a small one).
launch; rc=$?
check "A/V numbers file named, no trace by default" 'grep -qx "BITSOS_AV_STATS=$RUN/av-stats.txt" "$RUN/env.txt" && ! grep -q "^BITSOS_AV_TRACE=" "$RUN/env.txt"'
launch --stats; rc=$?
check "--stats prints the last session's numbers" '[ $rc = 0 ] && grep -qx "cobalt: Cobalt does not run (numbers of the last session)" "$R/out" && grep -qx "frames_shown=7" "$R/out"'
COBALT_AV_TRACE=1 launch; rc=$?
check "COBALT_AV_TRACE=1 adds the per-frame trace" '[ $rc = 0 ] && grep -qx "BITSOS_AV_TRACE=$RUN/av-trace.txt" "$RUN/env.txt"'
rm -f "$RUN/av-stats.txt"
launch --stats; rc=$?
check "--stats without numbers says so" '[ $rc = 1 ] && grep -q "^cobalt: no numbers in $RUN/av-stats.txt" "$R/out"'

# YT-5: patch 0007 still carries the Starboard rules that host/tests/yt/starboard_rules_test.cc tests (a
# regenerated patch that lost a hunk would otherwise pass every test without the Cobalt container).
P7=$wt/user/ports/cobalt-launcher/patches/0007-starboard-v2-framebuffer-evdev-backend.patch
sect() { awk -v f="a/$1 " '/^diff --git /{on = index($0, f) > 0} on' "$P7" >"$R/out"; }
sect starboard/linux/shared/media_is_video_supported.cc
check "0007: video capped by VideoSizeSupported, HDR refused" 'grep -q "^+#include \"starboard/shared/bitsos/fb/media_caps.h\"" "$R/out" && grep -q "^+  if (!bitsos_fb::VideoSizeSupported(frame_width, frame_height)) {" "$R/out" && grep -q "^+  if (!IsSDRVideo(bit_depth, primary_id, transfer_id, matrix_id)) {" "$R/out"'
sect starboard/linux/shared/media_is_audio_supported.cc
check "0007: AC-3/E-AC-3 refused" 'grep -A4 "^+#if defined(STARBOARD_BITSOS_CANVAS)" "$R/out" | grep -q "^+    return false;"'
sect starboard/shared/alsa/alsa_audio_sink_type.cc
check "0007: ALSA frames count as played by the delay" 'grep -q "^+#include \"starboard/shared/bitsos/fb/alsa_played.h\"" "$R/out" && [ "$(grep -c "^+ *played_.Wrote(consumed);" "$R/out")" = 2 ] && grep -q "^+  Report(played_.Played(delayed_frames" "$R/out" && grep -q "^+    ReportPlayed(delayed_frame);" "$R/out" && grep -q "^+        if (played_.Resync(frames_in_buffer)) {" "$R/out"'

if [ $fails = 0 ]; then echo "launcher: all checks passed"; rm -rf "$R"; exit 0; fi
echo "launcher: $fails checks failed (run dir $R)"; exit 1
