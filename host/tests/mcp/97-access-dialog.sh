#!/usr/bin/env bash
# MC-10 on the host (bitscomp's headless back end, nothing on the screen): the full-access dialog through the real
# input path, against a stand-in for bitsinstall's confirmer socket (access_stand_in.py). Injected Enter on the
# preselected Erlauben goes out marked injected (the installer refuses it); a real Enter counts only once armed,
# and every real key before the arming starts it again (a stray Enter typed elsewhere never grants); a real
# Enter after that grants 24 h without the NVMe box; the live grant shows "Fernzugriff offen bis HH:MM" in the
# bar; Strg+Alt+V from injected keys opens nothing, from the real keyboard Georg's own dialog, whose
# "Fernzugriff beenden" sends the revoke, and the chip goes.
set -euo pipefail

ROOT=${BITSOS_ROOT:-$(cd "$(dirname "$0")/../../.." && pwd)}
RUN=${BITSOS_RUN_DIR:-$(mktemp -d)}
mkdir -p "$RUN"
RUN=$(realpath "$RUN")
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-$ROOT/target}
BIN=$CARGO_TARGET_DIR/release/bitscomp
export XDG_RUNTIME_DIR=$RUN/xdg
mkdir -m 700 -p "$XDG_RUNTIME_DIR"
unset DISPLAY WAYLAND_DISPLAY
SOCK=mc97-$$
ISOCK=$RUN/install.sock
FLOG=$RUN/stand-in.log
FP=$(printf 'cd%.0s' $(seq 32))
rm -f "$ISOCK" "$FLOG"
: >"$FLOG"

fail() { echo "FAIL: $*"; echo "--- stand-in"; tail -20 "$FLOG"; echo "--- bitscomp"; grep -a installer "$RUN/bitscomp.log" | tail -20; exit 1; }

echo "== build bitscomp"
(cd "$ROOT/user/services/bitscomp" && cargo build --release -q --offline)

PIDS=()
cleanup() {
    for p in "${PIDS[@]}"; do kill "$p" 2>/dev/null || true; done
    for p in "${PIDS[@]}"; do wait "$p" 2>/dev/null || true; done
}
trap cleanup EXIT

ctl() { "$BIN" ctl --socket "$SOCK" "$@"; }
key() { ctl key "$@" >/dev/null; }
phys() { ctl phys "$@" >/dev/null; }
wait_for() { # pattern, what
    for _ in $(seq 100); do ctl install 2>/dev/null | grep -q -- "$1" && return 0; sleep 0.05; done
    fail "$2 (last: $(ctl install 2>&1 | tail -1))"
}
gone() { # pattern, what
    for _ in $(seq 100); do ctl install | grep -q -- "$1" || return 0; sleep 0.05; done
    fail "$2"
}
flog() { # pattern, what (10 s)
    for _ in $(seq 200); do grep -q -- "$1" "$FLOG" && return 0; sleep 0.05; done
    fail "$2"
}

"$BIN" --backend headless --size 1600x900 --socket "$SOCK" --mcp none --settings none --fixed-status --installer "$ISOCK" >"$RUN/bitscomp.log" 2>&1 &
PIDS+=($!)
for _ in $(seq 50); do [ -S "$XDG_RUNTIME_DIR/$SOCK.ctl" ] && break; sleep 0.1; done
python3 -I "$ROOT/host/tests/mcp/access_stand_in.py" "$ISOCK" "$FLOG" "$FP" >"$RUN/stand-in.out" 2>&1 &
PIDS+=($!)
flog "confirmer connected" "bitscomp did not connect to the stand-in"

echo "== 11: the question, Erlauben preselected with 24 h; injected Enter goes out as injected"
wait_for "access-dialog id=11 client=cdcdcdcdcdcdcdcd hours=24 choice=24 nvme=false focus=Yes" "no access dialog 11 as asked"
ctl install | grep -q 'title="Vollzugriff für den Gaming-PC (bitsos-desk, cdcd cdcd cdcd cdcd) bis' || fail "the title: $(ctl install)"
key 15 105
ctl install | grep -q "focus=Yes" || fail "injected Tab/Left moved the focus: $(ctl install)"
key 28
flog "answer 11 type=access input=injected hours=24 nvme=False client_ok=True" "the injected Enter was not sent as injected"
gone "access-dialog id=11" "dialog 11 stayed open"

echo "== 12: a real Enter only once armed; real keys before the arming start it again"
wait_for "access-dialog id=12" "no access dialog 12"
phys key 28
sleep 1.5
ctl install | grep -q "access-dialog id=12" || fail "the first real Enter (not armed) answered"
phys key 28
sleep 1.5
# Without the re-arming Erlauben would be armed by now (2 s after the first key); with it, not before 2 s after the second.
ctl install | grep -q "access-dialog id=12.*armed=false" || fail "a real key before the arming did not start it again: $(ctl install)"
grep -q "answer 12" "$FLOG" && fail "an answer went out before Erlauben was armed"
wait_for "access-dialog id=12.*armed=true" "Erlauben never armed"
phys key 28
flog "answer 12 type=access input=physical hours=24 nvme=False client_ok=True" "the real Enter did not grant 24 h without NVMe"
gone "access-dialog id=12" "dialog 12 stayed open"

echo "== the live grant: the chip; Strg+Alt+V injected opens nothing, real opens Georg's own dialog"
flog "grant shown" "the stand-in did not show the grant"
wait_for 'marker="Fernzugriff offen bis [0-9][0-9]:[0-9][0-9]' "no chip \"Fernzugriff offen bis HH:MM\""
key +29 +56 47 -56 -29
sleep 0.2
ctl install | grep -q "access-dialog" && fail "injected Strg+Alt+V opened the access dialog"
phys key +29 +56 47 -56 -29
wait_for "access-dialog id=own" "a real Strg+Alt+V did not open the access dialog"
ctl install | grep -q " revoke=" || fail "no \"Fernzugriff beenden\" while a grant runs: $(ctl install)"
phys key 103 103
ctl install | grep -q "focus=Revoke" || fail "Up twice from Erlauben is not on Fernzugriff beenden: $(ctl install)"
phys key 28
flog "revoked" "Fernzugriff beenden sent no revoke"
flog "DONE" "the stand-in did not finish"
gone 'marker="Fernzugriff offen' "the chip stayed after the revoke"
echo "OK: mc 97-access-dialog"
