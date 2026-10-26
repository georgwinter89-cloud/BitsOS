# Networking over loopback ($1 = scratch dir, $2/$3 = the two ports: the ktest gives each run its own pair): httpd + wget, nc listen + nc connect, ip, ifconfig; ping as a user (EPERM as on Linux; a real echo needs root: console test and Mini-PC check list).
# The ports lie in lane UL's range (28500-28999), so the host run cannot clash with other lanes.
d=$1; hp=${2:-28530}; np=${3:-28531}
mkdir -p "$d/www" && echo "served by httpd" > "$d/www/index.html" || exit 9
httpd -f -p 127.0.0.1:$hp -h "$d/www" & h=$!
i=0; until wget -q -O - http://127.0.0.1:$hp/index.html; do i=$((i + 1)); [ $i -ge 25 ] && echo "wget failed (httpd $(kill -0 $h 2>/dev/null && echo running || echo gone))" && break; sleep 0.2; done
kill $h; wait $h
nc -l -p $np > "$d/got" & n=$!
i=0; until echo "via nc" | nc 127.0.0.1 $np; do i=$((i + 1)); [ $i -ge 25 ] && echo "nc failed" && kill $n && break; sleep 0.2; done
wait $n; cat "$d/got"
ip link show lo | grep -q LOOPBACK && echo ip-link
ip addr show lo | grep -q 'inet 127.0.0.1' && echo ip-addr
ifconfig lo | grep -q 127.0.0.1 && echo ifconfig
# ping as an ordinary user (the ktest runs as uid 1000, the host run as Georg): Linux refuses the raw socket.
ping -c 1 -W 2 127.0.0.1 2>&1 | grep -q 'are you root' && echo ping-eperm
rm -rf "$d" && echo end
