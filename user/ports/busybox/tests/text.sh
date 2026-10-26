# Text tools on fixed input ($1 = scratch dir): head/tail/wc/sort/uniq/cut/paste/comm/join, sed, grep, awk, od/hexdump/xxd, base64, checksums.
d=$1; mkdir -p "$d" && cd "$d" || exit 9
printf '3 cherry\n1 apple\n2 banana\n1 apple\n10 date\n' > f
head -n 2 f; tail -n 1 f; tail -n +4 f | head -n 1; head -c 7 f; echo
wc -l < f; wc -w < f; wc -c < f
sort f | uniq -c | sed 's/^ *//'
sort -n -r f | head -n 2; sort -k2 f | tail -n 1; sort -u -t ' ' -k1,1n f | tr '\n' ,; echo
cut -d ' ' -f 2 f | tr '\n' ' '; echo; cut -c 1-3 f | head -n 1
seq 3 > a; printf 'x\ny\nz\n' > b; paste a b | tr '\t' :; paste -s -d + a
printf 'a\nb\nc\n' > c1; printf 'b\nc\nd\n' > c2; comm -12 c1 c2 | tr '\n' ' '; echo; comm -3 c1 c2 | tr '\t\n' '_ '; echo
tac a | tr '\n' ' '; echo; nl -b a a | tr -s ' \t' ' ' | sed 's/^ //' | head -n 1
echo abcdefghij | fold -w 4 | tail -n 1; printf 'a\tb\n' | expand -t 4 | od -c | head -n 1 | tr -s ' '
sed -n '2,3p' f; sed -e '/apple/d' -e 's/ /=/' f | head -n 1; sed -i 's/date/fig/' f && grep -c fig f
grep -E '^(1|2) ' f | wc -l; grep -v apple f | wc -l; grep -o 'an' f | wc -l; grep -i CHERRY f; grep -n -w 10 f; grep -q nothing f; echo "grep $?"
awk '{ s += $1; n[$2]++ } END { printf "%d %d\n", s, n["apple"] }' f; awk -F ' ' 'NR == 2 { print toupper($2), length($2) }' f
awk 'BEGIN { x = sprintf("%05.1f|%-4s|%x", 3.14159, "ab", 255); print x; print substr("hello", 2, 3), index("hello", "ll"), split("a:b:c", arr, ":") }'
printf 'Hi\n' | od -A x -t x1 | head -n 1 | tr -s ' '; printf 'Hi\n' | hexdump -C | head -n 1; printf 'Hi\n' | xxd
printf 'BusyBox on V2' | base64; echo QnVzeUJveA== | base64 -d; echo
printf 'abc' | md5sum; printf 'abc' | sha1sum; printf 'abc' | sha512sum | cut -c 1-16; printf 'abc' | cksum; printf 'abc' | crc32
md5sum a > a.md5 && md5sum -c a.md5
printf 'one two\n' | tr a-z A-Z; printf 'aabbcc\n' | tr -s abc; printf 'hello\n' | tr -d l; echo abc | rev
printf '%s=%d %5.2f %c%%\n' x 42 2.5 Z; printf '%b' 'tab\there\n'
seq -s , 1 5; seq 10 -3 1 | tr '\n' ' '; echo; echo '2^10 + 1' | bc; echo '6 7 * p' | dc; factor 360
printf 'a b\nb c\n' | tsort | tr '\n' ' '; echo; printf 'x\0y\n' | strings -n 1 | wc -l
printf 'l1\r\nl2\r\n' | dos2unix | od -c | head -n 1 | tr -s ' '
echo hi | tee t1 t2 > /dev/null; cat t1 t2; split -l 2 f part && ls part* | tr '\n' ' '; echo
diff -u c1 c2 | tail -n +3; patch c1 <<'P'
--- c1
+++ c1
@@ -1,3 +1,3 @@
 a
-b
+B
 c
P
cat c1 | tr '\n' ' '; echo
printf 'k1 v1\nk2 v2\n' > j1; printf 'k1 w1\nk2 w2\n' > j2; cat j1 j2 | sort | uniq -d | wc -l
expr length "busybox" ; expr 7 % 3; expr abc : 'a\(.\)'
cd / && rm -rf "$d" && echo end
