#!/bin/bash
# UL-4b: procs, evtest, fbtest, memstress, alsa-test on CachyOS, the oracle for user/tools/cases.txt (exit codes
# and the transcripts in user/tools/expected/). The ul.tools.cases ktest runs the same table on V2.
# Never opens /dev/input/* or /dev/fb0 here (Georg's keyboard and screen): the evdev/fbdev cases use fake files.
set -eu
BITSOS_ROOT="${BITSOS_ROOT:-$(cd "$(dirname "$0")/../../.." && pwd)}"
cd "$BITSOS_ROOT"
T="$BITSOS_ROOT/target/user-scenario"
(cd user && env -u CARGO_TARGET_DIR -u RUSTUP_TOOLCHAIN cargo build -q --offline --release --target x86_64-unknown-linux-musl --target-dir "$T" -p procs -p evtest -p fbtest -p memstress -p alsa-test -p bitspkg)
BIN="$T/x86_64-unknown-linux-musl/release"
RUN="$BITSOS_ROOT/target/ul-tools-linux"
fail=0
while read -r name want expect prog rest; do
    case "$name" in ''|'#'*) continue ;; esac
    D="$RUN/$name"; rm -rf "$D"; mkdir -p "$D"
    touch "$D/event0" "$D/event1" "$D/mouse0" "$D/fb"
    args=${rest//@D/$D}
    set +e; (cd "$D" && "$BIN/$prog" $args > "$D.out" 2> "$D.err"); got=$?; set -e
    note=""
    if [ "$expect" != - ] && ! diff <(sed "s|@D|$D|g" "user/tools/expected/$expect") "$D.out" > "$D.diff"; then note=" (stdout differs)"; fi
    if [ "$got" = "$want" ] && [ -z "$note" ]; then echo "ok   $name exit $got"
    else echo "RED  $name: exit $got, want $want$note"; sed 's/^/     /' "$D.err" "$D.diff" 2>/dev/null | head -20; fail=1; fi
done < user/tools/cases.txt
out=$("$BIN/procs" -p $$)
echo "$out" | grep -q "^ *$$ " && echo "ok   procs -p shows this shell" || { echo "RED  procs -p $$:"; echo "$out"; fail=1; }
n=$("$BIN/procs" --top -n 2 -d 0.1 | grep -c '^procs - up ')
[ "$n" = 2 ] && echo "ok   procs --top -n 2: two frames" || { echo "RED  procs --top: $n frames"; fail=1; }
out=$("$BIN/memstress" truncate) && echo "ok   memstress truncate: SIGBUS" || { echo "RED  memstress truncate: $out"; fail=1; }
[ $fail = 0 ] || exit 1
echo "tools on CachyOS: green"
