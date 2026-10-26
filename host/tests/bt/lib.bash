# Helpers for the BT scenario scripts (sourced, not run: the runner only runs *.sh).
# Images live in target/xtask/<name>.img (built by `cargo xtask image`); they are changed only through mtools
# on those image files. QEMU is started only through `cargo xtask qemu`.
set -eu
ROOT="${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
cd "$ROOT"
export CARGO_TARGET_DIR="$ROOT/target"
export BITSOS_LANE="${BITSOS_LANE:-bt}"
export MTOOLS_SKIP_CHECK=1
RUN_DIR="${BITSOS_RUN_DIR:-$ROOT/target/xtask/runs/bt-$(basename "$0" .sh)}"
mkdir -p "$RUN_DIR"
RUN_DIR="$(realpath "$RUN_DIR")"
X="$ROOT/target/xtask"
FAILED=0

# A ktest image whose kernel runs one quick contract test and exits QEMU with PASS.
# $1 = image name, $2 = extra config lines ("k=v;k2=v2", may be empty)
make_image() {
    local extra="ktest=contract.frame_alloc"
    [ -n "${2:-}" ] && extra="$extra;$2"
    cargo xtask image --name "$1" --ktest --config-extra "$extra" >"$RUN_DIR/image-$1.txt" 2>&1 \
        || { cat "$RUN_DIR/image-$1.txt"; echo "RED: cannot build image $1"; exit 1; }
}

img() { echo "$X/$1.img"; }

# $1 = image name, $2 = run tag (names the log), $3 = timeout s, rest = xtask qemu options.
# Never fails: the checks decide. Up to 9 boot retries: on the Gaming-PC OVMF stalled before bitsboot in about
# half of all boots on 15.10. (usb-storage on qemu-xhci, independent of -smp and -m; xtask detects it).
boot() {
    local name="$1" tag="$2" t="$3"; shift 3
    local log="$RUN_DIR/$tag.log"
    echo "--- boot $tag: image $name ${*:-}"
    cargo xtask qemu --name "$name" --boot-retries 9 --timeout "$t" --serial-log "$log" "$@" >"$RUN_DIR/$tag.out" 2>&1 || true
    grep -a "^xtask: " "$RUN_DIR/$tag.out" | tail -1 || true
}

logf() { echo "$RUN_DIR/$1.log"; }

# $1 = run tag, $2 = fixed string that must be in the serial log, $3 = what it proves
want() {
    if grep -aqF -- "$2" "$(logf "$1")"; then
        echo "  ok: $3"
    else
        echo "  RED: $3 (missing: $2)"
        FAILED=1
    fi
}

# Same with an extended regular expression.
want_re() {
    if grep -aqE -- "$2" "$(logf "$1")"; then
        echo "  ok: $3"
    else
        echo "  RED: $3 (no line matches: $2)"
        FAILED=1
    fi
}

refuse() {
    if grep -aqF -- "$2" "$(logf "$1")"; then
        echo "  RED: $3 (found: $2)"
        FAILED=1
    else
        echo "  ok: $3"
    fi
}

# The kernel came up through bitsboot and its ktest passed.
want_kernel_ok() {
    want "$1" "bitsboot: ExitBootServices done" "bitsboot handed over"
    want "$1" "BitsOS V2 kernel: entry" "the kernel started"
    want "$1" "KTEST done passed=1 failed=0" "the kernel's ktest passed (it read CONFIG.TXT)"
}

# bitsboot stopped with its readable error screen and went back to the firmware.
want_refusal() {
    want "$1" "BitsOS cannot start." "the error screen is shown"
    want_re "$1" "bitsboot: gave [0-9]+ pages back; returning to the firmware" "bitsboot freed its memory and returned to the firmware"
    want "$1" ": Load Error" "the firmware went on with its boot order"
    refuse "$1" "BitsOS V2 kernel: entry" "no kernel was started"
}

finish() {
    if [ "$FAILED" = 0 ]; then
        echo "bt $(basename "$0" .sh): green"
    else
        echo "bt $(basename "$0" .sh): RED (logs in $RUN_DIR)"
        exit 1
    fi
}

# Generation markers in an image (mtools only). $1 = image name, $2 = generation, $3... = marker names.
mark() {
    local im="$1" n="$2"; shift 2
    : >"$RUN_DIR/empty-marker"
    for m in "$@"; do
        mcopy -o -i "$(img "$im")" "$RUN_DIR/empty-marker" "::/BITSOS2/SYS/$n/$m"
    done
}

# A copy of generation $3 of image $1 as generation $2 (KERNEL + INITRD), no markers. The kernel copy loses
# its debug sections (objcopy --strip-debug: the debug ktest kernel is ~22 MB, the 64 MB test stick holds two).
copy_gen() {
    local im="$1" to="$2" from="${3:-1}"
    mkdir -p "$RUN_DIR/gen-copy"
    mcopy -o -i "$(img "$im")" "::/BITSOS2/SYS/$from/KERNEL" "::/BITSOS2/SYS/$from/INITRD" "$RUN_DIR/gen-copy/"
    objcopy --strip-debug "$RUN_DIR/gen-copy/KERNEL" 2>/dev/null || true  # a broken ELF stays as it is
    mmd -i "$(img "$im")" "::/BITSOS2/SYS/$to" 2>/dev/null || true
    mcopy -o -i "$(img "$im")" "$RUN_DIR/gen-copy/KERNEL" "$RUN_DIR/gen-copy/INITRD" "::/BITSOS2/SYS/$to/"
}

# The files of one generation directory, upper case, sorted, one line ("INITRD KERNEL TRY").
gen_files() {
    mdir -b -i "$(img "$1")" "::/BITSOS2/SYS/$2" 2>/dev/null | sed 's|.*/||' | tr 'a-z' 'A-Z' | sort | tr '\n' ' ' | sed 's/ $//'
}

# $1 = image, $2 = generation, $3 = expected gen_files line, $4 = what it proves
want_files() {
    local got
    got="$(gen_files "$1" "$2")"
    if [ "$got" = "$3" ]; then
        echo "  ok: $4 ($got)"
    else
        echo "  RED: $4 (want '$3', got '$got')"
        FAILED=1
    fi
}

# Every file of the image with its checksum (path and sha256), for "nothing else changed" checks.
snapshot() {
    local d="$RUN_DIR/snap-$2"
    rm -rf "$d"
    mkdir -p "$d"
    mcopy -s -n -i "$(img "$1")" "::/*" "$d/" 2>/dev/null
    (cd "$d" && find . -type f -print0 | sort -z | xargs -0 sha256sum)
}

# The kernel's GENERATION record as bitsboot logged it: $1 = run tag, $2 = number, $3 = reason, $4 = skipped,
# $5 = flags ("-" = none), $6 = what it proves
want_gen() {
    want "$1" "generation.chosen number=$2 reason=$3 skipped=$4 flags=$5:" "$6"
    want "$1" "kernel start version=0.1.0 generation=$2 " "the kernel got generation $2 in its GENERATION record"
}
