# Packaging: every applet busybox lists has a link next to sh that leads to busybox, and each one starts as its own
# process (`<applet> --help` prints the usage through busybox's common path; a signal end means a crash).
dir=$(dirname "$(command -v ls)")
n=0; for a in $(busybox --list); do
  n=$((n + 1))
  case $(readlink -f "$dir/$a") in */busybox) ;; *) echo "not linked: $a" ;; esac
  "$dir/$a" --help > /dev/null 2>&1 < /dev/null; rc=$?
  [ $rc -ge 128 ] && echo "crashed: $a ($rc)"
done
[ "$n" -gt 300 ] && echo "applets listed"
busybox 2>&1 | head -n 1 | cut -d ' ' -f 1-2
# Applets on a syscall V2 lacks (ENOSYS today: sethostname, syslog, setuid, SysV IPC) or the host user may not use
# (EPERM): an error exit, never a signal. hostname NAME must be refused for uid 1000; `hostname kept` catches a
# sethostname without the permission check. A 2 MiB cp (FEATURE_USE_SENDFILE is off: read/write in 64 KiB steps).
d=$1; mkdir -p "$d" || exit 9
dd if=/dev/zero of="$d/big" bs=65536 count=32 2> /dev/null
for c in "hostname bb-not-set" dmesg "su -c true" ipcs "cp $d/big $d/big2"; do
  $c > /dev/null 2>&1 < /dev/null; rc=$?
  [ $rc -lt 128 ] && echo "clean: ${c%% *}"
done
[ "$(hostname)" != bb-not-set ] && echo "hostname kept"
cmp -s "$d/big" "$d/big2" && echo "cp copied"
echo end
