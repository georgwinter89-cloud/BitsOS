# Networking basics beyond net.sh ($1 scratch dir, $2/$3 ports from the ktest; host run 28532/28533): names, ip, netstat/route/arp (/proc/net), refused connections, UDP, wget errors.
d=$1; p1=${2:-28532}; p2=${3:-28533}
mkdir -p "$d" && cd "$d" || exit 9
hostname | grep -q . && echo hostname; uname -n | grep -q . && echo uname-n
ipcalc -n -b 192.168.100.113/24; ipcalc -m 10.1.2.3/8
ip -4 addr show lo | grep -o 'inet [0-9./]*'; ip -o link show lo | awk '{ print $2 }'
ip route show table local 2>&1 | grep -q 'local 127.0.0.1' && echo ip-route-local
route -n 2>/dev/null | head -n 1; arp -n > /dev/null 2>&1 && echo arp
nc -l -p $p1 > got & n=$!
i=0; until netstat -tln 2>&1 | grep -q ":$p1 "; do i=$((i + 1)); [ $i -ge 25 ] && break; sleep 0.2; done
[ $i -lt 25 ] && echo netstat-listen
netstat -tn 2>&1 | head -n 1 | cut -d ' ' -f 1-2
echo over-tcp | nc 127.0.0.1 $p1; wait $n; cat got
nc 127.0.0.1 $p2 < /dev/null 2> /dev/null; echo "nc $?"
wget -q -O - http://127.0.0.1:$p2/ 2> err; echo "wget $?"; sed 's/ ([^)]*)//' err
nc -u -l -p $p2 > udp & u=$!
i=0; until netstat -uln 2>&1 | grep -q ":$p2 "; do i=$((i + 1)); [ $i -ge 25 ] && break; sleep 0.2; done
echo over-udp | nc -u -w 1 127.0.0.1 $p2
i=0; until grep -q over-udp udp; do i=$((i + 1)); [ $i -ge 25 ] && echo "udp lost" && break; sleep 0.2; done
kill $u; wait $u 2>/dev/null; cat udp
cd / && rm -rf "$d" && echo end
