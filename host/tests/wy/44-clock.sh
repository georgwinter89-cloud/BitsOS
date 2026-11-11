#!/usr/bin/env bash
# WY-17b on the host (headless back end): bitscomp's clock goes through libc and the zone, not the kernel's rule. A valid
# TZ gives Berlin's time, without " UTC", the offset libc answers; an unknown zone gives UTC and says so. Then V2's way,
# no TZ and only /etc/localtime, in a mount namespace with another file over it: a zone far from the host's (Tokyo), so
# the host's own zone cannot pass by chance, and an empty file, which is UTC and says so. The host's own /etc/localtime
# is never read. 43-v2-clock.sh proves the same path on V2.
set -euo pipefail
ROOT=${BITSOS_ROOT:-$(cd "$(dirname "$0")/../../.." && pwd)}
cd "$ROOT"
RUN=${BITSOS_RUN_DIR:-$(mktemp -d)}
mkdir -p "$RUN"
RUN=$(realpath "$RUN")
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-$ROOT/target}
BIN=$CARGO_TARGET_DIR/release/bitscomp
export XDG_RUNTIME_DIR=$RUN/xdg
mkdir -m 700 -p "$XDG_RUNTIME_DIR"
unset DISPLAY WAYLAND_DISPLAY

fail() { echo "FAIL: $*"; exit 1; }
(cd user/services/bitscomp && cargo build --release -q --offline)
[ -f /usr/share/zoneinfo/Europe/Berlin ] && [ -f /usr/share/zoneinfo/Asia/Tokyo ] || fail "no zoneinfo on this machine: the test cannot do its work"
[ -e /etc/localtime ] || fail "no /etc/localtime to cover on this machine: the test cannot do its work"
unshare -Urm true 2>/dev/null || fail "no user and mount namespace here: the test cannot do its work"

CPID=
cleanup() { [ -n "$CPID" ] && kill "$CPID" 2>/dev/null; wait 2>/dev/null || true; }
trap cleanup EXIT

# clock_of TZ-VALUE [FILE]: the `ctl clock` answer of a bitscomp started with it (without the variable when empty). With
# FILE: in a mount namespace where FILE lies over /etc/localtime (the link's target: glibc follows the link).
clock_of() {
    local name=wy44-$$-$RANDOM log=$RUN/clock.log
    if [ -n "${2:-}" ]; then
        env -u TZ unshare -Urm bash -c 'mount --bind "$1" "$(readlink -f /etc/localtime)" && shift && exec "$@"' -- "$2" \
            "$BIN" --backend headless --size 800x600 --socket "$name" --mcp none --settings none --installer none >"$log" 2>&1 &
    else
        env ${1:+TZ="$1"} "$BIN" --backend headless --size 800x600 --socket "$name" --mcp none --settings none --installer none >"$log" 2>&1 &
    fi
    CPID=$!
    for _ in $(seq 100); do grep -q "^WAYLAND_DISPLAY=" "$log" && break; sleep 0.1; done
    grep -q "^WAYLAND_DISPLAY=" "$log" || fail "no start line: $(tail -5 "$log")"
    local a=
    for _ in $(seq 50); do
        a=$("$BIN" ctl --socket "$name" clock)
        [[ $a =~ [0-9][0-9]:[0-9][0-9] ]] && break
        sleep 0.2
    done
    "$BIN" ctl --socket "$name" quit >/dev/null 2>&1 || true
    wait "$CPID" 2>/dev/null || true
    CPID=
    echo "$a"
}
# near HH:MM TZ: the time shown is the zone's now, one minute either way.
near() {
    local shown=$1 tz=$2 h m d
    h=$((10#${shown%:*})); m=$((10#${shown#*:}))
    d=$(( ( ($(TZ=$tz date +%-H) * 60 + $(TZ=$tz date +%-M)) - (h * 60 + m) + 1440) % 1440 ))
    [ "$d" -le 1 ] || [ "$d" -ge 1439 ]
}

for tz in Europe/Berlin "CET-1CEST,M3.5.0,M10.5.0/3"; do
    a=$(clock_of "$tz")
    echo "$a"
    [[ $a =~ ([0-9][0-9]:[0-9][0-9])\ local=true ]] || fail "TZ=$tz: not local: $a"
    near "${BASH_REMATCH[1]}" Europe/Berlin || fail "TZ=$tz: not Berlin's time ($(TZ=Europe/Berlin date +%H:%M)): $a"
    [[ $a =~ offset_min=(60|120) ]] || fail "TZ=$tz: libc's offset: $a"
    [[ $a != *" UTC "* ]] || fail "TZ=$tz: marked UTC: $a"
done
echo "ok: a valid zone gives Berlin's time, unmarked"

a=$(clock_of "Not/AZone")
echo "$a"
[[ $a =~ ([0-9][0-9]:[0-9][0-9])\ UTC\ local=false ]] || fail "an unknown zone must say UTC: $a"
near "${BASH_REMATCH[1]}" UTC || fail "an unknown zone must show UTC's time ($(TZ=UTC date +%H:%M)): $a"
echo "ok: an unknown zone gives UTC and says so"

a=$(clock_of "" /usr/share/zoneinfo/Asia/Tokyo)
echo "$a"
[[ $a =~ ([0-9][0-9]:[0-9][0-9])\ local=true\ tz=\(unset\) ]] || fail "/etc/localtime: not local: $a"
near "${BASH_REMATCH[1]}" Asia/Tokyo || fail "/etc/localtime: not Tokyo's time ($(TZ=Asia/Tokyo date +%H:%M)): $a"
[[ $a =~ localtime_tzif=true\ offset_min=540 ]] || fail "/etc/localtime: libc's offset: $a"
echo "ok: without TZ the zone comes from /etc/localtime"

: > "$RUN/empty-zone"
a=$(clock_of "" "$RUN/empty-zone")
echo "$a"
[[ $a =~ ([0-9][0-9]:[0-9][0-9])\ UTC\ local=false\ tz=\(unset\) ]] || fail "an empty /etc/localtime must say UTC: $a"
near "${BASH_REMATCH[1]}" UTC || fail "an empty /etc/localtime must show UTC's time ($(TZ=UTC date +%H:%M)): $a"
echo "ok: an empty /etc/localtime gives UTC and says so"
echo "GREEN: the clock follows the zone"
