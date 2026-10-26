#!/bin/sh
# BR-4a: CPU load and memory of Brave's processes on BitsOS V2 (busybox sh; also runs on CachyOS for comparison).
# Pushed to the stick and run over MCP: run {"path":"/bin/sh","args":["/boot/BITSOS2/bin/br4-procs","10"]}.
#   br4-procs [SECONDS] [PATTERN]   (default 10 s, pattern /brave/brave)
# One line per process: pid, Chromium's --type (browser for the main process), CPU % of one CPU over SECONDS (from
# utime+stime in /proc/<pid>/stat, USER_HZ 100), RSS and PSS in KB (status VmRSS, smaps_rollup Pss); then the sums
# and the system's MemAvailable. A renderer is one tab (or one site): match it with the window title by time.
secs=${1:-10}
pat=${2:-/brave/brave}
hz=100
pids=
for d in /proc/[0-9]*; do
  [ "${d#/proc/}" = "$$" ] && continue  # this sh itself: its own arguments name the pattern
  c=$(tr '\0' ' ' < "$d/cmdline" 2>/dev/null) || continue
  case $c in *"$pat"*) pids="$pids ${d#/proc/}" ;; esac
done
[ -n "$pids" ] || { echo "br4-procs: no process matches $pat"; exit 1; }
ticks() { sed 's/^.*) //' "/proc/$1/stat" 2>/dev/null | awk '{ print $12 + $13 }'; }
for p in $pids; do eval "t0_$p=$(ticks "$p" || echo 0)"; done
sleep "$secs"
echo "pid type cpu% rss_kb pss_kb"
tot_cpu=0 tot_rss=0 tot_pss=0
for p in $pids; do
  [ -d "/proc/$p" ] || continue
  t1=$(ticks "$p" || echo 0)
  eval "t0=\$t0_$p"
  # Chromium rewrites its argv into one space-separated string: split on both.
  type=$(tr '\0 ' '\n\n' < "/proc/$p/cmdline" 2>/dev/null | sed -n 's/^--type=//p' | head -n 1)
  [ -n "$type" ] || type=browser
  rss=$(awk '/^VmRSS:/ { print $2 }' "/proc/$p/status" 2>/dev/null)
  pss=$(awk '/^Pss:/ { print $2; exit }' "/proc/$p/smaps_rollup" 2>/dev/null)
  cpu=$(awk -v a="$t0" -v b="$t1" -v s="$secs" -v h="$hz" 'BEGIN { printf "%.1f", (b - a) * 100 / (s * h) }')
  echo "$p $type $cpu ${rss:-0} ${pss:-0}"
  tot_cpu=$(awk -v a="$tot_cpu" -v b="$cpu" 'BEGIN { print a + b }')
  tot_rss=$((tot_rss + ${rss:-0}))
  tot_pss=$((tot_pss + ${pss:-0}))
done
echo "sum cpu%=$tot_cpu rss_kb=$tot_rss pss_kb=$tot_pss over ${secs}s"
awk '/^MemAvailable:/ { print "MemAvailable_kb=" $2 }' /proc/meminfo
