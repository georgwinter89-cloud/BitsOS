#!/bin/bash
# slow
# SC-9: the measuring build (--cfg bitsos_fd_measure, FD-1) leaves nothing in a release kernel without the cfg.
# Two lean release kernels (fat LTO, one codegen unit, --cfg bitsos_no_ktests, the kernel target flags of
# .cargo/config.toml), one with the cfg:
#  1. both build;
#  2. the plain one has no hook symbol and no hook string, the measuring one has them (so the check sees them);
#  3. the syscall entry, the user trap/execute stubs and all interrupt vector stubs disassemble identically.
# Builds in target dirs of its own, so the gate's target is not rebuilt.
set -eu
cd "$BITSOS_ROOT"
RUN="$BITSOS_RUN_DIR"
fail=0
ok() { echo "ok   $1"; }
bad() { echo "FAIL $1"; fail=1; }

# RUSTFLAGS replaces the target rustflags of .cargo/config.toml, so they are repeated here.
BASE="-C relocation-model=static -C code-model=kernel -C force-frame-pointers=yes"
BASE="$BASE -C target-feature=+popcnt,+lzcnt,+bmi1,+bmi2,+movbe,+cmpxchg16b --cfg bitsos_no_ktests"
build() { # tag, extra rustflags
    local dir="$BITSOS_ROOT/target/fdcmp-$1"
    if CARGO_TARGET_DIR="$dir" RUSTFLAGS="$BASE $2" CARGO_PROFILE_RELEASE_LTO=fat CARGO_PROFILE_RELEASE_CODEGEN_UNITS=1 \
        cargo build -q -p kernel --target x86_64-unknown-none --release > "$RUN/build-$1.txt" 2>&1; then
        cp "$dir/x86_64-unknown-none/release/kernel" "$RUN/kernel-$1"
        ok "the $1 lean release kernel builds"
        # CONTRACTS §4 (Georg 29.10.): no compiler warning in any cfg the gate builds, the lean ones included.
        if grep -q '^warning' "$RUN/build-$1.txt"; then
            bad "the $1 lean release kernel builds with warnings (build-$1.txt)"
            grep -A3 '^warning' "$RUN/build-$1.txt" | head -20
        else
            ok "the $1 lean release kernel builds without a warning"
        fi
    else
        bad "the $1 lean release kernel does not build (build-$1.txt)"
        tail -20 "$RUN/build-$1.txt"
    fi
}
build plain ""
build measure "--cfg bitsos_fd_measure"
P="$RUN/kernel-plain" M="$RUN/kernel-measure"
[ -f "$P" ] && [ -f "$M" ] || { echo "FAIL no kernels to compare"; exit 1; }

# 2. Hook symbols and strings.
SYMS='hal::prof::|proc::mcp_run::prof::|sched_mcp::idle_tool::|sched::cstate::measure::|hal::entry::steps::'
STRS='idle_mode|sched\.idle\.c1_entries|sched\.idle\.c2_ns'
np=$(nm -C "$P" | grep -cE "$SYMS" || true)
nm_=$(nm -C "$M" | grep -cE "$SYMS" || true)
sp=$(grep -acE "$STRS" "$P" || true)
sm=$(grep -acE "$STRS" "$M" || true)
[ "$np" = 0 ] && ok "plain: no hook symbol" || bad "plain: $np hook symbols"
[ "$sp" = 0 ] && ok "plain: no hook string" || bad "plain: $sp hook strings"
[ "$nm_" -gt 0 ] && [ "$sm" -gt 0 ] && ok "measure: $nm_ hook symbols, $sm hook strings (the check sees them)" \
    || bad "measure: $nm_ hook symbols, $sm hook strings (expected some)"

# 3. The asm stubs: addresses, rip offsets and branch targets normalized.
dis() { # kernel, symbol
    objdump -d -M intel --no-show-raw-insn --disassemble="$2" "$1" | sed -n '/^[0-9a-f]* <'"$2"'>:/,/^$/p' \
        | sed -E 's/^ *[0-9a-f]+:\t//; s/0x[0-9a-f]+//g; s/[0-9a-f]{16}//g; s/<[^>]*>//g'
}
for s in hal_syscall_entry hal_trap_from_user hal_user_execute hal_vec_base; do
    dis "$P" "$s" > "$RUN/p-$s.s"
    dis "$M" "$s" > "$RUN/m-$s.s"
    n=$(grep -c . "$RUN/p-$s.s" || true)
    if [ "$n" -lt 3 ]; then
        bad "$s: not found in the plain kernel"
    elif cmp -s "$RUN/p-$s.s" "$RUN/m-$s.s"; then
        ok "$s: $n lines, identical with and without the cfg"
    else
        bad "$s differs with the cfg ($(diff "$RUN/p-$s.s" "$RUN/m-$s.s" | grep -c '^[<>]') lines)"
    fi
done
exit $fail
