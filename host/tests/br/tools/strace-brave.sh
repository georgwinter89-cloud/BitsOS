#!/usr/bin/env bash
# BR-1: Brave from CachyOS (read-only) inside bitscomp --backend headless, under strace -f, on the host.
# Not a fullrun scenario (needs Brave on the host, takes about a minute). Opens nothing on Georg's screen:
# a private XDG_RUNTIME_DIR, HOME and D-Bus address, no DISPLAY, the compositor has no window.
#
# usage: strace-brave.sh MODE OUT_DIR [SECS] [URL]
#   MODE trace : strace -f -yy -s 128 -o OUT_DIR/trace   (every call, decoded)
#   MODE count : strace -f -c          -o OUT_DIR/count   (counts and errors per syscall)
#   MODE net   : like trace, URL defaults to https://search.brave.com (the only outside host let through)
# Brave runs with V2's flags (V2-LANES BR-1); every process started here is stopped by PID / process group.
# Exit 0 only if the page reached its marker (title "all-ok", net: "Brave Search"), the trace has Brave's execve
# and nothing of the process group is left; 1 otherwise. Waits for the marker up to SECS+60 s, then traces at
# least SECS seconds in all.
set -euo pipefail

MODE=${1:?mode}
OUT=${2:?out dir}
SECS=${3:-30}
ROOT=${BITSOS_ROOT:-$(cd "$(dirname "$0")/../../../.." && pwd)}
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-$ROOT/target}
BIN=$CARGO_TARGET_DIR/release/bitscomp
BRAVE=/opt/brave-bin/brave
PORT=${BITSOS_PORT_LO:-31000}
SOCK=wayland-br1-$$
mkdir -p "$OUT"
OUT=$(cd "$OUT" && pwd)
RUN=$OUT/run
rm -rf "$RUN"
mkdir -p "$RUN/xdg" "$RUN/home" "$RUN/profile"
chmod 700 "$RUN/xdg"

# Only the local test page resolves; in net mode also search.brave.com (brief: read-only, no sign-in).
RULES="MAP * ~NOTFOUND, EXCLUDE 127.0.0.1"
URL=${4:-http://127.0.0.1:$PORT/}
MARK="all-ok"
if [ "$MODE" = net ]; then
    RULES="MAP * ~NOTFOUND, EXCLUDE 127.0.0.1, EXCLUDE search.brave.com"
    URL=${4:-https://search.brave.com/search?q=BitsOS}
    MARK="Brave Search"
fi
fail() { echo "FAIL: $*"; FAILED=1; }
FAILED=0

case "$MODE" in
    trace|net) STRACE=(strace -f -qq -yy -s 128 -o "$OUT/trace") ;;
    count) STRACE=(strace -f -qq -c -o "$OUT/count") ;;
    *) echo "unknown mode $MODE"; exit 2 ;;
esac

[ -x "$BIN" ] || (cd "$ROOT/user/services/bitscomp" && cargo build --release -q)

HTTP="" COMP="" PG=""
cleanup() {
    if [ -n "$PG" ]; then
        # PG is strace's pid and the group id. Brave's main process first, so strace sees a normal shutdown and
        # writes its summary; then the group.
        main=$(awk '{print $1}' "/proc/$PG/task/$PG/children" 2>/dev/null || true)
        [ -n "$main" ] && kill -TERM "$main" 2>/dev/null || true
        for _ in $(seq 75); do kill -0 "$PG" 2>/dev/null || break; sleep 0.2; done
        kill -TERM -- "-$PG" 2>/dev/null || true
        for _ in $(seq 50); do kill -0 -- "-$PG" 2>/dev/null || break; sleep 0.2; done
        kill -KILL -- "-$PG" 2>/dev/null || true
        sleep 0.5
        if kill -0 -- "-$PG" 2>/dev/null; then fail "process group $PG still alive after SIGKILL"; fi
    fi
    [ -n "$COMP" ] && kill "$COMP" 2>/dev/null || true
    [ -n "$HTTP" ] && kill "$HTTP" 2>/dev/null || true
    wait 2>/dev/null || true
    rm -rf "$RUN/profile" "$RUN/home"
}
trap cleanup EXIT

# A server left on the port would serve some other page without any error: refuse a busy port, then wait until
# ours answers with the page's own file.
PAGE=$ROOT/host/tests/br/page
if (exec 3<>"/dev/tcp/127.0.0.1/$PORT") 2>/dev/null; then echo "FAIL: port $PORT already in use"; exit 1; fi
python3 -m http.server "$PORT" --bind 127.0.0.1 --directory "$PAGE" >"$OUT/http.log" 2>&1 &
HTTP=$!
up=0
for _ in $(seq 50); do
    kill -0 "$HTTP" 2>/dev/null || break
    if curl -sf --max-time 2 "http://127.0.0.1:$PORT/index.html" | cmp -s - "$PAGE/index.html"; then up=1; break; fi
    sleep 0.2
done
if [ "$up" != 1 ]; then echo "FAIL: http server on $PORT did not serve $PAGE"; exit 1; fi

XDG_RUNTIME_DIR=$RUN/xdg "$BIN" --backend headless --size 1280x800 --socket "$SOCK" >"$OUT/bitscomp.log" 2>&1 &
COMP=$!
for _ in $(seq 50); do [ -S "$RUN/xdg/$SOCK.ctl" ] && break; sleep 0.2; done
ctl() { XDG_RUNTIME_DIR=$RUN/xdg "$BIN" ctl --socket "$SOCK" "$@"; }

# env -i: what V2 will give Brave (no desktop session, no D-Bus, no PulseAudio, so Chromium falls back to ALSA).
# setsid makes strace (and so Brave and all its children) one process group to stop at the end. The group id
# comes from the new leader itself (setsid forks when it starts as a group leader, so $! is not always it).
setsid bash -c 'echo $$ >"$0"; exec "$@"' "$RUN/pgid" env -i PATH=/usr/bin HOME="$RUN/home" LANG=de_DE.UTF-8 TZ=Europe/Berlin \
    XDG_RUNTIME_DIR="$RUN/xdg" WAYLAND_DISPLAY="$SOCK" \
    DBUS_SESSION_BUS_ADDRESS="unix:path=$RUN/xdg/no-bus" PULSE_SERVER="unix:$RUN/xdg/no-pulse" \
    PIPEWIRE_REMOTE="$RUN/xdg/no-pipewire" \
    "${STRACE[@]}" -- "$BRAVE" \
    --ozone-platform=wayland --disable-gpu --password-store=basic --no-sandbox \
    --disable-breakpad --disable-crash-reporter --no-first-run --no-default-browser-check \
    --user-data-dir="$RUN/profile" --autoplay-policy=no-user-gesture-required \
    --host-resolver-rules="$RULES" "$URL" >"$OUT/brave.log" 2>&1 &
for _ in $(seq 50); do [ -s "$RUN/pgid" ] && break; sleep 0.1; done
PG=$(cat "$RUN/pgid" 2>/dev/null || true)
[ -n "$PG" ] || { echo "FAIL: no process group id from setsid"; exit 1; }

start=$SECONDS
seen=""
while [ $((SECONDS - start)) -lt $((SECS + 60)) ]; do
    ctl list >"$RUN/list.now" 2>/dev/null || true
    if grep -qF "$MARK" "$RUN/list.now"; then seen=$((SECONDS - start)); break; fi
    sleep 1
done
left=$((SECS - (SECONDS - start)))
[ "$left" -gt 0 ] && sleep "$left"
ctl list >"$OUT/list.txt" 2>&1 || true
ctl shot "$OUT/shot.ppm" >/dev/null 2>&1 || true
cat "$OUT/list.txt"
cleanup
PG="" COMP="" HTTP=""
trap - EXIT

[ -n "$seen" ] && grep -qF "$MARK" "$OUT/list.txt" || fail "page marker '$MARK' never in ctl list"
case "$MODE" in
    count) grep -qE '^ *100[,.]00 .* total$' "$OUT/count" 2>/dev/null || fail "no strace -c summary in $OUT/count" ;;
    *) grep -qF 'execve("/opt/brave-bin/brave"' "$OUT/trace" 2>/dev/null || fail "no execve of Brave in $OUT/trace" ;;
esac
[ "$FAILED" = 0 ] || exit 1
echo "done: $OUT (marker '$MARK' seen after $seen s)"
