#!/usr/bin/env bash
# BR-2: the `brave` launcher (user/ports/brave-launcher/bitsos/brave) under BusyBox's sh on the build host, with a
# fake tree (a fake brave that answers LD_TRACE_LOADED_OBJECTS and writes into its --user-data-dir, a fake ld.so), a
# fake desktop (a process with XDG_RUNTIME_DIR and a real Wayland-named socket), fake mount tables and a fake
# pidof. Checks --check, the libraries, the desktop, the user switch, the three profile places (data volume, tar
# on the stick with .old fallback and unclean ends, RAM), the downloads folder (BR-7), the environment and flags
# (BR-8: the GPU's), the sandbox that is always on (BR-6b), arguments and the exit code.
# Host only, a few seconds; nothing outside the run directory, no Brave, no QEMU, no screen.
set -uo pipefail
here=$(cd "$(dirname "$0")" && pwd)
wt=${BITSOS_ROOT:-$(cd "$here/../../.." && pwd)}
R=${BITSOS_RUN_DIR:-$wt/target/br/launcher}/fake
L=$wt/user/ports/brave-launcher/bitsos/brave
BB=$wt/user/ports/busybox/out/busybox
if [ ! -x "$BB" ]; then
  # BusyBox is built against the program sysroot (musl); a fresh checkout has neither yet.
  [ -d "$wt/user/sysroot/out/sysroot" ] || bash "$wt/user/sysroot/build.sh" >/dev/null 2>&1 || true
  V2_OFFLINE=1 sh "$wt/user/ports/busybox/build.sh" >/dev/null 2>&1 || true
fi
[ -x "$BB" ] || { echo "RED: no BusyBox build ($BB): user/sysroot/build.sh or user/ports/busybox/build.sh failed"; exit 1; }
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
# BR-6: the launcher's sandbox probe (timeout, then BusyBox unshare -r -p -n -f chroot / /bin/sh -c :);
# FAKE_NOUSERNS = a kernel without them, FAKE_USERNS_HANG = a kernel call that never returns.
rm -f "$R/bin/unshare"
printf '#!%s sh\n[ "$*" = "-r -p -n -f chroot / /bin/sh -c :" ] || { echo "unshare: unexpected arguments $*" >&2; exit 2; }\n[ -z "$FAKE_USERNS_HANG" ] || exec sleep 60\n[ -z "$FAKE_NOUSERNS" ] && exit 0\necho "unshare: unshare(0x70000000): Function not implemented" >&2\nexit 1\n' "$BB" >"$R/bin/unshare"
chmod +x "$R/bin/pidof" "$R/rootbin/id" "$R/rootbin/chpst.ok" "$R/bin/unshare"

T=$R/mnt/BITSOS2/brave
mkdir -p "$T/lib/gbm" "$T/lib/dri" "$T/locales" "$T/etc/fonts" "$T/share/glib-2.0/schemas" "$T/fonts" "$T/fontcache" "$T/share/mime" \
  "$T/share/icons/Adwaita" "$T/etc/gdk-pixbuf" "$T/glycin" "$T/share/glycin-loaders/2+/conf.d" "$T/libexec" "$T/share/glvnd/egl_vendor.d"
for f in resources.pak icudtl.dat v8_context_snapshot.bin locales/en-US.pak lib/libgtk-3.so.0 lib/libsoftokn3.so \
    lib/libfreeblpriv3.so etc/fonts/fonts.conf share/glib-2.0/schemas/gschemas.compiled fonts/a.ttf fonts/b.ttf \
    fontcache/x-le64.cache-12 share/mime/mime.cache share/icons/Adwaita/index.theme etc/gdk-pixbuf/loaders.cache \
    glycin/glycin-image-rs share/glycin-loaders/2+/conf.d/glycin-image-rs.conf libexec/bwrap lib/libEGL.so.1 \
    lib/libEGL_mesa.so.0 lib/libGLESv2.so.2 lib/libgbm.so.1 lib/gbm/dri_gbm.so lib/libva.so.2 lib/libva-drm.so.2 \
    lib/dri/radeonsi_drv_video.so share/glvnd/egl_vendor.d/50_mesa.json; do : >"$T/$f"; done
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
if [ -e "$R/nosb" ]; then
  echo "[1:1:1027/000000.000000:FATAL:content/browser/zygote_host/zygote_host_impl_linux.cc(128)] No usable sandbox! fake"
  exit 133
fi
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
  BRAVE_APPS="${BAPPS:-$R/noapps}" BRAVE_BITSRUN="$R/bitsrun" ${BSTICK:+BRAVE_TEST_STICK=$BSTICK} \
  BRAVE_DATA_STATE="${STATE_F:-$R/nostate/data-state}" ${BRAVE_DATA_WAIT:+BRAVE_DATA_WAIT=$BRAVE_DATA_WAIT} \
  ${WD:+WAYLAND_DISPLAY=$WD} FAKE_BITSCOMP="${DESK-$desk}" FAKE_MISSING="${FAKE_MISSING:-}" FAKE_NOUSERNS="${FAKE_NOUSERNS:-}" FAKE_USERNS_HANG="${FAKE_USERNS_HANG:-}" ${SB_SECS:+BRAVE_TEST_SANDBOX_SECS=$SB_SECS} ${BRAVE_WAIT:+BRAVE_WAIT=$BRAVE_WAIT} ${THEME_T:+BRAVE_THEME=$THEME_T} ${HOUR_T:+THEME_HOUR=$HOUR_T} LANG=de_DE.UTF-8 PATH="$R/bin" \
  "$BB" sh "$L" "$@" >"$R/${OUT:-out}" 2>&1; }
cookie() { "$BB" tar -xOf "${1:-$PROF}" profile/Cookies 2>/dev/null; }
has() { grep -q -- "$1" "$R/args.txt"; }
envv() { grep -qx -- "$1" "$R/env.txt"; }

launch --check; rc=$?
check "--check passes on a whole tree" '[ $rc = 0 ] && grep -qx "brave: tree $T: ok" "$R/out"'
check "--check counts distinct libraries over brave and the dlopened ones (BR-8: the GPU's too)" 'grep -qx "brave: libraries: 13 found, none missing" "$R/out"'
check "--check: fonts, desktop, user, sound" 'grep -qx "brave: fonts: 2 in $T/fonts, 1 prebuilt cache file(s)" "$R/out" && grep -qx "brave: desktop: $R/xdg/wayland-bits-0" "$R/out" && grep -qx "brave: user: runs as uid $(id -u)" "$R/out" && grep -qx "brave: sound: no ALSA configuration (/nonexistent)" "$R/out"'
check "--check: no data volume, no home yet: RAM only" 'grep -q "^brave: profile: in RAM only ($RUN/profile): no data volume at $R/data and $P is not writable for uid" "$R/out"'
check "--check: downloads in RAM without a data volume" 'grep -qx "brave: downloads: $RUN/home/Downloads (in RAM)" "$R/out"'
FAKE_MISSING=1 launch --check; rc=$?
check "--check fails on a missing library" '[ $rc = 1 ] && grep -qx "brave: libraries missing: libcups.so.2 => not found" "$R/out"'
mv "$T/lib/libgtk-3.so.0" "$R/gtk"; launch --check; rc=$?
check "--check fails on a missing file" '[ $rc = 1 ] && grep -q "^brave: tree $T: lib/libgtk-3.so.0 is missing" "$R/out"'
launch; rc=$?
check "PE-1: a start with a missing file says it in plain German (the desktop's card shows only that line)" '[ $rc = 1 ] && grep -q "^brave: tree $T: lib/libgtk-3.so.0 is missing" "$R/out" && [ "$(grep "^» " "$R/out")" = "» Brave startet nicht: Auf dem Stick fehlen Dateien von Brave." ]'
mv "$R/gtk" "$T/lib/libgtk-3.so.0"
mv "$T/share/mime/mime.cache" "$R/mime"; launch --check; rc=$?
check "BR-7: --check fails without the MIME database" '[ $rc = 1 ] && grep -q "^brave: tree $T: share/mime/mime.cache is missing" "$R/out"'
mv "$R/mime" "$T/share/mime/mime.cache"
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
check "as root without id change: refused in German and English" '[ $rc = 1 ] && grep -qx "» Brave startet nicht mit Verwalterrechten. Bitte als normaler Benutzer starten." "$R/out" && grep -q "^brave: Brave does not run as root: " "$R/out" && [ ! -e "$R/args.txt" ]'
cp "$R/rootbin/chpst.ok" "$R/rootbin/chpst"
XPATH="$R/rootbin:$R/bin" launch --check; rc=$?
check "as root with id change: --check says uid 1000, the stick's tar profile" '[ $rc = 0 ] && grep -qx "brave: user: runs as uid 1000 (started by root: chpst -u 1000:1000)" "$R/out" && grep -q "^brave: profile: $PROF (packed back" "$R/out"'
rm -f "$R/rootbin/chpst"

# The data volume: the profile on it, the page cache with it, Brave's environment and flags. /data/home/1000 as
# the kernel makes it (FS-8c).
mkdir -p "$R/data/home/1000"
MNT=data launch http://127.0.0.1:31001/ -- --lang=de; rc=$?
check "data volume: the profile in appdata" '[ $rc = 0 ] && grep -qx "brave: profile: $R/data/appdata/brave" "$R/out" && has "--user-data-dir=$R/data/appdata/brave" && ! has "--disk-cache-dir" && [ "$(cat "$R/data/appdata/brave/Cookies")" = 1 ]'
check "BR-1's flags with BR-8's GPU (ANGLE on GL, VA-API decode), without --no-sandbox, the URL and the extra arguments" 'has "^--ozone-platform=wayland --use-gl=angle --use-angle=gl --enable-features=AcceleratedVideoDecoder,AcceleratedVideoDecodeLinuxGL,VaapiIgnoreDriverChecks --password-store=basic --disable-breakpad --disable-crash-reporter --no-first-run --no-default-browser-check --autoplay-policy=no-user-gesture-required --enable-wayland-ime --wayland-text-input-version=3 --user-data-dir=" && has " http://127.0.0.1:31001/ --lang=de$" && ! has "-sandbox"'
check "BR-6: the sandbox is on (namespaces probed as Brave's user)" 'grep -qx "brave: sandbox: on (user, PID and network namespaces; the kernel names seccomp in /proc: \(yes\|no\))" "$R/out"'
check "a clean environment pointing into the tree" 'envv "LD_LIBRARY_PATH=$T/lib" && envv "FONTCONFIG_FILE=$T/etc/fonts/fonts.conf" && envv "WAYLAND_DISPLAY=wayland-bits-0" && envv "XDG_RUNTIME_DIR=$R/xdg" && envv "HOME=$RUN/home" && envv "GSETTINGS_SCHEMA_DIR=$T/share/glib-2.0/schemas" && envv "XCURSOR_PATH=$T/share/icons" && envv "LANG=de_DE.UTF-8" && ! grep -q "^FAKE_\|^BRAVE_" "$R/env.txt"'
check "BR-8: the GPU's libraries and files from the tree" 'envv "__EGL_VENDOR_LIBRARY_FILENAMES=$T/share/glvnd/egl_vendor.d/50_mesa.json" && envv "GBM_BACKENDS_PATH=$T/lib/gbm" && envv "DRIRC_CONFIGDIR=$T/share/drirc.d" && envv "AMDGPU_ASIC_ID_TABLE_PATHS=$T/share/libdrm" && envv "LIBVA_DRIVERS_PATH=$T/lib/dri" && envv "LIBVA_DRIVER_NAME=radeonsi" && ! has "--disable-gpu"'
check "light or unknown theme: no forced dark mode" '! has "--force-dark-mode"'
check "BR-7: image loaders, MIME database and icons from the tree" 'envv "GDK_PIXBUF_MODULE_FILE=$T/etc/gdk-pixbuf/loaders.cache" && envv "PATH=$R/bin:$T/libexec" && envv "XDG_DATA_DIRS=$T/share:/usr/share"'
check "BR-7: data volume: downloads into /data/home/1000/Downloads (user-dirs.dirs)" 'grep -qx "brave: downloads: $R/data/home/1000/Downloads (data volume)" "$R/out" && [ -d "$R/data/home/1000/Downloads" ] && grep -qx "XDG_DOWNLOAD_DIR=\"$R/data/home/1000/Downloads\"" "$RUN/home/.config/user-dirs.dirs"'
MNT=data launch; rc=$?
check "data volume: the lock link of a Brave that is gone is dropped" '[ $rc = 0 ] && [ "$(cat "$R/lock.txt")" = none ] && [ "$(cat "$R/data/appdata/brave/Cookies")" = 2 ] && [ ! -e "$RUN/.lock" ]'
THEME_T=dark MNT=data launch http://127.0.0.1:31001/; rc=$?
check "dark desktop theme: Brave and web pages dark (--force-dark-mode)" '[ $rc = 0 ] && has " --force-dark-mode" && ! has "--no-sandbox"'
# WY-SET: "Nach Uhrzeit" (auto) as the desktop: light from 07 to 19 (no settingsd here: its default hours).
for hm in 06:dark 07:light 10:light 18:light 19:dark 23:dark; do
  HOUR_T=${hm%:*} THEME_T=auto MNT=data launch http://127.0.0.1:31001/; rc=$?
  if [ "${hm#*:}" = dark ]; then
    check "theme auto at ${hm%:*} o'clock: dark like the desktop" '[ $rc = 0 ] && has " --force-dark-mode"'
  else
    check "theme auto at ${hm%:*} o'clock: light like the desktop" '[ $rc = 0 ] && ! has "--force-dark-mode"'
  fi
done
# BR-6b: the sandbox is always on: probed and said, never --no-sandbox. A probe that hangs ends after its time (B4)
# and counts as no sandbox.
FAKE_USERNS_HANG=1 SB_SECS=1 MNT=data launch; rc=$?
check "BR-6b: a hanging probe ends after its time, said as missing, Brave starts with its sandbox (Chromium decides)" '[ $rc = 0 ] && ! has "--no-sandbox" && grep -qx "brave: sandbox: missing (no user, PID and network namespace for uid [0-9]*: no answer within 1 s; Chromium decides)" "$R/out"'
# The probe passes, Chromium finds no usable sandbox: Brave stays ended.
: >"$R/nosb"; rm -f "$R/args.txt"
MNT=data launch; rc=$?
check "BR-6b: Chromium's 'No usable sandbox!' ends the start (no --no-sandbox, no second start), said in English and German" '[ $rc = 133 ] && [ ! -e "$R/args.txt" ] && grep -qx "brave: sandbox: missing (Chromium: No usable sandbox! fake; Brave stays ended)" "$R/out" && grep -qx "» Brave startet nicht: Der Schutzraum, in dem Brave Webseiten öffnet, fehlt in BitsOS noch." "$R/out" && [ "$(grep -c "^» " "$R/out")" = 1 ] && grep -q "^brave:   \[.*No usable sandbox! fake$" "$R/out"'
FAKE_NOUSERNS=1 MNT=data launch; rc=$?
check "BR-6b: kernel without namespaces: the probe says missing, Chromium refuses, nothing falls back" '[ $rc = 133 ] && grep -q "^brave: sandbox: missing (no user, PID and network namespace for uid [0-9]*: unshare: unshare(0x70000000): Function not implemented; " "$R/out" && grep -q "^brave: sandbox: missing (Chromium: " "$R/out" && ! grep -q "sandbox: off" "$R/out"'
rm -f "$R/nosb"
# A kernel without user namespaces: said and no --no-sandbox.
FAKE_NOUSERNS=1 THEME_T=dark MNT=data launch http://127.0.0.1:31001/; rc=$?
check "BR-6b: no user namespaces: said as missing, no --no-sandbox, the theme kept" '[ $rc = 0 ] && has "^--ozone-platform=wayland .*--autoplay-policy=no-user-gesture-required --enable-wayland-ime --wayland-text-input-version=3 --force-dark-mode --user-data-dir=" && ! has "--no-sandbox" && grep -qx "brave: sandbox: missing (no user, PID and network namespace for uid [0-9]*: unshare: unshare(0x70000000): Function not implemented; Chromium decides)" "$R/out"'
FAKE_NOUSERNS=1 MNT=data launch --check; rc=$?
check "BR-6b: --check names a missing sandbox and fails" '[ $rc = 1 ] && grep -q "^brave: sandbox: missing (no user, PID and network namespace for uid [0-9]*: " "$R/out"'
MNT=datafat launch --check
check "a vfat /data is no data volume" 'grep -q "^brave: profile: $PROF (packed back after a clean end; $R/data is vfat)" "$R/out" || grep -q "^brave: profile: in RAM only .*$R/data is vfat" "$R/out"'
MNT=data launch --check
check "BR-7: --check names the data volume's downloads folder" 'grep -qx "brave: downloads: $R/data/home/1000/Downloads (data volume)" "$R/out"'
check "BR-7: --check says the image loaders run without a sandbox (the stand-in)" 'grep -qx "brave: image loaders: without a sandbox (the tree'"'"'s bwrap stand-in; no real bwrap on PATH)" "$R/out"'
printf '#!%s sh\nexit 0\n' "$BB" >"$R/bin/bwrap"; chmod +x "$R/bin/bwrap"
MNT=data launch --check
rm -f "$R/bin/bwrap"
check "BR-7: a real bwrap on PATH comes before the stand-in" 'grep -qx "brave: image loaders: sandbox by $R/bin/bwrap" "$R/out"'
rm -rf "$R/data/home/1000/Downloads"; ln -s "$R/tmp" "$R/data/home/1000/Downloads"
MNT=data launch; rc=$?
check "BR-7: a link at Downloads is not followed: HOME's folder" '[ $rc = 0 ] && grep -qx "brave: downloads: $R/data/home/1000/Downloads cannot be made (not uid 1000'"'"'s own folder?)" "$R/out" && grep -qx "brave: downloads: $RUN/home/Downloads (in RAM: gone at the next boot)" "$R/out" && grep -qx "XDG_DOWNLOAD_DIR=\"$RUN/home/Downloads\"" "$RUN/home/.config/user-dirs.dirs"'
rm -f "$R/data/home/1000/Downloads"
if [ "$(id -u)" = 1000 ]; then
  # Root makes /data/home/1000 for uid 1000 when a volume lacks it (chown to 1000 works for uid 1000 itself here).
  rm -rf "$R/data/home"; cp "$R/rootbin/chpst.ok" "$R/rootbin/chpst"
  XPATH="$R/rootbin:$R/bin" MNT=data launch; rc=$?
  rm -f "$R/rootbin/chpst"
  check "BR-7: started by root, no /data/home/1000 yet: made (0700) with Downloads" '[ $rc = 0 ] && grep -qx "brave: downloads: $R/data/home/1000/Downloads (data volume)" "$R/out" && [ "$("$BB" stat -c %a "$R/data/home/1000")" = 700 ] && [ -d "$R/data/home/1000/Downloads" ]'
  # A chown that fails takes the new folder away again (the next start tries anew), and Brave downloads into RAM.
  rm -rf "$R/data/home"; cp "$R/rootbin/chpst.ok" "$R/rootbin/chpst"
  printf '#!%s sh\nexit 1\n' "$BB" >"$R/rootbin/chown"; chmod +x "$R/rootbin/chown"
  XPATH="$R/rootbin:$R/bin" MNT=data launch; rc=$?
  rm -f "$R/rootbin/chpst" "$R/rootbin/chown"
  check "BR-7: started by root, chown fails: /data/home/1000 taken away again, downloads in RAM" '[ $rc = 0 ] && [ ! -e "$R/data/home/1000" ] && grep -qx "brave: downloads: chown of $R/data/home/1000 failed (taken away again)" "$R/out" && grep -q "^brave: downloads: .*/home/Downloads (in RAM: gone at the next boot)$" "$R/out"'
  # uid 1000 itself makes its folder when /data/home lets it.
  rm -rf "$R/data/home"; mkdir "$R/data/home"
  MNT=data launch; rc=$?
  check "BR-7: started by uid 1000, no /data/home/1000 yet: made (0700) with Downloads" '[ $rc = 0 ] && grep -qx "brave: downloads: $R/data/home/1000/Downloads (data volume)" "$R/out" && [ "$("$BB" stat -c %a "$R/data/home/1000")" = 700 ] && [ -d "$R/data/home/1000/Downloads" ]'
fi

# The stick: one tar file, renames, the copy before (.old), stale lock links dropped, unclean ends not saved.
mkdir -p "$P"
launch; rc=$?
check "stick: first start packs the profile" '[ $rc = 0 ] && grep -q "^brave: profile: new" "$R/out" && grep -q "^brave: profile saved on the stick" "$R/out" && [ "$(cookie)" = 1 ] && has "--disk-cache-dir=$RUN/cache" && has "--user-data-dir=$RUN/profile"'
check "BR-7: no data volume: downloads into HOME (RAM), said so" 'grep -qx "brave: downloads: $RUN/home/Downloads (in RAM: gone at the next boot)" "$R/out" && [ -d "$RUN/home/Downloads" ] && grep -qx "XDG_DOWNLOAD_DIR=\"$RUN/home/Downloads\"" "$RUN/home/.config/user-dirs.dirs"'
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
FAKE_NOUSERNS=1 launch http://127.0.0.1:31003/; rc=$?
check "BR-6: a hand-over keeps the running Brave's sandbox (no probe, no --no-sandbox)" '[ $rc = 0 ] && grep -q "^brave: sandbox: on (as the running Brave, pid [0-9]*)$" "$R/out" && grep -q -- "http://127.0.0.1:31003/" "$R/forward.txt" && ! grep -q -- "--no-sandbox" "$R/forward.txt"'
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
check "a live start without Brave: nothing started" '[ $rc = 1 ] && grep -qx "brave: another start (pid $holder) holds $RUN/.lock, but no Brave runs: nothing started" "$R/out" && grep -qx "» Brave wird schon gestartet, zeigt aber kein Fenster. Bitte gleich noch einmal öffnen." "$R/out"'
kill $holder; rm -rf "$RUN/.lock"
MNT=ro launch; rc=$?
check "a read-only stick: RAM only, nothing written" '[ $rc = 0 ] && grep -q "^brave: profile: in RAM only: no data volume at $R/data and the stick is not mounted writable" "$R/out" && ! grep -q "saved on the stick" "$R/out"'

# INT-W11: /data still coming (the kernel's state line), the stick's tar taken over once.
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

# BR-9: Brave installed from the programs (/apps/brave-bin) is the one Brave: every start goes to bitsrun brave-bin
# (HOME left to passwd, the desktop's socket named), the stick's tree is not started; BRAVE_TEST_STICK=1 still
# starts it. The fake bitsrun ends at once with Chromium's sandbox error while $R/pkgfail exists.
printf '#!%s sh\n{ echo "args=$*"; echo "home=[$HOME]"; echo "wd=$WAYLAND_DISPLAY"; echo "uid=$(id -u)"; } >"%s/bitsrun.txt"\n[ ! -e "%s/pkgfail" ] || { echo "FATAL: No usable sandbox! fake"; exit 133; }\n' "$BB" "$R" "$R" >"$R/bitsrun"
chmod +x "$R/bitsrun"
# A package half removed: current without the binary is no installed Brave.
mkdir -p "$R/apps/brave-bin/current/root/opt/brave-bin"
rm -f "$R/args.txt" "$R/bitsrun.txt"
BAPPS=$R/apps launch --check
check "BR-9: --check: a package without its binary is incomplete, the stick's tree starts" 'grep -qx "brave: installed Brave: $R/apps/brave-bin is incomplete (no $R/apps/brave-bin/current/root/opt/brave-bin/brave): the stick'"'"'s tree starts" "$R/out"'
BAPPS=$R/apps MNT=data launch; rc=$?
check "BR-9: a package without its binary: the stick's tree starts" '[ $rc = 0 ] && [ -e "$R/args.txt" ] && [ ! -e "$R/bitsrun.txt" ]'
rm -f "$R/args.txt"
printf '#!/bin/sh\n' >"$R/apps/brave-bin/current/root/opt/brave-bin/brave"
chmod +x "$R/apps/brave-bin/current/root/opt/brave-bin/brave"
BAPPS=$R/apps launch --check
check "BR-9: --check names the installed Brave" 'grep -qx "brave: installed Brave: $R/apps/brave-bin (every start goes to it)" "$R/out"'
launch --check
check "BR-9: --check without it: none" 'grep -qx "brave: installed Brave: none" "$R/out"'
BAPPS=$R/apps MNT=data launch https://a.test/ -- --start-fullscreen; rc=$?
check "BR-9: a start goes to the installed Brave, arguments as given" '[ $rc = 0 ] && grep -qx "args=brave-bin https://a.test/ -- --start-fullscreen" "$R/bitsrun.txt" && grep -qx "home=\[\]" "$R/bitsrun.txt" && grep -qx "wd=wayland-bits-0" "$R/bitsrun.txt" && [ ! -e "$R/args.txt" ] && grep -q "^brave: starting the installed Brave" "$R/out"'
check "BR-9: the installed Brave's end is said, its output in the run folder's log" 'grep -qx "brave: the installed Brave ended (code 0) after [0-9]* s" "$R/out" && grep -q "^brave: starting the installed Brave (.* log $RUN/installed.log)" "$R/out"'
# It ends with an error (Chromium found no sandbox in the view): its code, the plain German line and its last
# lines; nothing starts in its place.
: >"$R/pkgfail"; rm -f "$R/bitsrun.txt"
BAPPS=$R/apps MNT=data launch https://a.test/; rc=$?
check "BR-9: the installed Brave ends with an error: its code, plain German line, its last lines, the stick's tree not started" '[ $rc = 133 ] && [ -e "$R/bitsrun.txt" ] && [ ! -e "$R/args.txt" ] && grep -qx "brave: the installed Brave ended (code 133) after [0-9]* s" "$R/out" && grep -qx "» Brave hat sich mit einem Fehler beendet." "$R/out" && grep -qx "brave:   FATAL: No usable sandbox! fake" "$R/out"'
rm -f "$R/pkgfail" "$R/bitsrun.txt"
BAPPS=$R/apps BSTICK=1 MNT=data launch; rc=$?
check "BR-9: BRAVE_TEST_STICK=1 starts the stick's tree" '[ $rc = 0 ] && [ -e "$R/args.txt" ] && [ ! -e "$R/bitsrun.txt" ]'
rm -f "$R/args.txt"
DESK= BAPPS=$R/apps launch; rc=$?
check "BR-9: no desktop: the installed Brave is not started either" '[ $rc = 1 ] && [ ! -e "$R/bitsrun.txt" ] && grep -qx "brave: desktop: none runs (start the desktop first)" "$R/out"'
cp "$R/rootbin/chpst.ok" "$R/rootbin/chpst"
XPATH="$R/rootbin:$R/bin" BAPPS=$R/apps launch; rc=$?
check "BR-9: started by root: bitsrun runs as uid 1000" '[ $rc = 0 ] && grep -qx "uid=1000" "$R/bitsrun.txt" && grep -qx "args=brave-bin" "$R/bitsrun.txt"'
rm -f "$R/rootbin/chpst"

if [ $fails = 0 ]; then echo "launcher: all checks passed"; rm -rf "$R"; exit 0; fi
echo "launcher: $fails checks failed (run dir $R)"; exit 1
