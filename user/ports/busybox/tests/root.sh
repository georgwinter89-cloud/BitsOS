# as: root
# Root-only applets ($1 scratch dir): mount/df/umount of a tmpfs, chown, chroot, raw-socket ping and traceroute,
# interface lists. Runs as uid 0 on V2; on the host as root of a new user, mount, net and uts namespace (run-host.sh,
# which also sets the namespace's lo up), so nothing touches the machine.
# What it changes on V2, deliberately on every rerun (Mini-PC Safe runs): a tmpfs mounted on its own scratch dir in
# the one mount table, unmounted again in the same run; a mount still there at the end prints `leaked-mount` (an
# extra line, so the case fails) and is unmounted. chroot changes only its own child's root. No interface,
# hostname, clock or sysctl writes.
d=$1; mkdir -p "$d/m" && cd "$d" || exit 9
id -u
mount -t tmpfs bbtmp "$d/m" && echo mounted
grep -q " $d/m tmpfs" /proc/mounts && echo proc-mounts
mount | grep -q " on $d/m type tmpfs" && echo mount-list
df "$d/m" 2>/dev/null | awk 'NR == 2 { print ($2 > 0) ? "df" : "df-zero" }'
echo in-tmpfs > m/f && cat m/f
umount "$d/m" && test ! -e m/f && echo unmounted
: > o && chown 0:0 o && stat -c '%u %g' o
chroot / true && echo chroot
ping -c 1 -W 2 127.0.0.1 2>&1 | grep -q 'bytes from 127.0.0.1' && echo ping
traceroute -n -m 1 -q 1 -w 2 127.0.0.1 2> /dev/null | grep -q '^ 1  127.0.0.1' && echo traceroute
ifconfig | grep -q '^lo' && echo ifconfig-list
grep -q " $d/m " /proc/mounts 2> /dev/null && { echo leaked-mount; umount "$d/m"; }
cd / && rm -rf "$d" && echo end
