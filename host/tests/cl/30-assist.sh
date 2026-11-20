#!/bin/bash
# slow
# CL-3a on V2 (QEMU): the native assistant client `assist` (in the image, run as uid 1000 through MCP `run`)
# against a fake AI provider on the host (assist_fake_api.py: all three protocols from recorded answers; no real
# provider is contacted). assist --once gets a text answer from each provider kind; in one conversation the model
# calls settings_set (allowed without asking for the test) and the setting really changes, in the next
# settings_undo brings it back; in one the model asks the machine for its version through the kernel's local
# tools socket (CL-T) and gets it, and an assistant whose settings socket was named from outside says that only
# the settings tools are there; a client on the pipes of `assist --mcp` gets the list and the machine's version;
# a provider of kind program (a helper script) answers, and one that hangs is ended; a helper of version 2
# (CL-6) asks at the assistant's bell before its tool runs and hands BitsOS's tools on (`assist --mcp --bell`);
# EINS K2/K3: one level `ai.level` (Automodus until Georg chooses less, DECISIONS 18.11.; the scenario tightens it to
# Fragt vorher, which is anyone's) decides for every tool, the native tools and the helper's own alike, Nur antworten offers only what reads, and only root at the console loosens it;
# "Nur lokal" refuses a cloud provider before any connection; `assist serve` (CL-3s) makes its socket for the
# user alone, turns away every program that is not the session's compositor and does not start twice; a name on
# the list `ai.providers` makes a provider's keys, `assist --test` asks one provider once, a name taken off
# removes the provider and its key after a yes (CL-5s); that yes is only Georg's own (INT-CL-4): `settings
# --confirm` as uid 1000 is refused for a provider's removal and for its key, root at the console (typed
# through QMP on a USB keyboard) removes it; the key shows nowhere.
# Ports: lane range + 40 (MCP forward), + 42 (fake provider).
set -eu
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
ROOT="$(pwd)"
for t in python3 mtype; do command -v $t >/dev/null || { echo "RED: $t missing"; exit 1; }; done
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/cl-30-assist}"
mkdir -p "$RUN_DIR"
RUN_DIR="$(realpath "$RUN_DIR")"
LO="${BITSOS_PORT_LO:-30000}"
FWD=$((LO + 40))
FAKE=$((LO + 42))
TOKEN="cl3a-scenario-token-5e7a"
KEY="sk-fake-qemu-8a2f-not-a-real-key"
LOG="$RUN_DIR/serial.log"
QMP="$RUN_DIR/qmp.sock"
NAME=cl-assist
rm -f "$RUN_DIR/fake.log" "$LOG" "$QMP"

FPID=""
QPID=""
stop() {
    [ -n "$FPID" ] && kill "$FPID" 2>/dev/null || true
    [ -n "$QPID" ] && kill "$QPID" 2>/dev/null || true
    pf="target/xtask/qemu-$NAME.pid"
    if [ -f "$pf" ]; then
        pid="$(head -1 "$pf")"
        if tr '\0' ' ' < "/proc/$pid/cmdline" 2>/dev/null | grep -Eq -- "-name v2-[A-Za-z0-9]+-$NAME( |$)"; then kill "$pid" 2>/dev/null || true; fi
    fi
    [ -n "$QPID" ] && wait "$QPID" 2>/dev/null || true
}
trap stop EXIT

python3 -I host/tests/cl/assist_fake_api.py "$FAKE" "$KEY" "$RUN_DIR/fake.log" > "$RUN_DIR/fake.out" 2>&1 &
FPID=$!
for _ in $(seq 50); do grep -q "fake AI provider" "$RUN_DIR/fake.out" 2>/dev/null && break; sleep 0.1; done
grep -q "fake AI provider" "$RUN_DIR/fake.out" || { cat "$RUN_DIR/fake.out"; echo "RED: the fake provider did not start (port $FAKE busy?)"; exit 1; }

# The image holds the user workspace's programs, assist among them (static musl, /bin/assist).
cargo xtask image --name "$NAME" --config-extra "token=$TOKEN;mcp_port=8765;loglevel=info;nvme_write=off"
# The USB keyboard and QMP are for the one step root does at the console (as pk/50-apps-on-data.sh).
cargo xtask qemu --name "$NAME" --net virtio --hostfwd "$FWD:8765" --timeout 600 --serial-log "$LOG" \
    -- -machine i8042=off -device qemu-xhci,id=xb -device usb-kbd,bus=xb.0,port=1,id=kbd1 -qmp "unix:$QMP,server=on,wait=off" \
    > "$RUN_DIR/qemu.out" 2>&1 &
QPID=$!
python3 -I host/tests/cl/assist_qemu_check.py "$FWD" "$TOKEN" "$FAKE" "$KEY" "$RUN_DIR/fake.log" "$RUN_DIR" "$QMP"
if grep -aq "PANIC" "$LOG"; then echo "RED: the kernel panicked"; tail -20 "$LOG"; exit 1; fi
if grep -aqF "$KEY" "$LOG"; then echo "RED: the key is in the kernel log"; grep -aF "$KEY" "$LOG" | cut -c1-200 | head -3; exit 1; fi
echo "ok: the key is not in the kernel log"
# CL-T: the kernel wrote the assistant's call down, and never turned the assistant away.
grep -aq "mcp local.call .*uid=1000 tool=version outcome=done" "$LOG" \
    || { echo "RED: no local.call event for the assistant's version call"; grep -a "mcp local\." "$LOG" | tail -5; exit 1; }
# The run provokes two refusals on purpose (an assistant in a namespace of its own, another program); any
# other reason means the door turned the real assistant away.
if grep -a "mcp local.refused" "$LOG" | grep -v "namespace of its own" | grep -vq "does not run the boot's /bin/assist"; then echo "RED: the local tools socket refused the assistant"; grep -a "mcp local.refused" "$LOG" | tail -3; exit 1; fi
echo "ok: the kernel logged the assistant's call of version at the local tools socket"
# INT-CL-4: the refused confirmations are written down with the program the kernel named, the removal root made too.
grep -aq "settingsd confirm.refused what=remove key=ai.providers .*uid=1000 exe=/bin/settings by=kernel" "$LOG" \
    || { echo "RED: no confirm.refused event for the removal a command line of uid 1000 confirmed"; grep -a "settingsd" "$LOG" | tail -5; exit 1; }
grep -aq "settingsd confirm.refused what=set key=ai.provider.g1.key .*uid=1000 exe=/bin/settings by=kernel" "$LOG" \
    || { echo "RED: no confirm.refused event for the key a command line of uid 1000 tried to clear"; grep -a "settingsd" "$LOG" | tail -5; exit 1; }
grep -aq "settingsd provider.removed provider=g1 key_stored=true" "$LOG" \
    || { echo "RED: no provider.removed event for root's removal"; grep -a "settingsd" "$LOG" | tail -5; exit 1; }
echo "ok: the refused confirmations and root's removal are in the log"
# CL-S: the refused request is written down with the program the kernel named.
grep -aq "owner.refused what=get key=ai.provider.a1.key owner=assist .*uid=1000 exe=/bin/settings" "$LOG" \
    || { echo "RED: no owner.refused event for the stranger's request"; grep -a "settingsd" "$LOG" | tail -5; exit 1; }
echo "ok: the refused request is in the log with uid and program"
# INT-CL3T: the program was told by the kernel's record of connect; the try to move a stored key is written
# down; the assistant in a namespace of its own was turned away at both doors, by name.
grep -aq "owner.refused what=get key=ai.provider.a1.key owner=assist .*uid=1000 exe=/bin/settings by=kernel" "$LOG" \
    || { echo "RED: the stranger was not told by the kernel's record (by=kernel)"; grep -a "owner.refused" "$LOG" | tail -3; exit 1; }
grep -aq "redirect.refused key=ai.provider.a1.address provider=a1 op=set .*uid=1000 exe=/bin/settings by=kernel" "$LOG" \
    || { echo "RED: no redirect.refused event for the try to move the provider's address"; grep -a "settingsd" "$LOG" | tail -5; exit 1; }
grep -aq "peer.unknown what=get key=ai.provider.a1.key owner=assist .*exe=- by=kernel" "$LOG" \
    || { echo "RED: no peer.unknown event for the assistant in a namespace of its own"; grep -a "settingsd peer\." "$LOG" | tail -5; exit 1; }
grep -a "mcp local.refused" "$LOG" | grep -q "runs /bin/assist in a user or mount namespace of its own" \
    || { echo "RED: the local tools socket did not turn the assistant in a namespace away by name"; grep -a "mcp local\." "$LOG" | tail -5; exit 1; }
echo "ok: the kernel's record names who asks; a stored key stays; a namespace's assistant gets neither key nor tools"
# CL-5s: the removal of a provider is written down, without any value.
grep -aq "settingsd provider.removed provider=g1 key_stored=true" "$LOG" \
    || { echo "RED: no provider.removed event for the provider taken off the list"; grep -a "settingsd provider" "$LOG" | tail -5; exit 1; }
echo "ok: the removal of the provider is in the log"
stop
trap - EXIT
# On the stick the key sits in settingsd's file for the ai keys and nowhere else: not in the journal.
IMG="target/xtask/$NAME.img"
mtype -i "$IMG" ::BITSOS2/settings/journal.log > "$RUN_DIR/journal.log" 2>&1 || { cat "$RUN_DIR/journal.log"; echo "RED: no journal on the image"; exit 1; }
if grep -qF "$KEY" "$RUN_DIR/journal.log"; then echo "RED: the key is in settingsd's journal"; exit 1; fi
grep -q "ai.provider.a1.key" "$RUN_DIR/journal.log" || { echo "RED: the journal does not mention the key change at all"; exit 1; }
echo "ok: the journal records that the key changed, without its value"
# CL-5s: the removed provider left settingsd's file and its journal; the others are still there.
mtype -i "$IMG" ::BITSOS2/settings/ai.toml > "$RUN_DIR/ai.toml" 2>&1 || { cat "$RUN_DIR/ai.toml"; echo "RED: no ai.toml on the image"; exit 1; }
if grep -q "provider\.g1\." "$RUN_DIR/ai.toml" "$RUN_DIR/journal.log"; then echo "RED: the removed provider g1 is still in settingsd's files"; rm -f "$RUN_DIR/ai.toml"; exit 1; fi
grep -q "provider\.a1\.address" "$RUN_DIR/ai.toml" || { echo "RED: provider a1 is missing from ai.toml"; rm -f "$RUN_DIR/ai.toml"; exit 1; }
# The copy holds the fake key of the other providers: it does not stay in the run directory.
rm -f "$RUN_DIR/ai.toml"
echo "ok: the removed provider left ai.toml and the journal"
echo "assist on V2 (QEMU) against the fake provider: green"
