#!/usr/bin/env bash
# LO-1: the staged LibreOffice tree (user/ports/libreoffice-launcher/scripts/stage-v2.sh) and its launcher on the build
# host, a few seconds, no QEMU: the tree matches the installed package version, holds no link (FAT has none; CachyOS's
# links into /etc/libreoffice are real files), leaves out what the stage says it leaves out, has GTK's data as Brave's
# tree has it, and the launcher (with a fake mount table: a data volume, then none) says the right home and gives
# LibreOffice the tree's environment; a headless conversion through the launcher makes a PDF (the restart of a new
# profile included); German menus and spell checking (libreoffice-fresh-de, hunspell-de) when installed, else the way a
# dictionary takes with a test dictionary. RED without a stage: a test that cannot run is not green (CONTRACTS §4).
set -uo pipefail
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
S=${LO_STAGE:-/var/tmp/claude-v2/stage/libreoffice}
V2=/boot/BITSOS2/libreoffice
L=user/ports/libreoffice-launcher/bitsos/libreoffice
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/lo-05-stage}"
mkdir -p "$RUN_DIR"
RUN_DIR="$(realpath "$RUN_DIR")"
[ -x "$S/program/soffice.bin" ] || { echo "RED: no stage at $S (bash user/ports/libreoffice-launcher/scripts/stage-v2.sh makes it)"; exit 1; }
fails=0
check() { if eval "$2"; then echo "ok   $1"; else echo "FAIL $1"; fails=$((fails + 1)); fi; }

ver=$(LC_ALL=C pacman -Q libreoffice-fresh 2>/dev/null | cut -d' ' -f2)
check "VERSION.txt names the installed libreoffice-fresh ($ver)" 'grep -q "^libreoffice-fresh $ver " "$S/VERSION.txt"'
check "no link in the tree" '[ -z "$(find "$S" -type l -print -quit)" ]'
for f in program/bootstraprc program/sofficerc share/psprint/psprint.conf; do
  check "$f is the package's /etc/libreoffice file" '[ -f "$S/$f" ] && [ ! -L "$S/$f" ] && [ -s "$S/$f" ]'
done
check "bootstraprc puts the profile under \$SYSUSERCONFIG (HOME/.config)" 'grep -q "^UserInstallation=\$SYSUSERCONFIG/libreoffice/4" "$S/program/bootstraprc"'
check "left out: help, Java classes, Python, the Qt/KDE/GTK 4/X11 plugins" \
  '[ ! -e "$S/help" ] && [ ! -e "$S/program/classes" ] && [ ! -e "$S/program/libpyuno.so" ] && [ -z "$(find "$S/program" -name "*.py" -print -quit)" ] && [ ! -e "$S/program/libvclplug_qt6lo.so" ] && [ ! -e "$S/program/libvclplug_kf6lo.so" ] && [ ! -e "$S/program/libvclplug_gtk4lo.so" ] && [ ! -e "$S/program/libvclplug_genlo.so" ]'
check "the gtk3 plugin, Writer, Calc, Impress, Draw are there" \
  '[ -f "$S/program/libvclplug_gtk3lo.so" ] && [ -f "$S/program/libswlo.so" ] && [ -f "$S/program/libsclo.so" ] && [ -f "$S/program/libsdlo.so" ]'
check "icon themes: colibre and colibre_dark only" '[ "$(cd "$S/share/config" && LC_ALL=C ls images_*.zip | tr "\n" " ")" = "images_colibre.zip images_colibre_dark.zip " ]'
check "bitsos.xcd (no tip of the day) in share/registry" 'grep -q "ShowTipOfTheDay" "$S/share/registry/bitsos.xcd"'
check "every library of program/ finds its libraries on this host" \
  'out=$(for f in "$S"/program/*.so "$S"/program/*.so.* "$S"/program/soffice.bin; do LD_LIBRARY_PATH="$S/program" ldd "$f" 2>&1; done) && ! grep -q "not found" <<<"$out"'
check "fonts: Liberation and DejaVu" '[ -f "$S/fonts/LiberationSans-Regular.ttf" ] && [ -f "$S/fonts/DejaVuSans.ttf" ]'
check "fonts.conf names $V2/fonts and the user's cache" 'grep -q "<dir>$V2/fonts</dir>" "$S/etc/fonts/fonts.conf" && grep -q "prefix=\"xdg\"" "$S/etc/fonts/fonts.conf"'
CONF=$S/gtk/share/glycin-loaders/2+/conf.d/glycin-image-rs.conf
check "glycin's loader and its config (Exec= into the tree)" '[ -x "$S/gtk/glycin/glycin-image-rs" ] && grep -q "^Exec=" "$CONF" && ! grep "^Exec=" "$CONF" | grep -qvx "Exec=$V2/gtk/glycin/glycin-image-rs"'
check "the loader is this host's (libglycin comes from the host)" 'cmp -s "$S/gtk/glycin/glycin-image-rs" "/usr/lib/glycin-loaders/2+/glycin-image-rs"'
check "MIME database, GTK's schemas, loaders.cache, bwrap stand-in" \
  '[ -s "$S/gtk/share/mime/mime.cache" ] && [ -s "$S/gtk/share/glib-2.0/schemas/gschemas.compiled" ] && [ -f "$S/gtk/etc/gdk-pixbuf/loaders.cache" ] && [ -x "$S/gtk/libexec/bwrap" ]'
check "no SVG under gtk/share/icons; Adwaita and hicolor with their caches" \
  '[ -z "$(find "$S/gtk/share/icons" -name "*.svg" -print -quit)" ] && [ -s "$S/gtk/share/icons/Adwaita/icon-theme.cache" ] && [ -s "$S/gtk/share/icons/hicolor/icon-theme.cache" ]'
check "hicolor has LibreOffice's application icons" '[ -f "$S/gtk/share/icons/hicolor/48x48/apps/libreoffice-writer.png" ] && [ -f "$S/gtk/share/icons/hicolor/48x48/apps/libreoffice-calc.png" ]'
check "licences: LibreOffice's and every copied package's" \
  '[ -s "$S/LICENSES/libreoffice.txt" ] && for p in ttf-liberation ttf-dejavu noto-fonts adwaita-icon-theme shared-mime-info nss glycin; do [ -n "$(ls "$S/LICENSES/$p" 2>/dev/null)" ] || exit 1; done'

# The launcher on the host: a fake mount table with and without a data volume (LO_* settings), --check and --env.
F=$RUN_DIR/fake
rm -rf "$F"
mkdir -p "$F/data/home/1000" "$F/run" "$F/out"
chmod 700 "$F/data/home/1000"
echo "/dev/x $F/data ext4 rw,relatime 0 0" >"$F/mounts.data"
: >"$F/mounts.none"
run() { ${RUN_EXEC:+exec} env -i PATH=/usr/bin:/bin HOME=/ ${LO_LANG:+LO_LANG=$LO_LANG} LO_TREE="${RUN_TREE:-$S}" LO_DATA="$F/data" LO_MOUNTS="$F/mounts.$1" LO_USER_RUN="$F/run" LO_TMP="$F/run" \
  LO_PATH=/usr/bin:/bin LO_DATA_STATE="$F/nostate/x" LO_LDSO=/usr/lib/ld-linux-x86-64.so.2 sh "$L" "${@:2}"; }
out=$(run data --check 2>&1); rc=$?
echo "$out" >"$RUN_DIR/check-data.txt"
check "--check with a data volume: exit 0, libraries found" '[ $rc = 0 ] && grep -q "libraries: [0-9]* found, none missing" <<<"$out"'
check "--check: home and profile on the data volume" 'grep -qx "libreoffice: home: $F/data/home/1000 (data volume; documents and settings stay)" <<<"$out" && grep -qx "libreoffice: profile: $F/data/home/1000/.config/libreoffice/4" <<<"$out"'
check "--check: German locale without a settings registry" 'grep -q "^libreoffice: language: LANG=de_DE.UTF-8" <<<"$out"'
out=$(run none --check 2>&1)
check "--check without a data volume: home in RAM, said" 'grep -q "^libreoffice: home: $F/run/libreoffice-$(id -u)/home (in RAM: gone at the next boot; no data volume at $F/data)" <<<"$out"'
env=$(run data --env 2>&1)
echo "$env" >"$RUN_DIR/env.txt"
for kv in "HOME=$F/data/home/1000" "LD_LIBRARY_PATH=$S/lib" SAL_USE_VCLPLUGIN=gtk3 GDK_BACKEND=wayland "FONTCONFIG_FILE=$S/etc/fonts/fonts.conf" \
    "XDG_DATA_DIRS=$S/gtk/share" "GDK_PIXBUF_MODULE_FILE=$S/gtk/etc/gdk-pixbuf/loaders.cache" GSETTINGS_BACKEND=memory LANG=de_DE.UTF-8 \
    DBUS_SESSION_BUS_ADDRESS=disabled: SAL_DISABLE_OPENCL=1; do
  check "--env: $kv" 'grep -qx -- "$kv" <<<"$env"'
done
check "--env: nothing of the caller's (no LO_*, no DISPLAY)" '! grep -q "^LO_\|^DISPLAY=" <<<"$env"'
check "--env: LANG from LO_LANG" 'LO_LANG=en_US.UTF-8 run data --env | grep -qx LANG=en_US.UTF-8'

# A conversion through the launcher, as on V2 (the host's libraries; V2 runs it in ktest ul.pkg.libreoffice_convert).
printf 'LO-1 Hallo\n' >"$F/out/brief.txt"
out=$(run data --convert-to pdf --outdir "$F/out" "$F/out/brief.txt" 2>&1); rc=$?
echo "$out" >"$RUN_DIR/convert.txt"
check "--convert-to pdf: exit 0, the new profile's restart (81) handled" '[ $rc = 0 ] && grep -q "asked for a restart (code 81" <<<"$out" && grep -q "LibreOffice ended (code 0)" <<<"$out"'
check "the PDF is whole" 'head -c 5 "$F/out/brief.pdf" | grep -q "%PDF-" && tail -c 1024 "$F/out/brief.pdf" | grep -aq "%%EOF"'
check "the profile is in the data volume's home" '[ -f "$F/data/home/1000/.config/libreoffice/4/user/registrymodifications.xcu" ]'
out=$(run data --convert-to pdf --outdir "$F/out" "$F/out/brief.txt" 2>&1)
check "a second conversion keeps the profile (no restart)" '! grep -q "code 81" <<<"$out" && grep -q "LibreOffice ended (code 0)" <<<"$out"'

# German menus and spell checking come with libreoffice-fresh-de and hunspell-de when they are installed here: then the
# stage must hold them; else a second stage with a small test dictionary (LO_DICT_DIR) shows that LibreOffice takes a
# dictionary from share/hunspell through bitsos-dicts.xcd (asked over UNO by spell_client.py, the host's Python).
if pacman -Q libreoffice-fresh-de >/dev/null 2>&1; then
  check "libreoffice-fresh-de is installed: the stage has the German language pack" '[ -f "$S/share/registry/res/registry_de.xcd" ] || [ -f "$S/share/registry/Langpack-de.xcd" ]'
  check "--check says German menus" 'grep -q "^libreoffice: language: LANG=de_DE.UTF-8, menus German$" "$RUN_DIR/check-data.txt"'
else
  echo "note: libreoffice-fresh-de is not installed here: the menus stay English (the launcher says so)"
  check "--check says why the menus are English" 'grep -q "menus English (the tree has no German language pack" "$RUN_DIR/check-data.txt"'
fi
DS=$S GOOD=Haus BAD=Hauss
if pacman -Q hunspell-de >/dev/null 2>&1; then
  check "hunspell-de is installed: the stage has de_DE in share/hunspell and bitsos-dicts.xcd" '[ -n "$(ls "$S"/share/hunspell/de_DE*.dic 2>/dev/null)" ] && grep -q "<value>de-DE</value>" "$S/share/registry/bitsos-dicts.xcd"'
else
  echo "note: hunspell-de is not installed here: a second stage with a test dictionary checks the way in"
  mkdir -p "$F/dict"
  printf 'SET UTF-8\nTRY esianrtolcdugmphbyfvkwz\n' >"$F/dict/de_DE.aff"
  printf '3\nHaus\nBaum\nStick\n' >"$F/dict/de_DE.dic"
  DS=$RUN_DIR/stage-dict
  LO_STAGE=$DS LO_DICT_DIR=$F/dict bash user/ports/libreoffice-launcher/scripts/stage-v2.sh >"$RUN_DIR/stage-dict.log" 2>&1
  check "the stage with LO_DICT_DIR: share/hunspell/de_DE.* and bitsos-dicts.xcd" '[ -f "$DS/share/hunspell/de_DE.dic" ] && grep -q "<value>de-DE</value>" "$DS/share/registry/bitsos-dicts.xcd"'
  GOOD=Stick BAD=Stik
fi
pipe=lo1-spell-$$
RUN_TREE=$DS run data --headless --accept="pipe,name=$pipe;urp;" >"$RUN_DIR/spell-lo.txt" 2>&1 &
lo=$!
out=$(python3 host/tests/lo/spell_client.py "$pipe" "$GOOD" "$BAD" 2>&1); rc=$?
echo "$out" >"$RUN_DIR/spell.txt"
check "LibreOffice spell-checks de-DE with the stage's dictionary ($out)" '[ $rc = 0 ]'
for _ in $(seq 60); do kill -0 $lo 2>/dev/null || break; sleep 0.5; done
# A LibreOffice of this check that did not end by itself (the client failed early) is ended by its pid.
for p in $(pidof soffice.bin); do [ "$(readlink "/proc/$p/exe")" != "$DS/program/soffice.bin" ] || kill "$p"; done
kill $lo 2>/dev/null
wait $lo 2>/dev/null
[ "$DS" = "$S" ] || rm -rf "$DS"

# SIGTERM to the launcher ends LibreOffice (its own clean end) and the launcher with it.
RUN_EXEC=1 RUN_TREE=$S run data --headless --accept="pipe,name=lo1-term-$$;urp;" >"$RUN_DIR/term.txt" 2>&1 &
lo=$!
for _ in $(seq 60); do grep -q "starting" "$RUN_DIR/term.txt" && [ -n "$(pidof soffice.bin)" ] && break; sleep 0.5; done
sleep 2
kill -TERM $lo
ended=no
for _ in $(seq 40); do kill -0 $lo 2>/dev/null || { ended=yes; break; }; sleep 0.5; done
left=$(for p in $(pidof soffice.bin); do [ "$(readlink "/proc/$p/exe")" != "$S/program/soffice.bin" ] || echo "$p"; done)
check "SIGTERM to the launcher ends it and LibreOffice within 20 s" '[ $ended = yes ] && [ -z "$left" ] && grep -q "LibreOffice ended" "$RUN_DIR/term.txt"'
for p in $left; do kill "$p"; done
wait $lo 2>/dev/null
rm -rf "$F"
if [ $fails != 0 ]; then echo "RED: $fails check(s) failed"; exit 1; fi
echo "GREEN: the staged LibreOffice tree and its launcher"
