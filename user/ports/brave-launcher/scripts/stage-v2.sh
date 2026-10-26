#!/usr/bin/env bash
# BR-2: builds the tree that `cargo xtask image` puts into \BITSOS2\brave\ on the stick (manifest line
# `tree brave host:/var/tmp/claude-v2/stage/brave`) from CachyOS's Brave (pacman brave-bin, /opt/brave-bin; read
# only, nothing is downloaded):
#   brave, *.pak, icudtl.dat, v8_context_snapshot.bin, libvulkan/libvk_swiftshader, MEIPreload/, ...
#                      as installed, without: chrome-sandbox (setuid helper; V2 runs Brave with --no-sandbox and FAT
#                      has no setuid), chrome-management-service (enterprise device trust), libqt5_shim.so and
#                      libqt6_shim.so (only with --ui-toolkit=qt or a KDE session; their ldd would pull Qt 5 and 6),
#                      the Debian/AppArmor helpers (apparmor.d/, cron/, default-app-block), the wrapper script
#                      brave-browser (bitsos/brave replaces it), and every UI language but de, en-US, en-GB
#                      (79 -> 3 MB; Chromium falls back to en-US)
#   lib/               the libraries Chromium dlopens itself (BR-1/BR-2 strace): GTK 3 (ui/gtk: file dialogs, theme)
#                      and NSS's soft token + freebl (loaded next to libnss3, by NSS). xtask adds everything they and
#                      the programs link (ldd) beyond the initramfs set. Left out on purpose: libpulse (Chromium then
#                      takes ALSA at once), mesa's dri_gbm/gallium/LLVM (only with a DRM render node; --disable-gpu),
#                      libva, the pipewire ALSA plugin, GTK 4 (only probed with RTLD_NOLOAD)
#   fonts/             Noto Sans/Serif/Mono, Noto Color Emoji, Noto Sans CJK, Arabic, Hebrew, symbols, DejaVu
#   etc/fonts/         fonts.conf (files/fonts.conf: V2 paths) and the package conf.d rules for these fonts
#   fontcache/         fontconfig's cache for /boot/BITSOS2/brave/fonts, made by fc-cache against a staged root,
#                      its directory checksum set to what fontconfig computes on FAT (see fat_checksum below)
#   share/icons/       the Adwaita cursor theme (libwayland-cursor), hicolor's index.theme
#   share/glib-2.0/schemas/  GTK 3's GSettings schemas (GTK aborts without org.gtk.Settings.FileChooser)
#   LICENSES/          Brave's LICENSE and the licence texts of every other package copied here
# The stage is replaced as a whole (new dir, rename). Run it again after a Brave update (pacman -Syu).
set -euo pipefail
SRC=${BRAVE_SRC:-/opt/brave-bin}
DEST=${BRAVE_STAGE:-/var/tmp/claude-v2/stage/brave}
V2=/boot/BITSOS2/brave
HERE=$(cd "$(dirname "$0")/.." && pwd)
die() { echo "stage-v2: $*" >&2; exit 1; }
for f in brave resources.pak icudtl.dat v8_context_snapshot.bin locales/en-US.pak LICENSE; do
  [ -e "$SRC/$f" ] || die "$SRC/$f is missing (pacman -S brave-bin)"
done
for t in fc-cache glib-compile-schemas pacman python3; do command -v $t >/dev/null || die "no $t on this host"; done

NEW=$DEST.new ROOT=$DEST.root
rm -rf "$NEW" "$ROOT" "$DEST.old"
mkdir -p "$NEW"
trap 'rm -rf "$NEW" "$ROOT"' EXIT

skip='^(chrome-sandbox|chrome-management-service|libqt5_shim\.so|libqt6_shim\.so|apparmor\.d|cron|default-app-block|brave-browser|locales)$'
for e in "$SRC"/*; do
  n=${e##*/}
  [[ $n =~ $skip ]] || cp -r --reflink=auto "$e" "$NEW/$n"
done
mkdir "$NEW/locales"
for l in de en-US en-GB; do cp "$SRC/locales/$l.pak" "$NEW/locales/"; done

# Packages whose files are copied (for LICENSES/).
PKGS=$(LC_ALL=C pacman -Qoq "$SRC/brave")

mkdir "$NEW/lib"
for so in libgtk-3.so.0 libsoftokn3.so libfreeblpriv3.so; do
  cp -L "/usr/lib/$so" "$NEW/lib/$so"
  PKGS="$PKGS $(pacman -Qoq "/usr/lib/$so")"
done

mkdir "$NEW/fonts"
fonts="noto/NotoSans-Regular.ttf noto/NotoSans-Bold.ttf noto/NotoSans-Italic.ttf noto/NotoSans-BoldItalic.ttf
noto/NotoSerif-Regular.ttf noto/NotoSerif-Bold.ttf noto/NotoSerif-Italic.ttf noto/NotoSerif-BoldItalic.ttf
noto/NotoSansMono-Regular.ttf noto/NotoSansMono-Bold.ttf noto/NotoColorEmoji.ttf
noto/NotoSansSymbols-Regular.ttf noto/NotoSansSymbols2-Regular.ttf noto/NotoSansArabic-Regular.ttf
noto/NotoSansArabic-Bold.ttf noto/NotoNaskhArabic-Regular.ttf noto/NotoNaskhArabic-Bold.ttf
noto/NotoSansHebrew-Regular.ttf noto/NotoSansHebrew-Bold.ttf noto-cjk/NotoSansCJK-Regular.ttc
noto-cjk/NotoSansCJK-Bold.ttc TTF/DejaVuSans.ttf TTF/DejaVuSans-Bold.ttf TTF/DejaVuSans-Oblique.ttf
TTF/DejaVuSans-BoldOblique.ttf TTF/DejaVuSerif.ttf TTF/DejaVuSerif-Bold.ttf TTF/DejaVuSerif-Italic.ttf
TTF/DejaVuSerif-BoldItalic.ttf TTF/DejaVuSansMono.ttf TTF/DejaVuSansMono-Bold.ttf TTF/DejaVuSansMono-Oblique.ttf
TTF/DejaVuSansMono-BoldOblique.ttf"
for f in $fonts; do
  [ -f "/usr/share/fonts/$f" ] || die "/usr/share/fonts/$f is missing"
  cp "/usr/share/fonts/$f" "$NEW/fonts/"
  PKGS="$PKGS $(pacman -Qoq "/usr/share/fonts/$f")"
done

mkdir -p "$NEW/etc/fonts/conf.d" "$NEW/fontcache"
sed "s|@TREE@|$V2|g" "$HERE/files/fonts.conf" >"$NEW/etc/fonts/fonts.conf"
confs="10-hinting-slight 10-scale-bitmap-fonts 10-yes-antialias 11-lcdfilter-default 20-unhint-small-dejavu-sans
20-unhint-small-dejavu-sans-mono 20-unhint-small-dejavu-serif 30-metric-aliases 40-nonlatin 45-generic 45-latin
46-noto-sans 46-noto-serif 48-guessfamily 48-spacing 49-sansserif 50-user 57-dejavu-sans 57-dejavu-sans-mono
57-dejavu-serif 60-generic 60-latin 65-nonlatin 66-noto-sans 66-noto-serif 90-synthetic"
for c in $confs; do
  f=/usr/share/fontconfig/conf.default/$c.conf
  [ -f "$f" ] || die "$f is missing"
  cp "$f" "$NEW/etc/fonts/conf.d/"
  PKGS="$PKGS $(pacman -Qoq "$f")"
done
# The cache as V2's fontconfig looks it up: fc-cache against a root where the tree sits at its V2 path, so the
# cache file's name (MD5 of the directory), its directory and its font paths are V2's (checked with strings).
mkdir -p "$ROOT${V2%/*}"
ln -s "$NEW" "$ROOT$V2"
FONTCONFIG_FILE=$V2/etc/fonts/fonts.conf fc-cache -y "$ROOT" -s -f >/dev/null
rm -rf "$ROOT"
find "$NEW/fontcache" -type l -delete
caches=$(find "$NEW/fontcache" -name '*.cache-*' -type f)
[ -n "$caches" ] || die "fc-cache wrote no cache into $NEW/fontcache"
grep -q -a "$V2/fonts/NotoSans-Regular.ttf" $caches || die "the cache does not name $V2/fonts (fc-cache -y)"
# On FAT (statfs f_type 0x4d44) fontconfig does not trust a directory's mtime: FcStatChecksum takes an Adler-32
# over the directory's sorted entry names and d_types instead (fontconfig src/fcstat.c FcDirChecksum,
# FcIsFsMtimeBroken), and FcCacheTimeValid compares it with the cache's `checksum` (FcCache: int at offset 48 on
# x86-64). The host computed it from its own file system, so it is set here to what V2 computes for $V2/fonts.
# checksum_nano (offset 56) is 0: fontconfig 2.18 writes 0 there even on ext4 and keeps the cache whatever the dir's
# mtime nanoseconds are (BR-4a, tested with a statfs shim and in ktest ul.pkg.brave_fonts_mtime), so the cache stays
# valid when a cached dir inode on the stick reports an mtime with nanoseconds after a push.
python3 -I - "$NEW/fonts" $caches <<'PY'
import os, struct, sys
d, caches = sys.argv[1], sys.argv[2:]
names = sorted(n for n in os.listdir(d) if os.path.isfile(os.path.join(d, n)))
a, b = 1, 0
for n in reversed(names):  # FcDirChecksum walks the sorted list from its end
    raw = n.encode()
    assert all(c < 0x80 for c in raw), n  # fontconfig adds the bytes as signed char
    for c in raw + b"\0" + struct.pack("<i", 8):  # name with its NUL, then d_type DT_REG as an int
        a = (a + c) % 65521
        b = (b + a) % 65521
for c in caches:
    with open(c, "r+b") as f:
        head = f.read(64)
        assert struct.unpack_from("<I", head, 0)[0] == 0xFC02FC04, c  # FC_CACHE_MAGIC_MMAP
        f.seek(48); f.write(struct.pack("<I", (a + (b << 16)) & 0xFFFFFFFF))
        f.seek(56); f.write(struct.pack("<q", 0))
print(f"stage-v2: fontconfig cache: {len(names)} fonts, FAT checksum {(a + (b << 16)) & 0xFFFFFFFF:#010x}", file=sys.stderr)
PY

mkdir -p "$NEW/share/icons/Adwaita" "$NEW/share/icons/hicolor"
cp -rL /usr/share/icons/Adwaita/cursors "$NEW/share/icons/Adwaita/"
printf '[Icon Theme]\nName=Adwaita\nComment=The Adwaita cursor theme (BitsOS V2: cursors only)\nInherits=hicolor\nDirectories=\n' \
  >"$NEW/share/icons/Adwaita/index.theme"
cp /usr/share/icons/hicolor/index.theme "$NEW/share/icons/hicolor/"
PKGS="$PKGS $(pacman -Qoq /usr/share/icons/Adwaita/cursors/default) $(pacman -Qoq /usr/share/icons/hicolor/index.theme)"

S=$(mktemp -d)
cp /usr/share/glib-2.0/schemas/org.gtk.Settings.*.gschema.xml "$S/"
PKGS="$PKGS $(pacman -Qoq /usr/share/glib-2.0/schemas/org.gtk.Settings.FileChooser.gschema.xml)"
mkdir -p "$NEW/share/glib-2.0/schemas"
glib-compile-schemas --strict --targetdir="$NEW/share/glib-2.0/schemas" "$S"
rm -rf "$S"

# Licences: Brave's own, then per package its /usr/share/licenses/<pkg>/ files and the SPDX texts pacman names.
mkdir "$NEW/LICENSES"
cp "$SRC/LICENSE" "$NEW/LICENSES/brave.txt"
for p in $(echo "$PKGS" | tr ' ' '\n' | sort -u); do
  [ "$p" != brave-bin ] || continue
  mkdir "$NEW/LICENSES/$p"
  [ ! -d "/usr/share/licenses/$p" ] || cp -L /usr/share/licenses/"$p"/* "$NEW/LICENSES/$p/"
  for id in $(LC_ALL=C pacman -Qi "$p" | sed -n 's/^Licenses *: //p' | tr '()' '  '); do
    case $id in AND|OR|WITH|custom|custom:*) continue ;; esac
    [ ! -f "/usr/share/licenses/spdx/$id.txt" ] || cp "/usr/share/licenses/spdx/$id.txt" "$NEW/LICENSES/$p/$id.txt"
  done
  [ -n "$(ls "$NEW/LICENSES/$p")" ] || die "no licence text for package $p"
done
ver=$(LC_ALL=C pacman -Q brave-bin | cut -d' ' -f2)
cat >"$NEW/LICENSES/README.txt" <<TXT
Brave $ver (pacman brave-bin from CachyOS, /opt/brave-bin), packaged for BitsOS V2 by
user/ports/brave-launcher/scripts/stage-v2.sh of the BitsOS V2 tree; started by \\BITSOS2\\bin\\brave.
  brave.txt   Brave's LICENSE (Brave and Chromium; brave://credits lists every third-party part)
  <package>/  the licence texts of the CachyOS package each other file comes from (fonts, GTK 3, NSS, fontconfig
              rules, cursors). The libraries xtask adds to lib/ have their texts in /usr/share/licenses of V2.
TXT

if [ -e "$DEST" ]; then mv "$DEST" "$DEST.old"; fi
mv "$NEW" "$DEST"
rm -rf "$DEST.old"
trap - EXIT
echo "stage-v2: $DEST: $(find "$DEST" -type f | wc -l) files, $(du -sm "$DEST" | cut -f1) MB"
