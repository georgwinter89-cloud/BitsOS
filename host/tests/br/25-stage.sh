#!/usr/bin/env bash
# BR-7: the staged Brave tree (user/ports/brave-launcher/scripts/stage-v2.sh) has what GTK's dialogs need, so a
# regression of the stage or a pacman -Syu that left it behind turns this red before a Brave download aborts again
# (Georg 24.10.): the MIME database, the empty loaders.cache, glycin's loader with its config pointing into the tree
# and matching this host's glycin, the bwrap stand-in, icons without SVG and an index.theme listing every shipped
# folder. Host only, a few seconds, no QEMU. Without a stage it says so and passes (nothing to check).
set -uo pipefail
S=${BRAVE_STAGE:-/var/tmp/claude-v2/stage/brave}
V2=/boot/BITSOS2/brave
L=glycin-image-rs
[ -d "$S" ] || { echo "skip: no stage at $S (bash user/ports/brave-launcher/scripts/stage-v2.sh makes it)"; exit 0; }
fails=0
check() { if eval "$2"; then echo "ok   $1"; else echo "FAIL $1"; fails=$((fails + 1)); fi; }
CONF=$S/share/glycin-loaders/2+/conf.d/$L.conf

check "MIME database (mime.cache, image/png)" '[ -s "$S/share/mime/mime.cache" ] && [ -f "$S/share/mime/image/png.xml" ]'
check "loaders.cache for GDK_PIXBUF_MODULE_FILE" '[ -f "$S/etc/gdk-pixbuf/loaders.cache" ]'
check "bwrap stand-in (executable)" '[ -x "$S/libexec/bwrap" ]'
check "glycin loader and its config" '[ -x "$S/glycin/$L" ] && [ -f "$CONF" ] && grep -q "^\[loader:image/png\]" "$CONF"'
check "every Exec= of the config names $V2/glycin/$L" 'grep -q "^Exec=" "$CONF" && ! grep "^Exec=" "$CONF" | grep -qvx "Exec=$V2/glycin/$L"'
check "the loader is this host's (libglycin in the image comes from the host: same glycin version)" \
  'cmp -s "$S/glycin/$L" "/usr/lib/glycin-loaders/2+/$L"'
check "the loader finds all its libraries" 'out=$(ldd "$S/glycin/$L" 2>&1) && ! grep -q "not found" <<<"$out"'
check "no SVG under share/icons (GTK has no SVG loader in the tree)" '[ -z "$(find "$S/share/icons" -name "*.svg" -print -quit)" ]'
check "Adwaita: index.theme and icon-theme.cache" '[ -f "$S/share/icons/Adwaita/index.theme" ] && [ -s "$S/share/icons/Adwaita/icon-theme.cache" ] && [ -f "$S/share/icons/hicolor/index.theme" ]'
# The other way round from Adwaita's own index.theme, which lists folders no size ships (GTK skips those): a shipped
# icon folder that index.theme does not list is invisible to GTK.
dirs=$(sed -n 's/^\(Scaled\)\{0,1\}Directories=//p' "$S/share/icons/Adwaita/index.theme" | tr ',' '\n')
unlisted=$(cd "$S/share/icons/Adwaita" && find . -mindepth 2 -maxdepth 2 -type d ! -path './cursors*' | sed 's|^\./||' | sort |
  while read -r d; do grep -qx "$d" <<<"$dirs" || echo "$d"; done)
check "Adwaita: every shipped icon folder is in index.theme${unlisted:+ (not listed: $(echo $unlisted | cut -c1-200))}" '[ -z "$unlisted" ]'
[ $fails = 0 ] || { echo "stage $S: $fails check(s) failed: rebuild it with stage-v2.sh"; exit 1; }
echo "stage $S: all ok"
