#!/bin/bash
# slow
# MC-push: host/mcp/push.mjs against a real BitsOS V2 under QEMU (KVM, virtio-net, the stick on usb-storage, an
# image file of this run only), with a 50 MB package tree made here:
#  1. push_plan.mjs (the pure half) and push_wait.mjs (waiting for a "Still running" call, fake server);
#  2. refusals before anything is asked: --name SYS / home (without --home), --home for another package, --file
#     with a --to outside a package, in SYS or in home without --home, a tree with a symbolic link (exit 2);
#  3. --dry-run: exit 0, every file "would change", nothing on the stick;
#  4. the measurement: one 15 MB file with --via write (write_file pieces) into its own package, then removed;
#  5. an interrupted first push (BITSOS_PUSH_STOP_AFTER: half the files), a cut fetch's temp file put into the
#     staging tree; the package is still missing;
#     a staged file's first byte flipped (size kept);
#  6. the same command again: the staged half reused, the rest and the flipped file uploaded, the temp file
#     deleted, committed, equal;
#  7. one file changed, first served with a flipped byte (BITSOS_PUSH_CORRUPT: exit 1, no commit, the old file
#     stays), then right: only it moves ("1 files uploaded ... unchanged"); one file removed and one added with
#     --timeout-ms 1 (a call that outlives it is waited for with output);
#  8. an interrupted commit (BITSOS_PUSH_STOP_AT_COMMIT): the next run finishes it first, then is up to date;
# 8b. MC-push2: --file replaces one file of the package (merged) and makes one in a missing directory (moved
#     whole), the rest of the package untouched (the tree's dry run: 0 would change, 0 would be removed), the
#     same --file again is up to date; --home pushes a profile tree into \BITSOS2\home\ (uid/gid 1000, 0700
#     dirs, 0600 files at once), a second push replaces a changed file and keeps a file only the home has;
#     --file --home replaces one file there;
#  9. reboot over MCP, then --dry-run says up to date (the hashes now come from the disk, not the page cache), the
#     home's files still the user's (owner from the disk now, nothing cached);
# 10. on the host: fsck.fat -n clean, the package and the home on the image equal their trees, no .push-* left,
#     SYS, CONFIG.TXT and \EFI\BOOT\BOOTX64.EFI unchanged, the token never in the serial log.
# Ports: MCP hostfwd LO+180; push.mjs's server on 127.0.0.1 in LO+480..LO+489 (BITSOS_PUSH_PORTS; the guest
# fetches from 10.0.2.2, QEMU slirp's way to the host).
set -euo pipefail
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/mc-40-push}"
mkdir -p "$RUN_DIR"
RUN_DIR="$(realpath "$RUN_DIR")"
HERE="$(pwd)/host/tests/mcp"
# shellcheck source=host/tests/mcp/lib.bash
. "$HERE/lib.bash"
LO="${BITSOS_PORT_LO:-25000}"
FWD=$((LO + 180))
export BITSOS_PUSH_PORTS="$((LO + 480))-$((LO + 489))"
TOKEN="mcpush-scenario-8e41"
export BITSOS_TOKEN="$TOKEN"
NAME=mc-push
LOG="$RUN_DIR/serial.log"
SOCK="$RUN_DIR/qmp.sock"
fail() { echo "RED: $*"; exit 1; }

echo "== 1. push_plan.mjs"
node "$HERE/push_plan.mjs"
node "$HERE/push_wait.mjs" | tail -1

if ! grep -q "net::init" kernel/src/main.rs; then
    echo "SKIP: kernel/src/main.rs lacks net::init"
    exit 0
fi

WPID=""
NPID=""
cleanup() {
    if [ -n "$NPID" ]; then kill "$NPID" 2>/dev/null || true; fi
    if [ -n "$WPID" ]; then stop_run "$WPID" "$NAME"; fi
}
trap cleanup EXIT

# ---- the package tree: 50 MB in 2 big and 60 small files, 3 levels ----
T="$RUN_DIR/mcpushtest"
rm -rf "$T"
mkdir -p "$T/lib/deep" "$T/data"
head -c $((30 << 20)) /dev/urandom > "$T/lib/big1.bin"
head -c $((15 << 20)) /dev/urandom > "$T/data/big2.bin"
for i in $(seq 1 60); do head -c $(( (i * 1371) % 150000 + 1000 )) /dev/urandom > "$T/lib/deep/f$i.dat"; done
echo "#!/bin/sh" > "$T/run"
TOTAL=$(find "$T" -type f | wc -l)
echo "tree: $TOTAL files, $(du -sm "$T" | cut -f1) MB"

# ---- the image ----
cargo xtask image --name "$NAME" --config-extra "token=$TOKEN;mcp_port=8765;loglevel=info;wlan=off" | tail -1
ESP="target/xtask/esp-$NAME"
IMG="target/xtask/$NAME.img"
CONTENT=$(du -sb "$ESP" | cut -f1)
KIB=$(( (CONTENT * 2 + (256 << 20)) / 1024 / 1024 * 1024 ))
rm -f "$IMG"
touch "$ESP/BITSOS2/SYS/1/OK"
mkfs.fat -F 32 -n BITSOS -C "$IMG" "$KIB" > /dev/null
mcopy -s -i "$IMG" "$ESP/EFI" "$ESP/BITSOS2" ::/
mtype -i "$IMG" ::/EFI/BOOT/BOOTX64.EFI > "$RUN_DIR/loader.before"
mtype -i "$IMG" ::/BITSOS2/CONFIG.TXT > "$RUN_DIR/config.before"
mdir -/ -b -i "$IMG" ::/BITSOS2/SYS | sort > "$RUN_DIR/sys.before"
echo "image: $IMG ($((KIB / 1024)) MiB)"

stop_qemu "$NAME"
rm -f "$LOG" "$SOCK"
cargo xtask qemu --name "$NAME" --net virtio --mem 2048 --hostfwd "$FWD:8765" --timeout 1200 --serial-log "$LOG" \
    -- -action reboot=reset -qmp "unix:$SOCK,server,nowait" > "$RUN_DIR/qemu.out" 2>&1 &
WPID=$!
for _ in $(seq 100); do [ -S "$SOCK" ] && break; sleep 0.1; done
[ -S "$SOCK" ] || fail "no QMP socket"
python3 -I "$HERE/ovmf_nudge.py" "$LOG" "$SOCK" > "$RUN_DIR/nudge.log" 2>&1 &
NPID=$!
CALL() { node host/mcp/mcp-call.mjs "127.0.0.1:$FWD" "$@"; }
wait_mcp() {
    for i in $(seq 150); do
        CALL version > "$RUN_DIR/version.txt" 2>/dev/null && return 0
        sleep 1
    done
    fail "MCP does not answer on 127.0.0.1:$FWD within 150 s"
}
wait_mcp
echo "MCP: $(tail -1 "$RUN_DIR/version.txt")"
PUSH=(node host/mcp/push.mjs "127.0.0.1:$FWD" --bind 127.0.0.1)
bits2() { CALL list_dir '{"path":"/boot/BITSOS2"}' | tail -n +2 | awk '{print $NF}' | tr '\n' ' '; }
expect_rc() {
    local want="$1" out="$2"; shift 2
    set +e
    "$@" > "$out" 2>&1
    local rc=$?
    set -e
    [ "$rc" = "$want" ] || { cat "$out"; fail "$* exited $rc, want $want"; }
}

echo "== 2. refusals before anything is asked"
expect_rc 2 "$RUN_DIR/r-sys.out" "${PUSH[@]}" --tree "$T" --name SYS
grep -q "is not a package" "$RUN_DIR/r-sys.out" || fail "SYS not refused by name"
expect_rc 2 "$RUN_DIR/r-home.out" "${PUSH[@]}" --tree "$T" --name home
grep -q "only with --home" "$RUN_DIR/r-home.out" || fail "home without --home not refused by name"
expect_rc 2 "$RUN_DIR/r-home2.out" "${PUSH[@]}" --tree "$T" --home
grep -q -- "--home is only for" "$RUN_DIR/r-home2.out" || fail "--home for another package not refused"
expect_rc 2 "$RUN_DIR/r-file1.out" "${PUSH[@]}" --file "$T/run" --to /boot/BITSOS2/run
grep -q "<package>/<file>" "$RUN_DIR/r-file1.out" || fail "--file into \\BITSOS2\\ itself not refused"
expect_rc 2 "$RUN_DIR/r-file2.out" "${PUSH[@]}" --file "$T/run" --to /boot/BITSOS2/home/run
grep -q "only with --home" "$RUN_DIR/r-file2.out" || fail "--file into home without --home not refused"
expect_rc 2 "$RUN_DIR/r-file3.out" "${PUSH[@]}" --file "$T/run" --to /boot/BITSOS2/sys/9/x
expect_rc 2 "$RUN_DIR/r-file4.out" "${PUSH[@]}" --file "$T" --to /boot/BITSOS2/mcpushtest/x
expect_rc 2 "$RUN_DIR/r-file5.out" "${PUSH[@]}" --file "$T/run" --tree "$T"
mkdir -p "$RUN_DIR/linked"
ln -sf /etc/hostname "$RUN_DIR/linked/l"
expect_rc 2 "$RUN_DIR/r-link.out" "${PUSH[@]}" --tree "$RUN_DIR/linked"
grep -q "a symbolic link" "$RUN_DIR/r-link.out" || fail "link not refused"
echo "ok: SYS, home without --home, wrong --home/--file uses and a link refused (exit 2)"

echo "== 3. dry run"
B0=$(bits2)
"${PUSH[@]}" --tree "$T" --dry-run | tee "$RUN_DIR/dry.out" | grep -v "^  "
grep -q "push: dry run: $TOTAL files would change, 0 would be removed; nothing was written" "$RUN_DIR/dry.out" || fail "dry run line"
[ "$(bits2)" = "$B0" ] || fail "the dry run changed \\BITSOS2\\: $(bits2)"
echo "ok: dry run, \\BITSOS2\\ unchanged ($B0)"

echo "== 4. measurement: 15 MB with write_file pieces"
W="$RUN_DIR/mcpushw"
rm -rf "$W"; mkdir -p "$W"; cp "$T/data/big2.bin" "$W/"
"${PUSH[@]}" --tree "$W" --via write | tee "$RUN_DIR/write.out" | grep "^upload:\|^push:"
grep -q "push: mcpushw: 1 files uploaded" "$RUN_DIR/write.out" || fail "write push"
CALL delete '{"path":"/boot/BITSOS2/mcpushw","recursive":true}' > /dev/null || fail "cannot delete mcpushw"

echo "== 5. an interrupted first push"
HALF=$((TOTAL / 2))
expect_rc 3 "$RUN_DIR/half.out" env BITSOS_PUSH_STOP_AFTER=$HALF "${PUSH[@]}" --tree "$T"
grep -q "stopped after $HALF files" "$RUN_DIR/half.out" || { cat "$RUN_DIR/half.out"; fail "no stop line"; }
CALL stat '{"path":"/boot/BITSOS2/mcpushtest"}' > /dev/null 2>&1 && fail "the package exists after an interrupted first push"
# What a fetch cut by a power loss leaves: a hidden temp sibling.
CALL write_file '{"path":"/boot/BITSOS2/.push-mcpushtest/files/lib/.big1.bin.mcp-77~","text":"partial","parents":true}' > /dev/null
# A staged file gone bad with its size kept (a flipped byte): only its hash can tell.
BAD=$(grep -m1 "^  upload " "$RUN_DIR/half.out" | awk '{print $2}')
[ -n "$BAD" ] || fail "no uploaded file in half.out"
B0=$(head -c1 "$T/$BAD" | od -An -tu1 | tr -d ' ')
FLIP=$(printf "\\x$(printf %02x $((B0 ^ 255)))" | base64)
CALL write_file "{\"path\":\"/boot/BITSOS2/.push-mcpushtest/files/$BAD\",\"data\":\"$FLIP\",\"offset\":0}" > /dev/null || fail "cannot flip a byte of $BAD"
echo "ok: stopped after $HALF of $TOTAL files, the package still missing; first byte of staged $BAD flipped"

echo "== 6. resume"
"${PUSH[@]}" --tree "$T" | tee "$RUN_DIR/resume.out" | grep -v "^  "
grep -q "push: mcpushtest: $((TOTAL - HALF + 1)) files uploaded .*, $((HALF - 1)) from an earlier run, 0 removed, 0 unchanged; .* equals the tree" "$RUN_DIR/resume.out" || fail "resume line"
grep -q "2 stale in the staging tree" "$RUN_DIR/resume.out" || fail "the temp file and the flipped file were not seen as stale"
grep -q "^  upload $BAD " "$RUN_DIR/resume.out" || fail "the flipped $BAD was not uploaded again"
grep -q "moved whole" "$RUN_DIR/resume.out" || fail "first push not moved whole"
CALL stat '{"path":"/boot/BITSOS2/.push-mcpushtest"}' > /dev/null 2>&1 && fail "staging tree left"
grep "^upload:" "$RUN_DIR/resume.out" "$RUN_DIR/write.out" | sed 's/^/measure: /'

echo "== 7. one file changed (first served wrong); one removed and one added"
OLD7=$(sha256sum "$T/lib/deep/f7.dat" | cut -d' ' -f1)
echo "changed" >> "$T/lib/deep/f7.dat"
# A wrong body of the right size: the staging check refuses the commit, the package keeps the old f7.dat.
expect_rc 1 "$RUN_DIR/corrupt.out" env BITSOS_PUSH_CORRUPT=lib/deep/f7.dat "${PUSH[@]}" --tree "$T"
grep -q "the staging tree differs from the plan in 1 files" "$RUN_DIR/corrupt.out" || { cat "$RUN_DIR/corrupt.out"; fail "the wrong body was not caught"; }
grep -q "^| move_tree" "$RUN_DIR/corrupt.out" && fail "move_tree ran after a failed staging check"
CALL manifest '{"path":"/boot/BITSOS2/mcpushtest/lib/deep"}' | grep -o '"path":"f7.dat"[^}]*' | grep -q "$OLD7" || fail "f7.dat on the stick is not the old one after the refused commit"
echo "ok: a wrong body caught by its hash, the old f7.dat kept"
"${PUSH[@]}" --tree "$T" | tee "$RUN_DIR/one.out" | grep -v "^  "
grep -q "push: mcpushtest: 1 files uploaded .*, 0 from an earlier run, 0 removed, $((TOTAL - 1)) unchanged" "$RUN_DIR/one.out" || fail "one-file line"
grep -q "^  upload lib/deep/f7.dat " "$RUN_DIR/one.out" || fail "not f7.dat uploaded"
[ "$(grep -c "^  upload " "$RUN_DIR/one.out")" = 1 ] || fail "more than one upload"
rm "$T/lib/deep/f8.dat"
echo "new" > "$T/data/new.txt"
# --timeout-ms 1: a call that outlives 1 ms + the server's 250 ms grace answers "Still running" and push waits
# for it with output (whether one does depends on speed; push_wait.mjs checks the waiting itself).
"${PUSH[@]}" --tree "$T" --timeout-ms 1 | tee "$RUN_DIR/addrm.out" | grep -v "^  "
grep "^wait: " "$RUN_DIR/addrm.out" | sed 's/^/note: /' || true
grep -q "push: mcpushtest: 1 files uploaded .*, 1 removed, $((TOTAL - 1)) unchanged" "$RUN_DIR/addrm.out" || fail "add/remove line"
grep -q "^  remove lib/deep/f8.dat$" "$RUN_DIR/addrm.out" || fail "f8 not removed"

echo "== 8. an interrupted commit"
echo "again" >> "$T/run"
expect_rc 3 "$RUN_DIR/cut.out" env BITSOS_PUSH_STOP_AT_COMMIT=1 "${PUSH[@]}" --tree "$T"
"${PUSH[@]}" --tree "$T" | tee "$RUN_DIR/finish.out" | grep -v "^  "
grep -q "commit: finishing the interrupted commit" "$RUN_DIR/finish.out" || fail "the commit was not finished"
grep -q "push: mcpushtest is up to date" "$RUN_DIR/finish.out" || fail "not up to date after finishing"

echo "== 8b. one file (--file) and the user's home (--home)"
# owner_is PATH UID MODE: stat on BitsOS says uid = gid = UID and the permission bits MODE.
owner_is() {
    CALL stat "{\"path\":\"$1\"}" > "$RUN_DIR/stat.out" 2>&1 || { cat "$RUN_DIR/stat.out"; fail "stat $1"; }
    local j
    j=$(tr -d ' \n' < "$RUN_DIR/stat.out")
    [[ "$j" == *"\"mode\":\"$3\""* && "$j" == *"\"uid\":$2,\"gid\":$2,"* ]] || fail "$1: want uid/gid $2 mode $3, got $j"
}
echo "again 2" >> "$T/run"
mkdir -p "$T/bin"
printf '#!/bin/sh\necho launch\n' > "$T/bin/launch"
"${PUSH[@]}" --file "$T/run" --to /boot/BITSOS2/mcpushtest/run | tee "$RUN_DIR/file1.out" | grep -v "^  "
grep -q "push: /boot/BITSOS2/mcpushtest/run: 1 files uploaded .*, 0 removed, 0 unchanged; .* equals the file" "$RUN_DIR/file1.out" || fail "--file over an existing file"
grep -q "merged into /boot/BITSOS2/mcpushtest and removed" "$RUN_DIR/file1.out" || fail "--file not merged"
"${PUSH[@]}" --file "$T/bin/launch" --to /boot/BITSOS2/mcpushtest/bin/launch | tee "$RUN_DIR/file2.out" | grep -v "^  "
grep -q "moved whole to /boot/BITSOS2/mcpushtest/bin" "$RUN_DIR/file2.out" || fail "--file into a missing directory not moved whole"
"${PUSH[@]}" --file "$T/bin/launch" --to /boot/BITSOS2/mcpushtest/bin/launch > "$RUN_DIR/file3.out"
grep -q "push: /boot/BITSOS2/mcpushtest/bin/launch is up to date" "$RUN_DIR/file3.out" || { cat "$RUN_DIR/file3.out"; fail "--file again not up to date"; }
owner_is /boot/BITSOS2/mcpushtest/bin/launch 0 0755
"${PUSH[@]}" --tree "$T" --dry-run > "$RUN_DIR/file4.out"
grep -q "push: dry run: 0 files would change, 0 would be removed" "$RUN_DIR/file4.out" || { cat "$RUN_DIR/file4.out"; fail "the package is not the tree after two --file pushes"; }
CALL stat '{"path":"/boot/BITSOS2/.push-mcpushtest"}' > /dev/null 2>&1 && fail "staging tree left after --file"
echo "ok: --file replaced one file and made one, the rest of the package untouched"
H="$RUN_DIR/home"
rm -rf "$H"
mkdir -p "$H/.cobalt/storage"
echo "profile a" > "$H/.cobalt/storage/a.db"
head -c 300000 /dev/urandom > "$H/.cobalt/cache.bin"
"${PUSH[@]}" --tree "$H" --name home --home --dry-run > "$RUN_DIR/home0.out"
grep -q "push: dry run: 2 files would change, 0 would be removed" "$RUN_DIR/home0.out" || { cat "$RUN_DIR/home0.out"; fail "--home dry run"; }
"${PUSH[@]}" --tree "$H" --name home --home | tee "$RUN_DIR/home1.out" | grep -v "^  "
grep -q "push: home: 2 files uploaded .*, 0 removed, 0 unchanged; /boot/BITSOS2/home holds the tree" "$RUN_DIR/home1.out" || fail "--home push"
grep -q "into the user's home" "$RUN_DIR/home1.out" || fail "move_tree did not say it went into the home"
for d in /boot/BITSOS2/home/.cobalt /boot/BITSOS2/home/.cobalt/storage; do owner_is "$d" 1000 0700; done
for f in /boot/BITSOS2/home/.cobalt/storage/a.db /boot/BITSOS2/home/.cobalt/cache.bin; do owner_is "$f" 1000 0600; done
CALL write_file '{"path":"/boot/BITSOS2/home/georg-keep.txt","text":"mine"}' > /dev/null || fail "cannot write into the home"
echo "profile b" > "$H/.cobalt/storage/a.db"
"${PUSH[@]}" --tree "$H" --name Home. --home | tee "$RUN_DIR/home2.out" | grep -v "^  "
grep -q "push: home: 1 files uploaded .*, 0 removed, 1 unchanged; .*files only on BitsOS kept" "$RUN_DIR/home2.out" || fail "second --home push"
grep -q "^  remove " "$RUN_DIR/home2.out" && fail "--home planned a removal"
CALL stat '{"path":"/boot/BITSOS2/home/georg-keep.txt"}' > /dev/null 2>&1 || fail "--home removed a file only the home had"
owner_is /boot/BITSOS2/home/.cobalt/storage/a.db 1000 0600
head -c 1000 /dev/urandom >> "$H/.cobalt/cache.bin"
"${PUSH[@]}" --file "$H/.cobalt/cache.bin" --to /boot/BITSOS2/home/.cobalt/cache.bin --home | tee "$RUN_DIR/home3.out" | grep -v "^  "
grep -q "push: /boot/BITSOS2/home/.cobalt/cache.bin: 1 files uploaded .* equals the file" "$RUN_DIR/home3.out" || fail "--file --home"
owner_is /boot/BITSOS2/home/.cobalt/cache.bin 1000 0600
CALL stat '{"path":"/boot/BITSOS2/.push-home"}' > /dev/null 2>&1 && fail "staging tree of the home left"
echo "ok: --home pushed into the user's home with the user's owner, kept what only the home has"

echo "== 9. reboot, then the dry run reads the disk"
CALL reboot '{}' > "$RUN_DIR/reboot.out" || fail "reboot refused"
sleep 5
wait_mcp
"${PUSH[@]}" --tree "$T" --dry-run | tee "$RUN_DIR/after.out" | grep -v "^  "
grep -q "push: dry run: 0 files would change, 0 would be removed" "$RUN_DIR/after.out" || fail "not equal after the reboot"
"${PUSH[@]}" --tree "$H" --name home --home --dry-run > "$RUN_DIR/home-after.out"
grep -q "push: dry run: 0 files would change, 0 would be removed" "$RUN_DIR/home-after.out" || { cat "$RUN_DIR/home-after.out"; fail "the home is not the tree after the reboot"; }
owner_is /boot/BITSOS2/home/.cobalt 1000 0700
owner_is /boot/BITSOS2/home/.cobalt/storage/a.db 1000 0600
cat "$RUN_DIR/nudge.log"

echo "== 10. on the host"
stop_run "$WPID" "$NAME"
WPID=""
FSCK=$(fsck.fat -n "$IMG" 2>&1) || fail "fsck.fat -n is not clean: $FSCK"
rm -rf "$RUN_DIR/back"
mkdir -p "$RUN_DIR/back"
mcopy -s -i "$IMG" ::/BITSOS2/mcpushtest "$RUN_DIR/back/"
diff -r "$T" "$RUN_DIR/back/mcpushtest" > "$RUN_DIR/diff.out" || { head "$RUN_DIR/diff.out"; fail "the package on the image differs from the tree"; }
mcopy -s -i "$IMG" ::/BITSOS2/home "$RUN_DIR/back/"
diff -r -x georg-keep.txt "$H" "$RUN_DIR/back/home" > "$RUN_DIR/diff-home.out" || { head "$RUN_DIR/diff-home.out"; fail "the home on the image differs from the tree"; }
[ "$(mtype -i "$IMG" ::/BITSOS2/home/georg-keep.txt)" = mine ] || fail "georg-keep.txt is not on the image"
mdir -b -i "$IMG" ::/BITSOS2 | grep -qi "\.push-\|mcpushw" && fail "a staging tree or the measurement package is left"
mtype -i "$IMG" ::/EFI/BOOT/BOOTX64.EFI | cmp -s - "$RUN_DIR/loader.before" || fail "\\EFI\\BOOT\\BOOTX64.EFI changed"
mtype -i "$IMG" ::/BITSOS2/CONFIG.TXT | cmp -s - "$RUN_DIR/config.before" || fail "CONFIG.TXT changed"
mdir -/ -b -i "$IMG" ::/BITSOS2/SYS | sort | cmp -s - "$RUN_DIR/sys.before" || fail "SYS changed"
grep -aq "$TOKEN" "$LOG" && fail "the token is in the serial log"
if grep -a "PANIC\|lock.violation" "$LOG"; then fail "a failure line in the serial log"; fi
echo "ok: fsck clean, the package and the home equal their trees, no staging left, loader, CONFIG.TXT and SYS unchanged"
echo "mc-40-push: green"
