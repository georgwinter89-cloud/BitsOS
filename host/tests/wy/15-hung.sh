#!/usr/bin/env bash
# WY-4 (B4) on the host, headless back end (no QEMU, nothing on the screen): bitscomp never waits for a client.
# wyclient A animates (a frame per callback, buffers reused only after release) while B stops itself (SIGSTOP:
# reads nothing) and C ignores pings (keeps reading). Checks: B and C become hung (ctl list hung=true, log
# "antwortet nicht"), A does not and keeps getting frame callbacks; the hung windows show the grey veil with the
# box (ctl shot) and keep their place; the "Beenden" button (pointer click at ctl hot's kill=) ends B by SIGTERM
# alone (bitscomp sends SIGCONT with it, so a stopped program runs to its end); ctl kill on C, which ignores
# SIGTERM, needs the SIGKILL 2 s later; E stops reading its socket and ctl flood overflows its output queue:
# bitscomp drops it with a line and A's frames go on; a stopped client that gets SIGCONT answers again
# (hung=false, "answers again"); "Programm beenden" in the circle menu ends A by SIGTERM.
# Frame-time numbers are printed, judged only loosely (callbacks keep coming).
set -euo pipefail

ROOT=${BITSOS_ROOT:-$(cd "$(dirname "$0")/../../.." && pwd)}
RUN=${BITSOS_RUN_DIR:-$(mktemp -d)}
mkdir -p "$RUN"
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-$ROOT/target}
BIN=$CARGO_TARGET_DIR/release/bitscomp
CLIENT=$CARGO_TARGET_DIR/release/wyclient
SOCK=wayland-wy-hung-$$
export XDG_RUNTIME_DIR=${XDG_RUNTIME_DIR:-$RUN}
export WAYLAND_DISPLAY=$SOCK
unset DISPLAY

echo "== build"
(cd "$ROOT/user/services/bitscomp" && cargo build --release -q --workspace)

PIDS=()
COMP=""
cleanup() {
    for p in "${PIDS[@]}"; do kill -CONT "$p" 2>/dev/null || true; kill -KILL "$p" 2>/dev/null || true; done
    [ -n "$COMP" ] && kill "$COMP" 2>/dev/null || true
}
trap cleanup EXIT

ctl() { "$BIN" ctl --socket "$SOCK" "$@"; }
fail() { echo "FAIL: $*"; ctl list || true; tail -20 "$RUN/bitscomp.log"; exit 1; }
wait_for() { # pattern, what, [tries of 0.2 s]
    for _ in $(seq "${3:-50}"); do
        ctl list 2>/dev/null | grep -Eq -- "$1" && return 0
        sleep 0.2
    done
    fail "$2"
}
wait_gone() { # title, what
    for _ in $(seq 40); do
        ctl list 2>/dev/null | grep -q -- "title=\"$1\"" || return 0
        sleep 0.2
    done
    fail "$2"
}
field() { # list-line pattern, field name -> value
    ctl list | grep -E -- "$1" | head -1 | sed -E "s/.* $2=([^ ]*).*/\1/"
}
hot_field() { # title, field -> value
    ctl hot | grep -E -- "^win .*title=\"$1\"" | head -1 | tr ' ' '\n' | sed -n "s/^$2=//p" | tr ',' ' '
}
pixel() { # ppm, x, y -> "r g b"
    python3 -I -c 'import sys
d=open(sys.argv[1],"rb").read(); parts=d.split(None,4); w=int(parts[1]); x,y=int(sys.argv[2]),int(sys.argv[3])
off=len(d)-len(parts[4])+3*(y*w+x); print(d[off],d[off+1],d[off+2])' "$@"
}

echo "== start (headless 1280x800)"
"$BIN" --backend headless --size 1280x800 --socket "$SOCK" --mcp none >"$RUN/bitscomp.log" 2>&1 &
COMP=$!
for _ in $(seq 25); do [ -S "$XDG_RUNTIME_DIR/$SOCK.ctl" ] && break; sleep 0.2; done

"$CLIENT" --title A --pattern 0 --animate >"$RUN/a.log" 2>&1 & PIDS+=($!); APID=$!
wait_for 'title="A"' "A did not map"
"$CLIENT" --title B --pattern 1 --stop 1500 >"$RUN/b.log" 2>&1 & PIDS+=($!); BPID=$!
wait_for 'title="B"' "B did not map"
"$CLIENT" --title C --pattern 2 --hang 1000 --ignore-term >"$RUN/c.log" 2>&1 & PIDS+=($!); CPID=$!
wait_for 'title="C"' "C did not map"
[ "$(field 'title="B"' pid)" = "$BPID" ] || fail "B's pid from SO_PEERCRED is not $BPID"

echo "== B (SIGSTOP) and C (ignores pings) become hung, A does not"
wait_for 'title="B" pid=[0-9]+ hung=true' "B not hung within 10 s" 50
wait_for 'title="C" pid=[0-9]+ hung=true' "C not hung within 10 s" 50
grep -q "stopping" "$RUN/b.log" || fail "B did not stop itself"
grep -q "ignoring pings" "$RUN/c.log" || fail "C did not start ignoring pings"
ctl list | grep -q 'title="A" pid=[0-9]* hung=false' || fail "A is marked hung"
grep -q "(wyclient) of pid $BPID: antwortet nicht" "$RUN/bitscomp.log" || fail "no 'antwortet nicht' line for B"
before=$(grep -c "^frames" "$RUN/a.log" || true)
sleep 4.5
after=$(grep -c "^frames" "$RUN/a.log" || true)
[ "$after" -ge $((before + 2)) ] || fail "A's frame reports stopped while B and C hang ($before -> $after)"
last=$(grep "^frames" "$RUN/a.log" | tail -1)
echo "A while B and C hang: $last"
n=$(echo "$last" | sed -E 's/.*n=([0-9]+).*/\1/')
[ "$n" -ge 30 ] || fail "A got only $n callbacks in 2 s while B and C hang"
echo "$last" | grep -q "starved=0" || echo "note: A waited for a buffer release: $last"

echo "== the veil: hung windows grey with the box, A untouched"
ctl shot "$RUN/hung.ppm" >/dev/null
read -r bx by bw bh <<<"$(hot_field B body)"
read -r ax ay aw ah <<<"$(hot_field A body)"
corner=$(pixel "$RUN/hung.ppm" $((bx + 4)) $((by + 4)))
a_px=$(pixel "$RUN/hung.ppm" $((ax + 4)) $((ay + 4)))
echo "B's corner under the veil: $corner, A's corner: $a_px"
[ "$a_px" = "192 48 48" ] || fail "A's own picture is not on screen ($a_px)"
[ "$corner" != "48 160 64" ] || fail "B is not veiled"
read -r r g b <<<"$corner"
mx=$(printf '%s\n' "$r" "$g" "$b" | sort -n | tail -1); mn=$(printf '%s\n' "$r" "$g" "$b" | sort -n | head -1)
[ $((mx - mn)) -le 40 ] || fail "B's veil is not grey ($corner)"
# WY-5 look: the red "Beenden" button (danger colour over the veil) left of "Warten" in B's middle.
read -r kx ky <<<"$(hot_field B kill)"
red=$(python3 -I - "$RUN/hung.ppm" "$kx" "$ky" <<'PY'
import sys
d = open(sys.argv[1], "rb").read().split(maxsplit=4)
w, px, x0, y = int(d[1]), d[4], int(sys.argv[2]), int(sys.argv[3])
# The button's red border (#ff6b6b) within 120 pixels left of its middle.
print(any(px[(y * w + x) * 3] > px[(y * w + x) * 3 + 1] + 60 for x in range(max(0, x0 - 120), x0)))
PY
)
[ "$red" = True ] || fail "no red 'Beenden' button border in B's middle"
ctl hot | grep -E "^win .*wait=.*title=\"B\"" >/dev/null || fail "no 'Warten' button for B in ctl hot"
ctl hot | grep -q "title=\"B\"" && ctl hot | grep -E "^win .*kill=.*title=\"B\"" >/dev/null || fail "no kill button for B in ctl hot"

echo "== Beenden on B (stopped): SIGTERM with SIGCONT ends it, no SIGKILL needed"
read -r kx ky <<<"$(hot_field B kill)"
ctl click "$kx" "$ky" >/dev/null
wait_gone B "B's window did not go after Beenden"
for _ in $(seq 20); do kill -0 "$BPID" 2>/dev/null || break; sleep 0.2; done
kill -0 "$BPID" 2>/dev/null && fail "B's process is still there"
grep -q "of pid $BPID: SIGTERM (Beenden)" "$RUN/bitscomp.log" || fail "no SIGTERM line for B"
sleep 2.5
grep -q "of pid $BPID: still there" "$RUN/bitscomp.log" && fail "B needed SIGKILL although SIGCONT went with SIGTERM"

echo "== ctl kill on C (ignores SIGTERM): SIGKILL after 2 s"
cid=$(field 'title="C"' id)
ctl kill "$cid" | grep -q "SIGTERM sent" || fail "ctl kill $cid"
wait_gone C "C's window did not go after ctl kill"
for _ in $(seq 25); do kill -0 "$CPID" 2>/dev/null || break; sleep 0.2; done
kill -0 "$CPID" 2>/dev/null && fail "C's process is still there"
grep -q "of pid $CPID: still there 2 s after SIGTERM: SIGKILL" "$RUN/bitscomp.log" || fail "no SIGKILL line for C"

echo "== E stops reading: a flood of configures overflows its queue, bitscomp drops it and A goes on"
"$CLIENT" --title E --pattern 4 --noread 1000 >"$RUN/e.log" 2>&1 & PIDS+=($!); EPID=$!
wait_for 'title="E"' "E did not map"
for _ in $(seq 25); do grep -q "not reading" "$RUN/e.log" && break; sleep 0.2; done
grep -q "not reading" "$RUN/e.log" || fail "E did not stop reading"
eid=$(field 'title="E"' id)
before=$(grep -c "^frames" "$RUN/a.log" || true)
flood=$(ctl flood "$eid" 200000)
echo "flood: $flood"
wait_gone E "E's window did not go after its queue overflowed"
grep -q "client pid $EPID dropped: its output queue" "$RUN/bitscomp.log" || fail "no 'output queue overflowed' line for E"
kill -0 "$EPID" 2>/dev/null || fail "E's process should still run (only its client is dropped)"
sleep 2.5
after=$(grep -c "^frames" "$RUN/a.log" || true)
[ "$after" -gt "$before" ] || fail "A's frame reports stopped around E's overflow ($before -> $after)"
grep "^frames" "$RUN/a.log" | tail -1

echo "== a stopped client that gets SIGCONT answers again"
"$CLIENT" --title D --pattern 3 --stop 500 >"$RUN/d.log" 2>&1 & PIDS+=($!); DPID=$!
wait_for 'title="D" pid=[0-9]+ hung=true' "D not hung" 50
kill -CONT "$DPID"
wait_for 'title="D" pid=[0-9]+ hung=false' "D does not answer again after SIGCONT" 25
grep -q "of pid $DPID: answers again" "$RUN/bitscomp.log" || fail "no 'answers again' line"

echo "== Programm beenden from the circle menu on A"
read -r hx hy <<<"$(hot_field A header)"
ctl click "$hx" "$hy" right >/dev/null
at=$(ctl hot | sed -n 's/^item=5 at=\([0-9]*\),\([0-9]*\) .*label="Programm beenden"/\1 \2/p')
[ -n "$at" ] || fail "no 'Programm beenden' in the window menu"
ctl click $at >/dev/null
wait_gone A "A did not go after Programm beenden"
grep -q "of pid $APID: SIGTERM (Programm beenden)" "$RUN/bitscomp.log" || fail "no SIGTERM line for A"
sleep 2.5
grep -q "of pid $APID: still there" "$RUN/bitscomp.log" && fail "A needed SIGKILL"

echo "== quit"
ctl quit >/dev/null
for _ in $(seq 25); do kill -0 "$COMP" 2>/dev/null || break; sleep 0.2; done
kill -0 "$COMP" 2>/dev/null && fail "bitscomp did not quit"
COMP=""
grep -E "^bitscomp: (window|client)" "$RUN/bitscomp.log" || true
echo "PASS: 15-hung"
