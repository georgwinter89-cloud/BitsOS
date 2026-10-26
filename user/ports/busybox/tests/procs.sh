# Processes and /proc: ps/pidof/pgrep/pkill/killall, nice/renice/ionice/taskset/chrt, setsid, timeout, time, free/uptime/top/mpstat/iostat, lsof/fuser/pmap/pwdx, sysctl.
d=$1; mkdir -p "$d" && cd "$d" || exit 9
# A private copy of busybox named sleep: pidof/killall match it by its exe path, pgrep/pkill by its command line, so
# nothing else on the machine (the host run!) can match.
mkdir b && cp "$(command -v ls)" b/sleep || exit 9
s=$d/b/sleep
$s 30 & p=$!
sleep 0.2
pidof $s | grep -qw $p && echo pidof; pgrep -f "^$s 30" | grep -qw $p && echo pgrep
ps -o pid,ppid,comm | awk -v p=$p '$1 == p { print ($2 == '$$') ? "ps-ppid" : "ps-ppid-wrong", $3 }'
ps -o stat,args | grep -q "$s 30" && echo ps-args
tr "\\0" " " < /proc/$p/cmdline | sed "s|^$d/|D/|"; echo; readlink /proc/$p/exe | grep -q . && echo proc-exe; grep -q '^Name:' /proc/$p/status && echo proc-status
pwdx $p | grep -q "$d" && echo pwdx; pmap $p | tail -n 1 | grep -q '^mapped: [0-9]*K' && echo pmap; ls /proc/$p/fd | grep -q . && echo proc-fd
a=$(ps -o pid,nice | awk -v p=$p '$1 == p { print $2 }'); renice -n 3 -p $p > /dev/null; b=$(ps -o pid,nice | awk -v p=$p '$1 == p { print $2 }'); [ "$b" = $((a + 3)) ] && echo renice
taskset -p 1 $p > /dev/null && taskset -p $p | sed 's/.*: //'; chrt -p $p | head -n 1 | sed 's/.*: //'
ionice -p $p | grep -q . && echo ionice
pkill -f "^$s 30"; wait $p; echo "pkill $?"
$s 30 & p=$!; sleep 0.2; killall $s; wait $p; echo "killall $?"
setsid sh -c 'ps -o pid,sid | awk -v me=$$ "\$1 == me { print (\$2 == me) ? \"setsid\" : \"no-setsid\" }"'
timeout -s INT 1 $s 5; echo "timeout $?"
time -p true 2>&1 | head -n 1 | cut -d ' ' -f 1
echo "nice $(( $(nice -n 5 nice) - $(nice) ))"; kill -l 15; kill -l | wc -w | grep -q '^[3-9][0-9]' && echo kill-l
free -m | awk 'NR == 1 { print $1, $2 } /^Mem:/ { print ($2 > 0) ? "free-m" : "free-zero" }'
uptime | sed 's/.*load average: [0-9.]*, [0-9.]*, [0-9.]*$/load/'; cat /proc/loadavg | awk '{ print NF }'
mpstat 2>/dev/null | grep -q CPU && echo mpstat; iostat -c 2>/dev/null | grep -q user && echo iostat
top -b -n 1 | grep -q '^Mem:' && echo top
lsof 2>/dev/null | grep -q . && echo lsof; fuser . 2>/dev/null | grep -q . && echo fuser
sysctl -n kernel.ostype; cat /proc/sys/kernel/pid_max | grep -q '^[0-9]' && echo pid_max
pstree $$ | grep -q sh && echo pstree; w 2>/dev/null | head -n 1 | grep -q 'load average' && echo w
(trap 'echo child-term; exit 0' TERM; sh -c 'kill -TERM $PPID'; echo not-reached); echo after
cd / && rm -rf "$d" && echo end
