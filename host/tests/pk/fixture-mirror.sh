#!/bin/bash
# PK-6a: builds the fixture mirror in OUT (a folder under the worktree's target/, never in git: its packages are
# third-party binaries) from the build host's pacman cache, read-only. Layout, served by a plain HTTP server:
#   OUT/main/fixture/  the packages of the test closures with their real .sig files, and fixture.db, an Arch-style
#                      unsigned database made by repo-add (its %PGPSIG% fields come from the .sig files)
#   OUT/main/bad/      bad.db = the same database, bad.db.sig = jq's package signature: a real OpenPGP signature
#                      that does not sign this database (deliberately broken)
#   OUT/main/nosig/    nosig.db without a .sig (for a source whose database signature is required)
#   OUT/old/fixture/   fixture.db with only the oldest package: an older database a second mirror path serves
#   OUT/main/flip/     PK-6d: flip.db = fixture.db, and jq's package with one byte flipped (its sha256 no longer
#                      matches %SHA256SUM%) beside its real .sig
#   OUT/main/badpkg/   PK-6d: jq's real package with oniguruma's .sig, badpkg.db by repo-add --include-sigs (so %PGPSIG% is the
#                      wrong signature too): the sha256 matches, the signature does not
#   OUT/main/unsigned/ PK-6d: jq's real package without any .sig, unsigned.db by repo-add (no %PGPSIG% either)
# The closures are those of jq and foot as pacman resolves them on the build host (oniguruma comes with jq):
# `pacman -Sp jq foot` against a copy of its sync databases, 6.11. zlib is zlib-ng-compat here (CachyOS's provider
# of zlib); a package missing from the cache is RED. Runs as a user (never root), writes only below OUT.
set -eu -o pipefail
OUT="${1:?usage: fixture-mirror.sh OUT}"
CACHE=/var/cache/pacman/pkg
[ "$(id -u)" != 0 ] || { echo "RED: fixture-mirror.sh runs as a user, not as root"; exit 1; }
for t in repo-add vercmp bsdtar; do command -v $t >/dev/null || { echo "RED: $t missing on this host"; exit 1; }; done
PKGS=(
    # jq
    filesystem iana-etc tzdata linux-api-headers glibc oniguruma jq
    # foot (beyond glibc)
    libgcc libstdc++ ncurses readline bash expat brotli bzip2 zlib-ng zlib-ng-compat libpng freetype2 fontconfig
    libffi libsysprof-capture pcre2 sqlite util-linux-libs glib2 graphite harfbuzz libutf8proc pixman fcft icu
    libxml2 xkeyboard-config libxkbcommon default-cursors wayland foot
)
rm -rf "$OUT"
mkdir -p "$OUT/main/fixture" "$OUT/main/bad" "$OUT/main/nosig" "$OUT/old/fixture" "$OUT/main/flip" "$OUT/main/badpkg" "$OUT/main/unsigned"

# The newest cached file of exactly this name that has its .sig.
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

oldest="" oldest_date=""
for n in "${PKGS[@]}"; do
    f="$(pick "$n")"
    [ -n "$f" ] || { echo "RED: $n is not in $CACHE (with its .sig)"; exit 1; }
    cp "$f" "$f.sig" "$OUT/main/fixture/"
    d="$(bsdtar -xOf "$f" .PKGINFO | sed -n 's/^builddate = //p')"
    if [ -z "$oldest" ] || [ "$d" -lt "$oldest_date" ]; then oldest="${f##*/}"; oldest_date="$d"; fi
done
(cd "$OUT/main/fixture" && repo-add -q fixture.db.tar.gz ./*.pkg.tar.zst > /dev/null)
rm -f "$OUT/main/fixture/fixture.db" "$OUT/main/fixture/fixture.files"
cp "$OUT/main/fixture/fixture.db.tar.gz" "$OUT/main/fixture/fixture.db"
cp "$OUT/main/fixture/fixture.db" "$OUT/main/bad/bad.db"
cp "$OUT/main/fixture/$(cd "$OUT/main/fixture" && ls jq-*.pkg.tar.zst.sig | head -1)" "$OUT/main/bad/bad.db.sig"
cp "$OUT/main/fixture/fixture.db" "$OUT/main/nosig/nosig.db"
cp "$OUT/main/fixture/$oldest" "$OUT/main/fixture/$oldest.sig" "$OUT/old/fixture/"
(cd "$OUT/old/fixture" && repo-add -q fixture.db.tar.gz ./*.pkg.tar.zst > /dev/null)
rm -f "$OUT/old/fixture/fixture.db"
cp "$OUT/old/fixture/fixture.db.tar.gz" "$OUT/old/fixture/fixture.db"
jqf="$(cd "$OUT/main/fixture" && ls jq-*.pkg.tar.zst | head -1)"
onigf="$(cd "$OUT/main/fixture" && ls oniguruma-*.pkg.tar.zst | head -1)"
cp "$OUT/main/fixture/fixture.db" "$OUT/main/flip/flip.db"
cp "$OUT/main/fixture/$jqf" "$OUT/main/fixture/$jqf.sig" "$OUT/main/flip/"
size=$(stat -c %s "$OUT/main/flip/$jqf")
python3 -I -c 'import sys; p, at = sys.argv[1], int(sys.argv[2]); b = bytearray(open(p, "rb").read()); b[at] ^= 0x40; open(p, "wb").write(b)' "$OUT/main/flip/$jqf" $((size / 2))
cp "$OUT/main/fixture/$jqf" "$OUT/main/badpkg/"
cp "$OUT/main/fixture/$onigf.sig" "$OUT/main/badpkg/$jqf.sig"
(cd "$OUT/main/badpkg" && repo-add -q --include-sigs badpkg.db.tar.gz ./*.pkg.tar.zst > /dev/null)
rm -f "$OUT/main/badpkg/badpkg.db" "$OUT/main/badpkg/badpkg.files"
cp "$OUT/main/badpkg/badpkg.db.tar.gz" "$OUT/main/badpkg/badpkg.db"
cp "$OUT/main/fixture/$jqf" "$OUT/main/unsigned/"
(cd "$OUT/main/unsigned" && repo-add -q unsigned.db.tar.gz ./*.pkg.tar.zst > /dev/null)
rm -f "$OUT/main/unsigned/unsigned.db" "$OUT/main/unsigned/unsigned.files"
cp "$OUT/main/unsigned/unsigned.db.tar.gz" "$OUT/main/unsigned/unsigned.db"
echo "fixture mirror in $OUT: flip ($jqf with a flipped byte), badpkg ($jqf with $onigf's signature), unsigned ($jqf without one)"
echo "fixture mirror in $OUT: ${#PKGS[@]} packages in fixture, bad (broken .sig), nosig, old ($oldest, built $oldest_date)"
