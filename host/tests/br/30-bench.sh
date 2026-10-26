#!/usr/bin/env bash
# BR-4a: the BR-4 measuring tools work before the desk takes them to the Mini-PC. page/bench.html in the staged
# Brave under headless bitscomp (tools/reference.sh: never a window on the desktop) reaches the final title of
# ?mode=load and ?mode=scroll, and of ?mode=video with short local VP9 and AV1 clips (tools/make-bench-clips.sh,
# when ffmpeg can make them); tools/br4-procs.sh on a dummy process prints numbers in every column. The shape of
# the results only: timing is the Mini-PC's (the check list in plan/notes/br-BR-4a.md).
set -uo pipefail
here=$(cd "$(dirname "$0")" && pwd)
ROOT=${BITSOS_ROOT:-$(cd "$here/../../.." && pwd)}
RUN=${BITSOS_RUN_DIR:-$ROOT/target/br/bench}
PORT=$((${BITSOS_PORT_LO:-31000} + 40))
STAGE=${BRAVE_STAGE:-/var/tmp/claude-v2/stage/brave}
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-$ROOT/target}
BB=$ROOT/user/ports/busybox/out/busybox
rm -rf "$RUN"; mkdir -p "$RUN"
fails=0
ok() { echo "ok   $1"; }
bad() { echo "FAIL $1"; fails=$((fails + 1)); }
HTTP=""
trap '[ -n "$HTTP" ] && kill "$HTTP" 2>/dev/null; [ -n "${DUMMY:-}" ] && kill "$DUMMY" 2>/dev/null; wait 2>/dev/null' EXIT

# br4-procs.sh first (no Brave needed): a dummy whose argv looks like a Chromium renderer's rewritten argv.
SH=sh
[ -x "$BB" ] && SH="$BB sh"
pat="/br4dummy-$$/brave"
bash -c "exec -a '$pat --type=renderer' sleep 30" &
DUMMY=$!
sleep 0.3
P=$RUN/procs.txt
# shellcheck disable=SC2086
$SH "$here/tools/br4-procs.sh" 1 "$pat" >"$P" 2>&1 || true
if grep -qx "$DUMMY renderer [0-9.]* [0-9]* [0-9]*" "$P" \
    && [ "$(sed -n 1p "$P")" = "pid type cpu% rss_kb pss_kb" ] \
    && awk 'NR > 1 && $1 != "sum" && $1 !~ /^MemAvailable/ { if ($1 !~ /^[0-9]+$/ || $3 !~ /^[0-9.]+$/ || $4 !~ /^[0-9]+$/ || $5 !~ /^[0-9]+$/) bad = 1 } END { exit bad }' "$P" \
    && grep -q '^sum cpu%=[0-9.]* rss_kb=[0-9]* pss_kb=[0-9]* over 1s$' "$P" && grep -q '^MemAvailable_kb=[0-9]*$' "$P"; then
    ok "br4-procs: the dummy renderer and the sums in numbers ($(grep -c . "$P") lines)"
else
    bad "br4-procs output:"; sed 's/^/     /' "$P"
fi
kill "$DUMMY" 2>/dev/null; DUMMY=""

[ -x "$STAGE/brave" ] || { echo "SKIP: no staged Brave in $STAGE (bash user/ports/brave-launcher/scripts/stage-v2.sh)"; exit "$((fails > 0))"; }
(cd "$ROOT/user/services/bitscomp" && cargo build --release -q --offline) || { echo "FAIL: bitscomp build"; exit 1; }

# The pages and clips next to each other, as the desk serves /var/tmp/claude-v2/br-bench on the Mini-PC run.
WWW=$RUN/www
mkdir -p "$WWW"
clips=0
if command -v ffmpeg >/dev/null && bash "$here/tools/make-bench-clips.sh" "$WWW" 4 >"$RUN/clips.log" 2>&1; then
    clips=1
else
    cp "$here/page/bench.html" "$WWW/"
    echo "note: no clips (ffmpeg missing or failed, $RUN/clips.log): the video mode is not checked"
fi
if (exec 3<>"/dev/tcp/127.0.0.1/$PORT") 2>/dev/null; then echo "FAIL: port $PORT is busy"; exit 1; fi
python3 -I -m http.server "$PORT" --bind 127.0.0.1 --directory "$WWW" >"$RUN/http.log" 2>&1 &
HTTP=$!
up=0
for _ in $(seq 50); do curl -sf "http://127.0.0.1:$PORT/bench.html" | grep -q "BR4 loading" && { up=1; break; }; sleep 0.2; done
[ "$up" = 1 ] || { echo "FAIL: http server on $PORT did not serve $WWW"; exit 1; }

# One Brave per mode; the final title is checked against its shape. The OUT path must stay short (socket path).
bench() {
    local name=$1 mode=$2 query=$3 re=$4 out
    out=/var/tmp/claude-v2/br-30-$$-$name
    if bash "$here/tools/reference.sh" "$out" "$PORT" "http://br4.test:$PORT/bench.html?$query" "BR4 $mode done" \
        --test-type "--host-resolver-rules=MAP br4.test 127.0.0.1, MAP * ~NOTFOUND" \
        "--unsafely-treat-insecure-origin-as-secure=http://br4.test:$PORT" >"$RUN/$name.log" 2>&1; then
        local t
        t=$(grep -o 'title="BR4 [^"]*"' "$out/list.txt" | head -1)
        if echo "$t" | grep -Eq "$re"; then ok "$name: $t"; else bad "$name: final title $t does not match $re"; fi
    else
        bad "$name: $(tail -3 "$RUN/$name.log" | tr '\n' '|')"
    fi
    cp "$out/list.txt" "$RUN/$name-list.txt" 2>/dev/null; cp "$out/brave.log" "$RUN/$name-brave.log" 2>/dev/null
    rm -rf "$out"
}
bench load load "mode=load" '^title="BR4 load done dcl=[0-9]+ms load=[0-9]+ms heap=[0-9]+MB( - Brave)?"$'
bench scroll scroll "mode=scroll&s=3" '^title="BR4 scroll done fps=[0-9.]+ p95=[0-9.]+ms long=[0-9]+/[1-9][0-9]* [0-9]+x[0-9]+@[0-9.]+ heap=[0-9]+MB( - Brave)?"$'
if [ "$clips" = 1 ]; then
    for c in vp9 av1; do
        bench "video-$c" video "mode=video&s=2&src=clip-$c-1080p60.webm" '^title="BR4 video done 1920x1080 dropped=[0-9]+/[1-9][0-9]* \([0-9.]+%\) fps=[0-9.]+ heap=[0-9]+MB( - Brave)?"$'
    done
fi
[ "$fails" = 0 ] && echo "PASS: BR-4 tools" || echo "RED: $fails failed (logs in $RUN)"
exit "$((fails > 0))"
