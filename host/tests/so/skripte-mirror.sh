#!/bin/bash
# helper
# EINS-D: the fixture mirror of the set "skripte"'s Python (so/40-skripte-qemu.sh): python's closure as pacman
# resolves it on the build host (17.11.2026; zlib is zlib-ng-compat here, CachyOS's provider), each package the
# newest of its name in the build host's pacman cache with its real .sig (read only), and fixture.db by repo-add.
# OUT/fixture/ is served by a plain HTTP server; never in git (third-party binaries). A missing package is RED.
set -eu -o pipefail
OUT="${1:?usage: skripte-mirror.sh OUT}"
CACHE=/var/cache/pacman/pkg
[ "$(id -u)" != 0 ] || { echo "RED: skripte-mirror.sh runs as a user, not as root"; exit 1; }
for t in repo-add vercmp bsdtar; do command -v $t >/dev/null || { echo "RED: $t missing on this host (install pacman)"; exit 1; }; done
PKGS=(
    linux-api-headers tzdata iana-etc filesystem glibc libgcc libstdc++ ncurses readline bash bzip2 expat gdbm libffi
    zlib-ng zlib-ng-compat sqlite util-linux-libs e2fsprogs keyutils brotli xz lz4 zstd openssl libsasl libldap libevent
    libverto lmdb krb5 libtirpc libnsl libxcrypt openssl-3 mpdecimal python
)
rm -rf "$OUT"
mkdir -p "$OUT/fixture"
pick() {
    local want="$1" best="" bestv="" f base s v
    for f in "$CACHE/$want"-*.pkg.tar.zst; do
        [ -f "$f" ] && [ -f "$f.sig" ] || continue
        base="${f##*/}"; s="${base%.pkg.tar.zst}"; s="${s%-*}"
        v="${s##*-}"; s="${s%-*}"; v="${s##*-}-$v"; s="${s%-*}"
        [ "$s" = "$want" ] || continue
        if [ -z "$best" ] || [ "$(vercmp "$v" "$bestv")" -gt 0 ]; then best="$f"; bestv="$v"; fi
    done
    echo "$best"
}
for n in "${PKGS[@]}"; do
    f="$(pick "$n")"
    [ -n "$f" ] || { echo "RED: $n is not in $CACHE (with its .sig)"; exit 1; }
    cp "$f" "$f.sig" "$OUT/fixture/"
done
(cd "$OUT/fixture" && repo-add -q fixture.db.tar.gz ./*.pkg.tar.zst > /dev/null)
rm -f "$OUT/fixture/fixture.db" "$OUT/fixture/fixture.files"
cp "$OUT/fixture/fixture.db.tar.gz" "$OUT/fixture/fixture.db"
echo "skripte mirror in $OUT: ${#PKGS[@]} packages, $(du -sm "$OUT/fixture" | cut -f1) MB"
