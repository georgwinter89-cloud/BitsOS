#!/bin/bash
# FS-4: checks on Linux itself what the ktests fs.epoll.programs_wait and fs.epoll.kill_wakes_waiter expect of
# host/tests/fs/progs/fs-epoll.c (each wait kind sleeps until data arrives twice, then sees EOF; a program asleep
# in epoll_wait(-1) ends when killed). The single-process modes are checked by `cargo xtask abi-diff abi-epoll`
# (the same program as user/abi-tests/c/epoll.c).
set -eu
cd "$BITSOS_ROOT"
RUN="${BITSOS_RUN_DIR:-$(mktemp -d)}"
mkdir -p "$RUN"
cc -O1 -o "$RUN/fs-epoll" host/tests/fs/progs/fs-epoll.c
fail=0
want() { # want NAME EXPECTED GOT
    if [ "$2" = "$3" ]; then echo "fs-epoll: $1 ok"; else echo "fs-epoll: $1: want [$2] got [$3]"; fail=1; fi
}
for k in epoll epoll-et poll ppoll select pselect; do
    got="$( (sleep 0.05; printf abc; sleep 0.05; printf defgh) | "$RUN/fs-epoll" waiter "$k")"
    want "waiter $k" "$k got 8 sum 2758628677764" "$got"
done
mkfifo "$RUN/never"
exec 3<>"$RUN/never"
"$RUN/fs-epoll" waiter epoll <&3 > "$RUN/killed.out" &
w=$!
sleep 0.2
if ! kill -0 "$w" 2>/dev/null; then echo "fs-epoll: waiter epoll did not wait"; fail=1; fi
kill -9 "$w"
set +e
wait "$w"
rc=$?
set -e
want kill "137" "$rc"
exec 3>&-
rm -f "$RUN/never"
exit "$fail"
