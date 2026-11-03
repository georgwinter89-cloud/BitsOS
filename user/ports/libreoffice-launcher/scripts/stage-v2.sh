#!/usr/bin/env bash
# LO-1: builds the tree that `cargo xtask image` puts into \BITSOS2\libreoffice\ on the stick (manifest line
# `tree libreoffice host:/var/tmp/claude-v2/stage/libreoffice`), from CachyOS's libreoffice-fresh: the package file
# of the installed version in pacman's cache, checked against its signature with pacman's keyring (as the Mesa
# stage does); nothing is downloaded, nothing installed, no root needed. Run it again after an update (pacman -Syu).
#   program/, share/, presets/, LICENSE, ...   LibreOffice's /usr/lib/libreoffice as packaged, with:
#       program/bootstraprc, sofficerc, share/psprint/psprint.conf   the package's /etc/libreoffice files they link to
#                      (copied: FAT has no links, and without them LibreOffice stops with "The configuration file
#                      ... was not found"; headless it hangs on that error box)
#       share/registry/bitsos.xcd   files/bitsos.xcd: no tip-of-the-day box at the first start
#     left out: help/ (62 MB, 8000 files: F1 then offers the online help), the sdk link, readmes/; the VCL plugins
#     for Qt 6, KDE 6, GTK 4 and X11 (gen) and the media players for GStreamer, GTK 4 and Qt 6 (only gtk3 runs, on
#     bitscomp's Wayland; their libraries would fill lib/); Python (pyuno, the Python script provider, LibreLogo,
#     the .py helpers and program/wizards/, the Python document wizards: they need a whole Python); Java (classes/, the Java UNO bridge and loaders, the Java-only
#     database drivers and bundled extensions, share/Scripts: LibreOffice runs without a JRE and says so where a
#     part needs one); the components whose libraries CachyOS does not install (libmysqlclo: libmariadb,
#     libpostgresql-sdbc: libpq, libsolverlo: CoinMP (the swarm solvers stay), libwpftwriterlo/libwpftdrawlo:
#     libwpg, i.e. the WordPerfect, Works, AbiWord, ... import filters; libofficebean: libjawt); every icon theme
#     but colibre and colibre_dark (LibreOffice's own choice without a desktop environment; 2 of 22 zips); the
#     autocorrect lists of languages other than de and en; gdbtrace, gengal, opencltest (tools)
#   lib/               libsoftokn3.so, libfreeblpriv3.so: NSS loads them itself (dlopen) for LibreOffice's hashes and
#                      signatures. xtask adds every library the programs and program/*.so link (ldd, LO-1: the
#                      libraries with RUNPATH $ORIGIN) beyond the initramfs set.
#   fonts/             Liberation (metric twins of Arial, Times New Roman, Courier New), DejaVu, Noto Sans/Serif/Mono
#   etc/fonts/         fonts.conf (files/fonts.conf: V2 paths) and the conf.d rules for them (30-metric-aliases maps
#                      Arial & co. to Liberation); fontconfig keeps its cache in the user's ~/.cache (data volume)
#   gtk/               what GTK 3 needs besides its libraries, as in Brave's tree (BR-7): share/icons Adwaita (every
#                      SVG drawn to PNG: no SVG loader) and hicolor (with LibreOffice's application icons),
#                      share/mime (shared-mime-info), share/glib-2.0/schemas (GTK's GSettings), the glycin loader
#                      glycin/glycin-image-rs with share/glycin-loaders/2+/conf.d (Exec= into the tree, found through
#                      XDG_DATA_DIRS), etc/gdk-pixbuf/loaders.cache, libexec/bwrap (Brave's stand-in, last on PATH)
#   LICENSES/          LibreOffice's LICENSE and NOTICE (in the tree as packaged too), the licence texts of every
#                      other package copied here, README.txt
#   VERSION.txt        the package versions this tree was made from
# A German UI needs CachyOS's libreoffice-fresh-de (pacman -S libreoffice-fresh-de): when it is installed here, its
# package (checked the same way) goes into the tree too; without it the UI is English and documents use the
# launcher's LANG (de_DE: German dates, numbers, paper size). Spell checking likewise needs hunspell-de (and
# hunspell-en_us, ...): their dictionaries go to share/hunspell/ with share/registry/bitsos-dicts.xcd.
# The stage is replaced as a whole (new dir, rename).
set -euo pipefail
PKG=libreoffice-fresh
DEST=${LO_STAGE:-/var/tmp/claude-v2/stage/libreoffice}
CACHE=${LO_PKG_CACHE:-/var/cache/pacman/pkg}
KEYRING=${LO_KEYRING:-/etc/pacman.d/gnupg/pubring.kbx}
V2=/boot/BITSOS2/libreoffice
HERE=$(cd "$(dirname "$0")/.." && pwd)
BRAVE_FILES=$HERE/../brave-launcher/files
die() { echo "stage-v2: $*" >&2; exit 1; }
for t in pacman bsdtar gpgv readelf ldd glib-compile-schemas update-mime-database gtk-encode-symbolic-svg rsvg-convert \
    gtk-update-icon-cache; do command -v $t >/dev/null || die "no $t on this host"; done
[ -r "$KEYRING" ] || die "pacman's keyring $KEYRING cannot be read"
[ -f "$BRAVE_FILES/bwrap" ] || die "$BRAVE_FILES/bwrap is missing"

NEW=$DEST.new X=$DEST.x TMPD=$DEST.tmp
rm -rf "$NEW" "$X" "$TMPD" "$DEST.old"
mkdir -p "$NEW" "$X" "$TMPD"
trap 'rm -rf "$NEW" "$X" "$TMPD"' EXIT

# pacman prints complaints about unrelated repositories on stderr; only stdout is read.
q() { LC_ALL=C pacman "$@" 2>/dev/null; }
# The cached package file of the installed version of $1, checked by its signature. Never downloaded.
pkg_file() {
  local ver arch f
  ver=$(q -Q "$1" | cut -d' ' -f2) || die "package $1 is not installed here (pacman -S $1)"
  arch=$(q -Qi "$1" | sed -n 's/^Architecture *: //p')
  f=$CACHE/$1-$ver-$arch.pkg.tar.zst
  [ -f "$f" ] && [ -f "$f.sig" ] || die "$f or its .sig is not in pacman's cache (pacman -Sw $1)"
  gpgv --keyring "$KEYRING" "$f.sig" "$f" >/dev/null 2>&1 || die "$f: its signature does not check out"
  echo "$f"
}
# Unpacks package $1 into $X (its /usr/lib/libreoffice, /etc/libreoffice and the application icons).
unpack() {
  local f
  f=$(pkg_file "$1")
  bsdtar -xf "$f" -C "$X" --include 'usr/lib/libreoffice/*' --include 'etc/libreoffice/*' \
    --include 'usr/share/icons/hicolor/*/apps/*'
  echo "$1 $(q -Q "$1" | cut -d' ' -f2) ${f##*/}" >>"$TMPD/versions"
}
unpack $PKG
LANGPACK=
if q -Q $PKG-de >/dev/null; then unpack $PKG-de; LANGPACK=de; fi
L=$X/usr/lib/libreoffice
[ -x "$L/program/soffice.bin" ] && [ -f "$L/program/types.rdb" ] || die "$PKG has no program/soffice.bin"

# /etc/libreoffice: the only links pointing out of the tree (program/bootstraprc, sofficerc, share/psprint/psprint.conf).
while IFS= read -r l; do
  t=$(readlink "$l")
  case $t in
    /etc/libreoffice/*) [ -f "$X$t" ] || die "$l links to $t, which $PKG lacks"; rm "$l"; cp "$X$t" "$l" ;;
    /*) die "$l links out of the tree to $t" ;;
  esac
done < <(find "$L" -type l)

# What stays out (see the head of this file).
P=$L/program
rm -rf "$L/help" "$L/sdk" "$L/readmes" "$P/classes" "$P/wizards" "$L/share/Scripts" "$L/share/extensions/nlpsolver" \
  "$L/share/extensions/wiki-publisher"
for f in libvclplug_qt6lo.so libvclplug_kf6lo.so libvclplug_gtk4lo.so libvclplug_genlo.so libavmediagst.so \
    libavmediagtk.so libavmediaqt6.so libpyuno.so pyuno.so libpythonloaderlo.so pythonloader.unorc \
    libjava_uno.so libjpipe.so libjuh.so libjuhx.so libjavaloaderlo.so libjavavmlo.so libhsqldb.so libjdbclo.so \
    libmysql_jdbclo.so libofficebean.so JREProperties.class javaldx java-set-classpath javavendors.xml jvmfwk3rc \
    libmysqlclo.so libpostgresql-sdbclo.so libpostgresql-sdbc-impllo.so libsolverlo.so libwpftwriterlo.so \
    libwpftdrawlo.so gdbtrace gengal gengal.bin opencltest; do
  rm -rf "${P:?}/$f"
done
rm -f "$P"/*.py "$P"/*.pyi "$L/share/registry/librelogo.xcd" "$L/share/registry/postgresql.xcd"
for z in "$L"/share/config/images_*.zip; do
  case ${z##*/} in images_colibre.zip|images_colibre_dark.zip) ;; *) rm "$z" ;; esac
done
for a in "$L"/share/autocorr/acor_*.dat; do
  case ${a##*/} in acor_de*|acor_en*) ;; *) rm "$a" ;; esac
done
# Every program and library left must find its libraries here (program/ through RUNPATH $ORIGIN, the rest on the
# host, which xtask copies into lib/): a component whose library CachyOS lacks would fail only when it is loaded.
# program/ on the path as well: Firebird's program/intl/libfbintl.so links libfbclient.so.2 of program/, loaded first.
miss=$(find "$L" -type f \( -name '*.so' -o -name '*.so.*' -o -name '*.bin' -o -perm -u+x \) -print0 \
  | LD_LIBRARY_PATH=$L/program xargs -0 -n 32 sh -c 'for f; do head -c 4 "$f" | grep -q ELF || continue; ldd "$f" 2>&1 | sed -n "s|^\(.*\) => not found|$f: \1|p"; done' _ \
  | sed "s|$L/||")
[ -z "$miss" ] || die "libraries this host lacks (leave the component out or install the library): $miss"
[ -z "$(find "$L" -type l)" ] || die "links left in the tree: $(find "$L" -type l | head -3)"
cp -r --reflink=auto "$L"/. "$NEW/"
cp "$HERE/files/bitsos.xcd" "$NEW/share/registry/bitsos.xcd"

PKGS="$PKG"
[ -z "$LANGPACK" ] || PKGS="$PKGS $PKG-de"

# Spell checking: the de and en hunspell dictionaries of CachyOS's hunspell-de / hunspell-en_* packages, when they are
# installed (their signed packages, checked as above), in share/hunspell/, announced to LibreOffice by
# share/registry/bitsos-dicts.xcd (CachyOS's LibreOffice looks only in /usr/share/hunspell otherwise, which V2 does
# not have). LO_DICT_DIR (host/tests/lo/05-stage.sh) takes the .aff/.dic pairs of a folder instead of the packages.
D=$NEW/share/hunspell
mkdir -p "$D" "$X/dicts"
if [ -n "${LO_DICT_DIR:-}" ]; then
  cp "$LO_DICT_DIR"/*.aff "$LO_DICT_DIR"/*.dic "$X/dicts/" || die "LO_DICT_DIR $LO_DICT_DIR holds no .aff/.dic"
else
  for p in $(q -Qq | grep -E '^hunspell-(de|en(_[a-z]+)?)$' || true); do
    f=$(pkg_file "$p")
    mkdir -p "$X/dicts.$p"
    bsdtar -xf "$f" -C "$X/dicts.$p" --include 'usr/share/hunspell/*'
    # Real files only: the packages name some dictionaries twice through links (de_DE.dic -> de_DE_frami.dic).
    find "$X/dicts.$p/usr/share/hunspell" -maxdepth 1 -type f \( -name '*.aff' -o -name '*.dic' \) -exec cp {} "$X/dicts/" \;
    echo "$p $(q -Q "$p" | cut -d' ' -f2) ${f##*/}" >>"$TMPD/versions"
    PKGS="$PKGS $p"
  done
fi
{
  echo '<?xml version="1.0" encoding="UTF-8"?>'
  echo '<!-- LO-1: the spell checking dictionaries in share/hunspell/, made by stage-v2.sh. -->'
  echo '<oor:data xmlns:xs="http://www.w3.org/2001/XMLSchema" xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance" xmlns:oor="http://openoffice.org/2001/registry">'
  echo '  <dependency file="main"/>'
  echo '  <oor:component-data oor:name="Linguistic" oor:package="org.openoffice.Office">'
  echo '    <node oor:name="ServiceManager">'
  echo '      <node oor:name="Dictionaries">'
} >"$TMPD/dicts.xcd"
DICTS=
for dic in "$X"/dicts/*.dic; do
  [ -f "$dic" ] || continue
  n=${dic##*/}; n=${n%.dic}
  case $n in de_[A-Z][A-Z]*|en_[A-Z][A-Z]*) ;; *) continue ;; esac
  [ -f "$X/dicts/$n.aff" ] || die "dictionary $n.dic has no $n.aff"
  loc=$(echo "$n" | cut -d_ -f1-2 | tr _ -)
  cp "$dic" "$X/dicts/$n.aff" "$D/"
  cat >>"$TMPD/dicts.xcd" <<XML
        <node oor:name="HunSpellDic_$n" oor:op="fuse">
          <prop oor:name="Locations" oor:type="oor:string-list"><value>vnd.sun.star.expand:\$BRAND_BASE_DIR/share/hunspell/$n.aff vnd.sun.star.expand:\$BRAND_BASE_DIR/share/hunspell/$n.dic</value></prop>
          <prop oor:name="Format" oor:type="xs:string"><value>DICT_SPELL</value></prop>
          <prop oor:name="Locales" oor:type="oor:string-list"><value>$loc</value></prop>
        </node>
XML
  DICTS="$DICTS $loc"
done
printf '      </node>\n    </node>\n  </oor:component-data>\n</oor:data>\n' >>"$TMPD/dicts.xcd"
if [ -n "$DICTS" ]; then cp "$TMPD/dicts.xcd" "$NEW/share/registry/bitsos-dicts.xcd"; else rmdir "$D"; fi
mkdir "$NEW/lib"
for so in libsoftokn3.so libfreeblpriv3.so; do
  cp -L "/usr/lib/$so" "$NEW/lib/$so"
  PKGS="$PKGS $(q -Qoq "/usr/lib/$so")"
done

mkdir -p "$NEW/fonts" "$NEW/etc/fonts/conf.d"
fonts="liberation/LiberationSans-Regular.ttf liberation/LiberationSans-Bold.ttf liberation/LiberationSans-Italic.ttf
liberation/LiberationSans-BoldItalic.ttf liberation/LiberationSerif-Regular.ttf liberation/LiberationSerif-Bold.ttf
liberation/LiberationSerif-Italic.ttf liberation/LiberationSerif-BoldItalic.ttf liberation/LiberationMono-Regular.ttf
liberation/LiberationMono-Bold.ttf liberation/LiberationMono-Italic.ttf liberation/LiberationMono-BoldItalic.ttf
noto/NotoSans-Regular.ttf noto/NotoSans-Bold.ttf noto/NotoSans-Italic.ttf noto/NotoSans-BoldItalic.ttf
noto/NotoSerif-Regular.ttf noto/NotoSerif-Bold.ttf noto/NotoSerif-Italic.ttf noto/NotoSerif-BoldItalic.ttf
noto/NotoSansMono-Regular.ttf noto/NotoSansMono-Bold.ttf TTF/DejaVuSans.ttf TTF/DejaVuSans-Bold.ttf
TTF/DejaVuSans-Oblique.ttf TTF/DejaVuSans-BoldOblique.ttf TTF/DejaVuSerif.ttf TTF/DejaVuSerif-Bold.ttf
TTF/DejaVuSerif-Italic.ttf TTF/DejaVuSerif-BoldItalic.ttf TTF/DejaVuSansMono.ttf TTF/DejaVuSansMono-Bold.ttf
TTF/DejaVuSansMono-Oblique.ttf TTF/DejaVuSansMono-BoldOblique.ttf"
for f in $fonts; do
  [ -f "/usr/share/fonts/$f" ] || die "/usr/share/fonts/$f is missing"
  cp "/usr/share/fonts/$f" "$NEW/fonts/"
  PKGS="$PKGS $(q -Qoq "/usr/share/fonts/$f")"
done
sed "s|@TREE@|$V2|g" "$HERE/files/fonts.conf" >"$NEW/etc/fonts/fonts.conf"
confs="10-hinting-slight 10-scale-bitmap-fonts 10-yes-antialias 11-lcdfilter-default 20-unhint-small-dejavu-sans
20-unhint-small-dejavu-sans-mono 20-unhint-small-dejavu-serif 30-metric-aliases 40-nonlatin 45-generic 45-latin
46-noto-sans 46-noto-serif 48-guessfamily 48-spacing 49-sansserif 50-user 57-dejavu-sans 57-dejavu-sans-mono
57-dejavu-serif 60-generic 60-latin 65-nonlatin 66-noto-sans 66-noto-serif 90-synthetic"
for c in $confs; do
  f=/usr/share/fontconfig/conf.default/$c.conf
  [ -f "$f" ] || die "$f is missing"
  cp "$f" "$NEW/etc/fonts/conf.d/"
  PKGS="$PKGS $(q -Qoq "$f")"
done

# GTK 3's runtime data, as Brave's stage makes it (user/ports/brave-launcher/scripts/stage-v2.sh, BR-7).
G=$NEW/gtk
A=$G/share/icons/Adwaita
mkdir -p "$G/share/icons/hicolor"
cp -rL /usr/share/icons/Adwaita "$G/share/icons/"
for f in index.theme cursors/default symbolic/places/folder-symbolic.svg scalable/status/image-missing.svg \
    16x16/places/folder.png; do
  [ -e "$A/$f" ] || die "/usr/share/icons/Adwaita/$f is missing (pacman -S adwaita-icon-theme)"
done
# shellcheck disable=SC2016
find "$A/symbolic" -name '*.svg' -print0 \
  | xargs -0 -P "$(nproc)" -n 16 sh -c 'for f; do gtk-encode-symbolic-svg -o "${f%/*}" "$f" 16x16 || exit 1; done' _ \
  || die "gtk-encode-symbolic-svg failed"
# shellcheck disable=SC2016
find "$A/scalable" -name '*.svg' -print0 \
  | xargs -0 -P "$(nproc)" -n 16 sh -c 'for f; do rsvg-convert -w 64 -h 64 -o "${f%.svg}.png" "$f" || exit 1; done' _ \
  || die "rsvg-convert failed"
[ "$(find "$A/symbolic" -name '*.symbolic.png' | wc -l)" = "$(find "$A/symbolic" -name '*.svg' | wc -l)" ] \
  || die "not every symbolic icon was encoded"
find "$A" -name '*.svg' -delete
rm -f "$A/icon-theme.cache"
gtk-update-icon-cache -q -f "$A" || die "gtk-update-icon-cache $A failed"
# hicolor: the fallback theme's index and LibreOffice's application icons (PNG sizes only; no SVG loader).
H=$G/share/icons/hicolor
cp /usr/share/icons/hicolor/index.theme "$H/"
for d in "$X"/usr/share/icons/hicolor/*x*/apps; do
  s=${d%/apps}; s=${s##*/}
  mkdir -p "$H/$s/apps"
  cp "$d"/libreoffice-*.png "$H/$s/apps/"
done
[ -f "$H/48x48/apps/libreoffice-writer.png" ] && [ -f "$H/48x48/apps/libreoffice-calc.png" ] \
  || die "$PKG has no 48x48 writer/calc icons"
gtk-update-icon-cache -q -f "$H" || die "gtk-update-icon-cache $H failed"
PKGS="$PKGS $(q -Qoq /usr/share/icons/Adwaita/index.theme) $(q -Qoq /usr/share/icons/hicolor/index.theme)"

M=$TMPD/mime
mkdir -p "$M/packages"
cp /usr/share/mime/packages/freedesktop.org.xml "$M/packages/"
PKGS="$PKGS $(q -Qoq /usr/share/mime/packages/freedesktop.org.xml)"
update-mime-database "$M" >/dev/null 2>&1 || die "update-mime-database $M failed"
rm -rf "$M/packages"
[ -s "$M/mime.cache" ] && [ -f "$M/image/png.xml" ] || die "update-mime-database made no mime.cache"
cp -r "$M" "$G/share/mime"

GL=/usr/lib/glycin-loaders/2+ GC=/usr/share/glycin-loaders/2+/conf.d l=glycin-image-rs
mkdir -p "$G/glycin" "$G/share/glycin-loaders/2+/conf.d" "$G/etc/gdk-pixbuf" "$G/libexec"
cp "$BRAVE_FILES/bwrap" "$G/libexec/bwrap"
[ -x "$GL/$l" ] && [ -f "$GC/$l.conf" ] || die "$GL/$l or $GC/$l.conf is missing (pacman -S glycin)"
cp "$GL/$l" "$G/glycin/"
sed "s|^Exec=$GL/|Exec=$V2/gtk/glycin/|" "$GC/$l.conf" >"$G/share/glycin-loaders/2+/conf.d/$l.conf"
! grep '^Exec=' "$G/share/glycin-loaders/2+/conf.d/$l.conf" | grep -qv "^Exec=$V2/gtk/glycin/$l\$" \
  || die "$GC/$l.conf names a program outside $GL/$l"
grep -q '^\[loader:image/png\]' "$G/share/glycin-loaders/2+/conf.d/$l.conf" || die "$l loads no PNG"
PKGS="$PKGS $(q -Qoq "$GL/$l")"
cat >"$G/etc/gdk-pixbuf/loaders.cache" <<TXT
# GdkPixbuf Image Loader Modules file for BitsOS V2 (GDK_PIXBUF_MODULE_FILE, made by stage-v2.sh).
# No module here: gdk-pixbuf $(q -Q gdk-pixbuf2 | cut -d' ' -f2) has its formats built in (glycin), whose loader
# program is $V2/gtk/glycin/$l, named in $V2/gtk/share/glycin-loaders/2+/conf.d/.
TXT

S=$TMPD/schemas
mkdir "$S"
cp /usr/share/glib-2.0/schemas/org.gtk.Settings.*.gschema.xml "$S/"
PKGS="$PKGS $(q -Qoq /usr/share/glib-2.0/schemas/org.gtk.Settings.FileChooser.gschema.xml)"
mkdir -p "$G/share/glib-2.0/schemas"
glib-compile-schemas --strict --targetdir="$G/share/glib-2.0/schemas" "$S"

# Licences: LibreOffice's own, then per package its /usr/share/licenses/<pkg>/ files and the SPDX texts pacman names.
mkdir "$NEW/LICENSES"
cp "$NEW/LICENSE" "$NEW/LICENSES/libreoffice.txt"
cp "$NEW/NOTICE" "$NEW/LICENSES/libreoffice-NOTICE.txt"
for p in $(echo "$PKGS" | tr ' ' '\n' | sort -u); do
  case $p in $PKG|$PKG-de) continue ;; esac
  mkdir "$NEW/LICENSES/$p"
  [ ! -d "/usr/share/licenses/$p" ] || cp -L /usr/share/licenses/"$p"/* "$NEW/LICENSES/$p/"
  for id in $(q -Qi "$p" | sed -n 's/^Licenses *: //p' | tr '()' '  '); do
    case $id in AND|OR|WITH|custom|custom:*) continue ;; esac
    [ ! -f "/usr/share/licenses/spdx/$id.txt" ] || cp "/usr/share/licenses/spdx/$id.txt" "$NEW/LICENSES/$p/$id.txt"
  done
  [ -n "$(ls "$NEW/LICENSES/$p")" ] || die "no licence text for package $p"
done
cp "$TMPD/versions" "$NEW/VERSION.txt"
cat >"$NEW/LICENSES/README.txt" <<TXT
LibreOffice $(q -Q $PKG | cut -d' ' -f2) (pacman $PKG from CachyOS${LANGPACK:+, with $PKG-de}), packaged for BitsOS V2
by user/ports/libreoffice-launcher/scripts/stage-v2.sh of the BitsOS V2 tree; started by \\BITSOS2\\bin\\libreoffice.
  libreoffice.txt, libreoffice-NOTICE.txt   LibreOffice's LICENSE (MPL-2.0 and the third-party parts) and NOTICE
  <package>/  the licence texts of the CachyOS package each other file comes from (fonts, fontconfig rules, the icon
              themes, the MIME database, GTK's schemas, the glycin image loader, NSS). The libraries xtask adds to
              lib/ have their texts in /usr/share/licenses of V2.
TXT

if [ -e "$DEST" ]; then mv "$DEST" "$DEST.old"; fi
mv "$NEW" "$DEST"
rm -rf "$DEST.old" "$X" "$TMPD"
trap - EXIT
echo "stage-v2: $DEST: $(find "$DEST" -type f | wc -l) files, $(du -sm "$DEST" | cut -f1) MB${LANGPACK:+, German UI}${DICTS:+, dictionaries$DICTS}"
