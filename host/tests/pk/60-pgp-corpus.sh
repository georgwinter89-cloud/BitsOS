#!/bin/bash
# PK-6b: bitspkg's own OpenPGP verifier against gpg on the CachyOS build host (host only, no QEMU, no network).
#  1. The verifier's host tests (packet framing, RFC 9580 vectors, keyring trust, RSA ours = ring, fuzzing).
#  2. Every cached package signature of /var/cache/pacman/pkg with its package: bitspkg's verdict must equal
#     gpg's with a keyring populated exactly like `pacman-key --populate` (scratch GNUPGHOME as this user:
#     import <ring>.gpg, a local master key lsigns the -trusted keys, --import-ownertrust; -revoked keys count
#     as disabled, as pacman refuses them, signing.c:695). gpg's good is GOODSIG + TRUST_FULLY/ULTIMATE, which is
#     what pacman's default SigLevel TrustedOnly accepts. PGP_CORPUS_SAMPLE=N checks every k-th pair only.
#  3. Packages with one flipped byte (a sample of PGP_FLIP=30 packages up to 64 MiB, copied) must fail.
#  4. gpg-made test keys: 3 main keys certify a packager (good) / only 2 (refused), a key not in the keyring,
#     one in -revoked, one with a revocation signature, an expired signing subkey, SHA-1, a critical notation.
# The host's pacman state is only read: /var/cache/pacman/pkg and /usr/share/pacman/keyrings. Never as root.
set -euo pipefail
ROOT="${BITSOS_ROOT:-$(cd "$(dirname "$0")/../../.." && pwd)}"
RUN="${BITSOS_RUN_DIR:-$ROOT/target/xtask/runs/pk-60-pgp-corpus}"
K=/usr/share/pacman/keyrings
C=/var/cache/pacman/pkg
red() { echo "RED: $*"; exit 1; }
[ "$(id -u)" != 0 ] || red "runs as a normal user, never as root"
for f in archlinux.gpg archlinux-trusted archlinux-revoked cachyos.gpg cachyos-trusted; do
    [ -r "$K/$f" ] || red "$K/$f missing: this check runs on the CachyOS build host"
done
command -v gpg >/dev/null && command -v gpgconf >/dev/null || red "gpg missing on the build host"
rm -rf "$RUN" && mkdir -p "$RUN"
# A red exit must not leave a gpg-agent running in a scratch GNUPGHOME.
cleanup() { for h in "$RUN/gnupg" "$RUN/gen"; do [ -d "$h" ] && gpgconf --homedir "$h" --kill gpg-agent 2>/dev/null; done; true; }
trap cleanup EXIT
P=$(( $(nproc) / 2 )); [ "$P" -ge 1 ] || P=1; [ "$P" -le 8 ] || P=8

cd "$ROOT/user"
H=(env -u CARGO_TARGET_DIR -u RUSTUP_TOOLCHAIN cargo)
set +e
{ "${H[@]}" test --offline -p bitspkg --target-dir "$ROOT/target/user-host" --lib -- pgp:: &&
  "${H[@]}" test --offline -p bitspkg --target-dir "$ROOT/target/user-host" --test pgp_trust --test pgp_vectors --test pgp_fuzz; } > "$RUN/test.log" 2>&1
rc=$?
set -e
grep -E "^test |test result|panicked" "$RUN/test.log" | tail -n 30 || true
[ $rc = 0 ] || { tail -n 30 "$RUN/test.log"; red "pgp host tests (log $RUN/test.log)"; }
T="$ROOT/target/user-scenario"
"${H[@]}" build -q --offline --release --target x86_64-unknown-linux-musl --target-dir "$T" -p bitspkg
BIN="$T/x86_64-unknown-linux-musl/release/bitspkg"
file "$BIN" | grep -q "static" || red "$BIN is not static"

# The reference: a pacman keyring populated by hand in a scratch GNUPGHOME (pacman-key.sh.in:283-372).
G="$RUN/gnupg"
mkdir -m700 "$G"
gr() { gpg --homedir "$G" --batch --no-tty --quiet "$@"; }
gr --import "$K/archlinux.gpg" "$K/cachyos.gpg" 2>/dev/null || true
gr --pinentry-mode loopback --passphrase '' --quick-gen-key 'Pacman Keyring Master Key <pacman@localhost>' ed25519 cert never 2>/dev/null
for f in $(cut -d: -f1 "$K/archlinux-trusted" "$K/cachyos-trusted"); do gr --yes --quick-lsign-key "$f" > /dev/null 2>&1; done
cat "$K/archlinux-trusted" "$K/cachyos-trusted" | gr --import-ownertrust 2>/dev/null
gr --check-trustdb 2>/dev/null || true
cat "$K/archlinux-revoked" "$K/cachyos-revoked" 2>/dev/null | tr -d ' ' > "$RUN/revoked.txt"

# The corpus: every .sig with its package file (the download-* leftovers are no pairs).
: > "$RUN/all.tsv"
for s in "$C"/*.sig; do
    f="${s%.sig}"
    [ -f "$f" ] && [ -r "$f" ] && printf '%s\t%s\n' "$f" "$s" >> "$RUN/all.tsv"
done
N=$(wc -l < "$RUN/all.tsv")
[ "$N" -gt 0 ] || red "no package signatures in $C"
if [ -n "${PGP_CORPUS_SAMPLE:-}" ] && [ "$PGP_CORPUS_SAMPLE" -lt "$N" ]; then
    k=$(( N / PGP_CORPUS_SAMPLE ))
    awk -v k="$k" 'NR % k == 1' "$RUN/all.tsv" > "$RUN/list.tsv"
    echo "corpus: SAMPLE of $(wc -l < "$RUN/list.tsv") of $N pairs (PGP_CORPUS_SAMPLE)"
else
    cp "$RUN/all.tsv" "$RUN/list.tsv"
fi
split -n "l/$P" -d "$RUN/list.tsv" "$RUN/chunk."

gpg_chunk() {
    while IFS=$'\t' read -r f s; do
        out=$(gpg --homedir "$G" --batch --no-tty --no-auto-check-trustdb --status-fd 1 --verify "$s" "$f" 2>/dev/null || true)
        prim=$(awk '$2=="VALIDSIG"{print $12}' <<< "$out" | head -n 1)
        v=bad
        if grep -q '^\[GNUPG:\] GOODSIG' <<< "$out" && grep -Eq '^\[GNUPG:\] TRUST_(FULLY|ULTIMATE)' <<< "$out"; then
            grep -qx "$prim" "$RUN/revoked.txt" || v=good
        fi
        why=$(grep -oE '^\[GNUPG:\] (GOODSIG|BADSIG|EXPKEYSIG|REVKEYSIG|ERRSIG|NO_PUBKEY|TRUST_[A-Z]+)' <<< "$out" | cut -c10- | tr '\n' ' ')
        printf '%s\t%s\t%s\t%s\n' "$v" "$f" "$prim" "$why"
    done < "$1"
}
start=$(date +%s)
for c in "$RUN"/chunk.*; do
    nice -n 10 "$BIN" pgp verify-list "$c" --keyring "$K" > "$c.ours" 2> "$c.err" &
done
wait
mid=$(date +%s)
for c in "$RUN"/chunk.*; do
    [ "${c##*.}" = ours ] || [ "${c##*.}" = err ] || gpg_chunk "$c" > "$c.gpg" &
done
wait
end=$(date +%s)
gpgconf --homedir "$G" --kill gpg-agent || true
cat "$RUN"/chunk.*.ours | sort -t $'\t' -k2,2 > "$RUN/ours.tsv"
cat "$RUN"/chunk.*.gpg | sort -t $'\t' -k2,2 > "$RUN/gpg.tsv"
L=$(wc -l < "$RUN/list.tsv")
[ "$(wc -l < "$RUN/ours.tsv")" = "$L" ] || red "bitspkg gave $(wc -l < "$RUN/ours.tsv") verdicts for $L pairs (see $RUN/chunk.*.err)"
[ "$(wc -l < "$RUN/gpg.tsv")" = "$L" ] || red "gpg gave $(wc -l < "$RUN/gpg.tsv") verdicts for $L pairs"
# Joined: file, ours (verdict, fpr, reason), gpg (verdict, primary fpr, status). Every pair must join; the verdicts
# must be equal, and so must the primary key when both name one.
join -t $'\t' -1 2 -2 2 "$RUN/ours.tsv" "$RUN/gpg.tsv" > "$RUN/joined.tsv"
[ "$(wc -l < "$RUN/joined.tsv")" = "$L" ] || red "only $(wc -l < "$RUN/joined.tsv") of $L pairs have both verdicts"
awk -F'\t' '$2 != $5 || ($3 != "" && $6 != "" && toupper($3) != toupper($6))' "$RUN/joined.tsv" > "$RUN/diff.tsv"
same=$(awk -F'\t' '$3 != "" && $6 != ""' "$RUN/joined.tsv" | wc -l)
good=$(awk -F'\t' '$1=="good"' "$RUN/ours.tsv" | wc -l)
echo "corpus: $L signatures, $good good, $(( L - good )) refused, the same primary key as gpg on all $same where both name one (bitspkg $((mid-start)) s, gpg $((end-mid)) s, $P at once)"
awk -F'\t' '$1=="bad"{print $4}' "$RUN/ours.tsv" | sort | uniq -c | sort -rn | head -n 8
if [ -s "$RUN/diff.tsv" ]; then
    head -n 20 "$RUN/diff.tsv"
    red "$(wc -l < "$RUN/diff.tsv") verdicts or keys differ from gpg's ($RUN/diff.tsv: file, ours, fpr, reason, gpg, fpr, status)"
fi

# Flipped bytes: copies of a sample of packages up to 64 MiB, one byte changed each (first, last, or inside).
F="$RUN/flip"; mkdir "$F"
awk -F'\t' '{print $1"\t"$2}' "$RUN/list.tsv" | while IFS=$'\t' read -r f s; do
    sz=$(stat -c %s "$f"); [ "$sz" -le $((64 << 20)) ] && printf '%s\t%s\t%s\n' "$f" "$s" "$sz"
done > "$F/small.tsv"
M=$(wc -l < "$F/small.tsv"); W=${PGP_FLIP:-30}; [ "$W" -le "$M" ] || W=$M
[ "$W" -gt 0 ] || red "no package up to 64 MiB for the flipped-byte check"
awk -v k=$(( M / W )) 'NR % k == 0' "$F/small.tsv" | head -n "$W" > "$F/pick.tsv"
i=0; : > "$F/list.tsv"
while IFS=$'\t' read -r f s sz; do
    i=$((i + 1)); cp "$f" "$F/p$i"; cp "$s" "$F/p$i.sig"
    case $((i % 3)) in 0) off=0 ;; 1) off=$((sz - 1)) ;; *) off=$(( (i * 7919 * 4099) % sz )) ;; esac
    python3 -I -c 'import sys; p,o=sys.argv[1],int(sys.argv[2]); f=open(p,"r+b"); f.seek(o); b=f.read(1); f.seek(o); f.write(bytes([b[0]^0x20])); f.close()' "$F/p$i" "$off"
    printf '%s\t%s\n' "$F/p$i" "$F/p$i.sig" >> "$F/list.tsv"
done < "$F/pick.tsv"
"$BIN" pgp verify-list "$F/list.tsv" --keyring "$K" > "$F/out.tsv" 2>/dev/null
[ "$(wc -l < "$F/out.tsv")" = "$i" ] && [ "$(awk -F'\t' '$1=="bad"' "$F/out.tsv" | wc -l)" = "$i" ] || { cat "$F/out.tsv"; red "a package with a flipped byte was not refused"; }
echo "flipped byte: $i packages, all refused (SAMPLE of $W of $M packages up to 64 MiB, PGP_FLIP; all of them deferred under Georg's 6.11. rule)"
rm -f "$F"/p*

# gpg-made test keys in their own keyring folder (the real keyrings next to it).
E="$RUN/gen"; KT="$RUN/keyrings"; mkdir -m700 "$E"; mkdir "$KT"; cp "$K"/archlinux* "$K"/cachyos* "$KT/"
ge() { gpg --homedir "$E" --batch --no-tty --quiet --pinentry-mode loopback --passphrase '' "$@"; }
fpr() { ge --with-colons --list-keys "$1" | awk -F: '$1=="fpr"{print $10; exit}'; }
subfpr() { ge --with-colons --list-keys "$1" | awk -F: '$1=="sub"{s=1} s && $1=="fpr"{print $10; exit}'; }
past=$(date -u -d '-30 days' +%Y%m%dT%H%M%S)
for n in m1 m2 m3 p q x r v; do ge --quick-gen-key "$n <$n@bitsos.test>" ed25519 sign never 2>/dev/null; done
ge --quick-gen-key "rsa <rsa@bitsos.test>" rsa2048 sign never 2>/dev/null
ge --faked-system-time "$past" --quick-gen-key "e <e@bitsos.test>" ed25519 cert never 2>/dev/null
ge --faked-system-time "$past" --quick-add-key "$(fpr e@bitsos.test)" ed25519 sign 2d 2>/dev/null
D="$E/data"; head -c 100000 /dev/urandom > "$D"
sg() { local o="$E/$1.sig"; shift; ge "$@" --detach-sign -o "$o" "$D" 2>/dev/null || red "gpg could not make $o"; }
V=$(fpr v@bitsos.test)
# Signed while the key is still good; its revocation certificate comes into the keyring afterwards.
sg revsig -u "$V"
sed 's/^:-----BEGIN/-----BEGIN/' "$E/openpgp-revocs.d/$V.rev" | ge --import 2>/dev/null
for m in m1 m2 m3; do ge -u "$(fpr $m@bitsos.test)" --yes --quick-sign-key "$(fpr p@bitsos.test)" > /dev/null 2>&1; done
for m in m1 m2; do ge -u "$(fpr $m@bitsos.test)" --yes --quick-sign-key "$(fpr q@bitsos.test)" > /dev/null 2>&1; done
ge --armor --export m1@bitsos.test m2@bitsos.test m3@bitsos.test p@bitsos.test q@bitsos.test r@bitsos.test v@bitsos.test e@bitsos.test rsa@bitsos.test > "$KT/test.gpg"
for n in m1 m2 m3 r v e rsa; do echo "$(fpr $n@bitsos.test):4:"; done > "$KT/test-trusted"
fpr r@bitsos.test > "$KT/test-revoked"
sg good -u "$(fpr m1@bitsos.test)"
sg rsa -u "$(fpr rsa@bitsos.test)"
sg p3 -u "$(fpr p@bitsos.test)"
sg q2 -u "$(fpr q@bitsos.test)"
sg stranger -u "$(fpr x@bitsos.test)"
sg listed -u "$(fpr r@bitsos.test)"
sg expired --faked-system-time "$past" -u "$(subfpr e@bitsos.test)!"
sg sha1 -u "$(fpr rsa@bitsos.test)" --digest-algo SHA1
sg crit -u "$(fpr m1@bitsos.test)" --sig-notation '!crit@bitsos.test=1'
gpgconf --homedir "$E" --kill gpg-agent || true
fails=0
want() {
    local out; out=$("$BIN" pgp verify "$D" "$E/$1.sig" --keyring "$KT" 2>/dev/null || true)
    if [[ "$out" == *"$2"* ]]; then echo "  $1: $out"; else echo "  $1: WANT \"$2\", GOT: $out"; fails=$((fails + 1)); fi
}
echo "gpg-made test keys:"
want good "gut "; want rsa "gut "; want p3 "gut "
want q2 "2 von 3"; want stranger "nicht im Schlüsselbund"; want listed "gesperrt"; want revsig "widerrufen"
want expired "abgelaufen"; want sha1 "SHA-1"; want crit "Unterpaket 20"
[ "$fails" = 0 ] || red "$fails gpg-made test signatures got the wrong verdict"
echo "pgp corpus: green"
