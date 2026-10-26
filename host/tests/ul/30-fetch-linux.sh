#!/bin/bash
# UL-4a: fetch on CachyOS, the oracle for its cases. Builds fetch (static musl), starts host/tests/ul/fetch_server.py
# (test CAs made with the openssl CLI, HTTPS on 127.0.0.1:28540, HTTP on 28541), runs every line of
# user/tools/fetch/tests/cases.txt and checks the exit code and what is left on disk, then stdout output, the
# system CA bundle refusing the test server, and progress lines. The ul.fetch.cases ktest runs the same table on V2.
set -eu
BITSOS_ROOT="${BITSOS_ROOT:-$(cd "$(dirname "$0")/../../.." && pwd)}"
cd "$BITSOS_ROOT"
for t in python3 openssl; do command -v $t >/dev/null || { echo "SKIP: $t missing"; exit 0; }; done
T="$BITSOS_ROOT/target/user-scenario"
(cd user && env -u CARGO_TARGET_DIR -u RUSTUP_TOOLCHAIN cargo build -q --offline --release --target x86_64-unknown-linux-musl --target-dir "$T" -p fetch)
FETCH="$T/x86_64-unknown-linux-musl/release/fetch"
RUN="$BITSOS_ROOT/target/ul-fetch-linux"
rm -rf "$RUN"; mkdir -p "$RUN/srv"
SPID=""
stop() { rc=$?; if [ -n "$SPID" ]; then kill "$SPID" 2>/dev/null || true; wait "$SPID" 2>/dev/null || true; fi; exit $rc; }
trap stop EXIT
python3 -I host/tests/ul/fetch_server.py "$RUN/srv" 28540 28541 2> "$RUN/server.log" &
SPID=$!
for _ in $(seq 1 100); do [ -f "$RUN/srv/ready" ] && break; sleep 0.1; done
[ -f "$RUN/srv/ready" ] || { echo "RED: fetch_server.py did not start"; cat "$RUN/server.log"; exit 1; }
SHA=$(cut -c1-64 "$RUN/srv/blob.sha256")
H=127.0.0.1
fail=0
while read -r name want rest; do
    case "$name" in ''|'#'*) continue ;; esac
    D="$RUN/case-$name"; mkdir -p "$D"
    args=$(echo "$rest" | sed "s|@SHA|$SHA|g; s|@CA2|$RUN/srv/ca2.pem|g; s|@CA|$RUN/srv/ca.pem|g; s|@S|https://$H:28540|g; s|@P|http://$H:28541|g; s|@R|https://$H:28549|g; s|@D|$D|g; s|@H|$H|g")
    explicit=0; case " $args " in *" -o "*) explicit=1 ;; esac
    [ $explicit = 1 ] || args="$args -o $D/$name"
    set +e; $FETCH $args > "$D/stdout" 2> "$D/stderr"; got=$?; set -e
    note=""
    if [ $explicit = 0 ]; then
        if [ "$want" = 0 ]; then [ -f "$D/$name" ] || note=" (no output file)"
        elif [ -e "$D/$name" ] || [ -e "$D/$name.part" ] || [ -e "$D/$name.part.1" ]; then note=" (left a file behind)"; fi
    fi
    if [ "$got" = "$want" ] && [ -z "$note" ]; then echo "ok   $name exit $got"
    else echo "RED  $name: exit $got, want $want$note"; sed 's/^/     /' "$D/stderr"; fail=1; fi
done < user/tools/fetch/tests/cases.txt

out=$($FETCH -q --ca "$RUN/srv/ca.pem" -o - "https://localhost:28540/small" 2> "$RUN/small.err")
[ "$out" = "hello from the fetch test server" ] && grep -q "^sha256 [0-9a-f]\{64\}$" "$RUN/small.err" && echo "ok   stdout + DNS name localhost" \
    || { echo "RED  -o - to stdout: '$out'"; cat "$RUN/small.err"; fail=1; }
set +e; $FETCH -q -o "$RUN/sys" "https://$H:28540/blob" 2> "$RUN/sys.err"; got=$?; set -e
[ $got = 5 ] && grep -qi "unknown\|issuer" "$RUN/sys.err" && echo "ok   system bundle refuses the test CA" || { echo "RED  system bundle: exit $got"; cat "$RUN/sys.err"; fail=1; }
line=$($FETCH --ca "$RUN/srv/ca.pem" -o "$RUN/p" "https://$H:28540/blob" 2>&1 >/dev/null | tail -1)
case "$line" in "fetch: 3.0 MiB / 3.0 MiB (100%) "*" in "*" s") echo "ok   progress: $line" ;; *) echo "RED  progress line: $line"; fail=1 ;; esac
P="$RUN/par"; mkdir -p "$P"
set +e
$FETCH -q --ca "$RUN/srv/ca.pem" --sha256 "$SHA" -o "$P/f" "https://$H:28540/blob" > /dev/null 2> "$P/a.err" & A=$!
$FETCH -q --ca "$RUN/srv/ca.pem" --sha256 "$SHA" -o "$P/f" "https://$H:28540/blob" > /dev/null 2> "$P/b.err"; rb=$?
wait $A; ra=$?; set -e
left=$(ls -A "$P" | grep -v '\.err$' | tr '\n' ' ')
[ $ra = 0 ] && [ $rb = 0 ] && [ "$left" = "f " ] && [ "$(sha256sum < "$P/f" | cut -c1-64)" = "$SHA" ] && echo "ok   two runs to the same -o at once" \
    || { echo "RED  parallel: exits $ra $rb, left: $left"; cat "$P"/*.err; fail=1; }
ln -s real "$RUN/link"
$FETCH -q --ca "$RUN/srv/ca.pem" -o "$RUN/link" "https://$H:28540/small" > /dev/null 2>&1 && [ -L "$RUN/link" ] && [ -f "$RUN/real" ] && echo "ok   a symlink -o is written through" \
    || { echo "RED  symlink -o"; fail=1; }
[ $fail = 0 ] || exit 1
echo "fetch on CachyOS: green"
