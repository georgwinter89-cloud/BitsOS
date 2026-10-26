# Coreutils and file system calls in a scratch dir ($1): paths, dirs, links, modes, times, find/xargs, archives, fifos, locks.
d=$1; mkdir -p "$d" && cd "$d" || exit 9
basename /a/b/c.txt .txt; dirname /a/b/c.txt; pwd | grep -q "$d" && echo pwd
mkdir -p x/y/z && rmdir x/y/z && ls x; mkdir x 2>/dev/null || echo "mkdir exists"
echo data > f; ln f hard && stat -c %h f; ln -sf f sym && readlink sym && readlink -f sym | grep -q "/f$" && echo readlink-f
realpath x/../f | grep -q "$d/f$" && echo realpath; unlink hard && stat -c %h f; link f hard2 && ls hard2
ls -l f | cut -c 1-10; ls -a | head -n 2 | tr '\n' ' '; echo; ls -1 | wc -l; ls -d x; ls -F sym x | tr '\n' ' '; echo
truncate -s 1000 f && stat -c %s f; truncate -s 3 f && cat f; echo; fallocate -l 4096 big && stat -c %s big
TZ=UTC0 touch -t 202001020304.05 f && stat -c %Y f | grep -x 1577934245; TZ=UTC0 ls -l --full-time f | grep -q '2020-01-02 03:04:05' && echo full-time
chmod 600 f && ls -l f | cut -c 1-10; chmod u+x,g+r f && stat -c %A f; chmod -R 750 x && stat -c %a x/y | grep -x 750
install -m 640 f inst && stat -c %a inst | grep -x 640; cp -p inst inst2 && stat -c %a inst2 | grep -x 640; cp -r x x2 && ls x2
mv x2 x3 && test ! -e x2 && ls x3; mktemp -p . tmp.XXXXXX | grep -q '^./tmp\.' && echo mktemp; mktemp -d -p . | xargs rmdir && echo mktemp-d
find . -maxdepth 1 -type d | sort | tr '\n' ' '; echo; find . -name 'inst*' -exec ls {} + | sort | tr '\n' ' '; echo
find . -size +2k -type f; find . -name f -perm 0744 | wc -l
seq 6 | xargs -n 2 echo | tail -n 1; printf 'a b\0c\0' | xargs -0 -n 1 echo | wc -l; echo f | xargs -I % cat %; echo
du -k big | cut -f 2; du -sk . | grep -q . && echo du-s
mkfifo p && stat -c %F p; (echo through-fifo > p &) ; cat p
mknod n p 2>/dev/null && stat -c %F n
dd if=/dev/zero of=z bs=512 count=3 2>/dev/null && stat -c %s z; dd if=f of=g bs=1 skip=1 2>/dev/null && cat g; echo
printf 'x' > s && dd if=s of=z bs=1 seek=1 conv=notrunc 2>/dev/null && head -c 2 z | od -c | head -n 1 | tr -s ' '
sync && echo sync; fsync f && echo fsync
flock lk echo locked; flock -n lk true && echo flock-n
tar -czf a.tgz f x && tar -tzf a.tgz | sort | tr '\n' ' '; echo; mkdir u && tar -xzf a.tgz -C u && ls u
echo f | cpio -o -H newc 2>/dev/null > a.cpio && cpio -t < a.cpio 2>/dev/null; cpio -i -d --quiet -D u2 < a.cpio 2>/dev/null || (mkdir u2 && cd u2 && cpio -i < ../a.cpio 2>/dev/null); cat u2/f; echo
bzip2 -c f | bunzip2 -c; echo; lzop -c f | lzop -dc; echo; gzip -9 big && gunzip big.gz && stat -c %s big
printf 'PK test' > fake.zip; unzip -l fake.zip > /dev/null 2>&1; echo "unzip $?"
cat -n f; cat -A f 2>/dev/null || cat -e f; echo; yes x | head -n 3 | tr -d '\n'; echo
date -u -d @0 +'%Y-%m-%d %H:%M:%S'; date -u -d '2021-03-04 05:06:07' +%s; date -u -r f +%Y | grep -x 2020
env A=1 B=2 sh -c 'echo $A$B'; printenv HOME; id -u; id -g
which ls | grep -q '/ls$' && echo which; command -v cd; type true | grep -q builtin && echo type
test -r f -a -w f && echo rw; [ -x x ] && echo x-dir; [ 3 -gt 2 ] && [ abc \< abd ] && echo compare
nice -n 5 sh -c 'echo niced'; nohup sh -c 'echo nohup' 2>/dev/null; usleep 1000 && echo usleep
tty; echo "tty $?"; mountpoint -q / && echo mountpoint; tree x | tail -n 1
shred -u z && test ! -e z && echo shred; shuf -i 1-5 | sort -n | tr '\n' ' '; echo; hostid | grep -q '^[0-9a-f]' && echo hostid
cd / && rm -rf "$d" && echo end
