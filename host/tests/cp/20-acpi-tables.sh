#!/bin/bash
# CP-2: the kernel reads q35's ACPI tables plus two QEMU adds a test cannot fake in a host test: an SRAT with
# two NUMA domains (-numa) and an extra table passed with -acpitable. Runs the cp.acpi ktests on that machine.
# Needs the kernel hook `acpi::init(&bi)` (desk change request CP-2/1); without it the script says so and passes.
set -eu
cd "$BITSOS_ROOT"
RUN="$BITSOS_RUN_DIR"
if ! grep -q 'acpi::init(&bi)' kernel/src/main.rs; then
    echo "SKIP: kernel/src/main.rs does not call acpi::init(&bi) yet (desk change request CP-2/1)"
    exit 0
fi
NAME="$BITSOS_LANE-acpi"
printf 'BitsOS CP-2 test table\0' > "$RUN/bits.bin"
cargo xtask image --name "$NAME" --ktest --config-extra "ktest=cp.acpi"
cargo xtask qemu --name "$NAME" --timeout 120 --serial-log "$RUN/serial.log" -- \
    -object memory-backend-ram,size=1G,id=m0 -object memory-backend-ram,size=1G,id=m1 \
    -numa node,memdev=m0,cpus=0,nodeid=0 -numa node,memdev=m1,cpus=1,nodeid=1 \
    -acpitable "sig=BITS,rev=1,oem_id=BITSOS,oem_table_id=CP2TEST,data=$RUN/bits.bin" || true
log="$RUN/serial.log"
touch "$log"
fail=0
check() { if grep -aq -- "$1" "$log"; then echo "ok   $2"; else echo "FAIL $2 (no '$1')"; fail=1; fi; }
check 'KTEST done passed=7 failed=0 skipped=0' "all cp.acpi ktests pass, none skipped"
check 'acpi tables .*list="[^"]*BITS' "the -acpitable table is listed"
check 'acpi srat domains=2 cpus=2' "SRAT with two domains and two CPUs"
check 'acpi fadt revision=' "FADT read"
check 'acpi s5 typ_a=' "_S5 found"
if grep -aq 'acpi table.problem' "$log"; then echo "FAIL q35 tables gave problems:"; grep -a 'acpi table.problem' "$log"; fail=1; fi
exit $fail
