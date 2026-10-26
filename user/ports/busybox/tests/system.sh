# Applets that read /proc and system calls; their numbers differ per machine, so only shapes are checked.
ps -o pid,comm | head -n 1 | tr -s ' ' | sed 's/^ //'
ps -o pid,comm | awk 'NR > 1' | grep -q . && echo ps-rows
free | awk '/^Mem:/ { print ($2 > 0) ? "mem" : "mem-zero" }'
uname -s
uptime | grep -q 'load average' && echo uptime
date +%Y | grep -q '^20' && echo date
test -d /proc/self && echo proc-self
nproc | grep -q '^[1-9]' && echo nproc
top -b -n 1 | head -n 1 | cut -d' ' -f1
df / > /dev/null && echo df
echo paged | less
printf 'one\n' > "$1.vi" && vi -c ':s/one/two/' -c ':wq' "$1.vi" < /dev/null > /dev/null 2>&1; cat "$1.vi"; rm -f "$1.vi"
echo end
