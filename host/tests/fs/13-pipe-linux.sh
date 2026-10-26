#!/bin/bash
# FS-3: checks on Linux itself what the ktests fs.pipe.* and fs.fifo.open_waits expect of
# host/tests/fs/progs/fs-pipe.c (a 1 MB pipeline, 8 writers of records on one pipe that are never torn, SIGPIPE
# default/ignored/blocked, a FIFO open that waits for a writer). The single-process cases are checked by 10-cases-linux.sh (cases.txt) and `cargo xtask abi-diff
# abi-pipe` (the same program as user/abi-tests/c/pipe.c).
set -eu
cd "$BITSOS_ROOT"
RUN="${BITSOS_RUN_DIR:-$(mktemp -d)}"
mkdir -p "$RUN"
cc -O1 -o "$RUN/fs-pipe" host/tests/fs/progs/fs-pipe.c
fail=0
want() { # want NAME EXPECTED GOT
    if [ "$2" = "$3" ]; then echo "fs-pipe: $1 ok"; else echo "fs-pipe: $1: want [$2] got [$3]"; fail=1; fi
}
want pipeline "got 1000000 sum 17868753804800535352 pattern ok" "$("$RUN/fs-pipe" produce 1000000 | "$RUN/fs-pipe" consume)"
want many-writers "records 2400 from 8 writers whole" "$( (for i in 0 1 2 3 4 5 6 7; do "$RUN/fs-pipe" records "$i" 300 & done; wait) | "$RUN/fs-pipe" check-records 8 300)"
for mode in sigpipe sigpipe-ign sigpipe-block; do
    set +e
    out="$("$RUN/fs-pipe" "$mode")"
    rc=$?
    set -e
    case $mode in
        sigpipe) want "$mode" "141:writing" "$rc:$out" ;;
        sigpipe-ign) want "$mode" "0:writing
write: -EPIPE" "$rc:$out" ;;
        sigpipe-block) want "$mode" "0:writing
write: -EPIPE
SIGPIPE pending: yes" "$rc:$out" ;;
    esac
done
fifo="$RUN/fifo"
rm -f "$fifo"
mkfifo -m 666 "$fifo"
"$RUN/fs-pipe" fifo-read "$fifo" > "$RUN/fifo.out" &
reader=$!
sleep 0.3
if ! kill -0 "$reader" 2>/dev/null; then echo "fs-pipe: fifo-read did not wait for a writer"; fail=1; fi
printf 'hello fifo' > "$fifo"
wait "$reader"
want fifo-read "fifo got 10: hello fifo" "$(cat "$RUN/fifo.out")"
rm -f "$fifo"
exit "$fail"
