#!/bin/bash
# BT-1/BT-2: bitsboot's firmware-free logic on the host (marker rule over every marker combination, mode
# choice), plus source rules a test boot cannot show: no UEFI variable writes or runtime services, files only
# through the boot volume's own file system, and writes only in markers.rs behind the marker path check.
. "$(dirname "$0")/lib.bash"

echo "--- cargo test -p bitsboot (host)"
cargo test -q -p bitsboot 2>&1 | tail -3
cargo test -q -p bitsboot >/dev/null 2>&1 || { echo "RED: host tests"; exit 1; }

echo "--- source rules"
SRC=boot/bitsboot/src
check_absent() { # $1 = regex, $2 = rule
    if grep -rnE -- "$1" "$SRC"; then
        echo "  RED: $2"
        FAILED=1
    else
        echo "  ok: $2"
    fi
}
check_absent 'set_variable|SetVariable|set_var\b|runtime::' "no UEFI variable writes, no runtime services (R8g)"
check_absent 'locate_handle_buffer|find_handles|LocateHandle|get_image_file_system|SimpleFileSystem>\(\)' \
    "no search over other volumes: files come only from the loaded image's own device"
# Only markers.rs may open for writing, and it checks every path with gens::is_marker_path first.
if grep -rnE -- 'FileMode::(Read|Create)Write|FileMode::Create|\.delete\(|set_info' "$SRC" | grep -v "^$SRC/markers.rs:"; then
    echo "  RED: writes outside markers.rs"
    FAILED=1
else
    echo "  ok: only markers.rs writes files"
fi
if grep -q 'is_marker_path(&tried)' "$SRC/markers.rs" && ! grep -qE '\.delete\(|set_info' "$SRC/markers.rs"; then
    echo "  ok: markers.rs checks its path below \\BITSOS2\\SYS\\ and neither deletes nor renames"
else
    echo "  RED: markers.rs lost its path check, deletes or renames files"
    FAILED=1
fi
echo "--- cargo test -p xtask stick (host: the stick tool on stand-in folders)"
cargo test -q -p xtask stick 2>&1 | grep -E "^test result" || true
cargo test -q -p xtask stick >/dev/null 2>&1 || { echo "  RED: stick host tests"; FAILED=1; }
# The stick tool starts only these programs: no mkfs, dd, partitioning, labelling or device set-up (BT-3).
STICK_SRC="host/xtask/src/stick.rs host/xtask/src/stick"
bad="$(grep -rhoE 'Command::new\("[^"]+"\)' $STICK_SRC | sed -E 's/.*\("([^"]+)"\)/\1/' | sort -u | grep -vxE 'lsblk|stat|sync|sha256sum|date' || true)"
if [ -n "$bad" ]; then
    echo "  RED: the stick tool starts programs outside its list: $bad"
    FAILED=1
else
    echo "  ok: the stick tool starts only lsblk, stat, sync, sha256sum and date"
fi
# One exception: retire.rs reads files read-only with O_DIRECT for the backup compare (past the page cache).
if grep -rnE 'OpenOptions|custom_flags|O_DIRECT|"/dev/' $STICK_SRC | grep -v '/tests.rs:' | grep -vE '^\S+:[0-9]+:\s*(//|const MOUNTS)' | grep -vE 'starts_with\("/dev/"\)|"/dev/(nvme0n1p2|sdb1)' \
    | grep -vE '^host/xtask/src/stick/retire\.rs:[0-9]+:(use std::os::unix::fs::OpenOptionsExt;|const O_DIRECT: i32 = 0o40000;|    match File::options\(\)\.read\(true\)\.custom_flags\(O_DIRECT\)\.open\(p\) \{|\s+"[^"]*O_DIRECT[^"]*",?)$'; then
    echo "  RED: the stick tool opens files with special flags or names a device path"
    FAILED=1
else
    echo "  ok: the stick tool opens no device; the only special open is retire.rs's read-only O_DIRECT"
fi
if [ "$(grep -c 'custom_flags' host/xtask/src/stick/retire.rs)" != 1 ] || grep -rn 'custom_flags' $STICK_SRC | grep -v '/retire.rs:' | grep -q .; then
    echo "  RED: custom_flags appears outside retire.rs's one read-only open"
    FAILED=1
fi
finish
