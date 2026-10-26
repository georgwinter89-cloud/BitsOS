# Shared by the MC scenarios (sourced, not a scenario itself).
#
# `cargo xtask qemu --name NAME` runs qemu-system as its child and keeps the child's PID in
# target/xtask/qemu-NAME.pid while it runs. Killing the xtask wrapper alone leaves QEMU running until its own
# timeout, holding the hostfwd port and the QMP socket, so the next run of the scenario cannot start.

# PID of the QEMU this lane started under NAME, if it still runs (checked against its -name, never another process).
qemu_pid() {
    local name="$1" pf pid
    pf="target/xtask/qemu-$name.pid"
    [ -f "$pf" ] || return 0
    pid="$(head -1 "$pf" 2>/dev/null || true)"
    case "$pid" in ''|*[!0-9]*) return 0 ;; esac
    if tr '\0' ' ' < "/proc/$pid/cmdline" 2>/dev/null | grep -Eq -- "-name v2-[A-Za-z0-9]+-$name( |$)"; then
        echo "$pid"
    fi
}

# Stops the QEMU of NAME by its PID (TERM, then KILL after 5 s) and removes the stale pidfile.
stop_qemu() {
    local name="$1" pid
    pid="$(qemu_pid "$name")"
    if [ -n "$pid" ]; then
        kill "$pid" 2>/dev/null || true
        for _ in $(seq 1 50); do kill -0 "$pid" 2>/dev/null || break; sleep 0.1; done
        if kill -0 "$pid" 2>/dev/null; then kill -9 "$pid" 2>/dev/null || true; fi
    fi
    rm -f "target/xtask/qemu-$name.pid"
}

# Stops the xtask wrapper WPID and then its QEMU NAME (for `trap ... EXIT`).
stop_run() {
    local wpid="$1" name="$2"
    if kill -0 "$wpid" 2>/dev/null; then kill "$wpid" 2>/dev/null || true; fi
    stop_qemu "$name"
    wait "$wpid" 2>/dev/null || true
}
