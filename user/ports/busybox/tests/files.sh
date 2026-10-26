# Files in a scratch directory ($1): create, copy, move, link, search, archive, remove.
d=$1
mkdir -p "$d/sub" && cd "$d" || exit 9
echo first > a.txt
echo second >> a.txt
cat a.txt
cp a.txt sub/b.txt && mv sub/b.txt sub/c.txt && ls sub
grep -n sec a.txt
sed 's/first/1st/' a.txt > s.txt && cat s.txt
ln -s a.txt link && readlink link && wc -l < link
find . -name '*.txt' | sort
test -f a.txt && echo exists
stat -c '%s %F' a.txt
chmod 640 a.txt && stat -c %a a.txt
touch -d '2020-01-02 03:04:05' a.txt && date -r a.txt +%Y-%m-%d
tar -cf t.tar a.txt sub && tar -tf t.tar
gzip -c a.txt | gunzip -c | tail -n 1
dd if=/dev/zero bs=1024 count=4 2>/dev/null | wc -c
head -c 100 /dev/urandom | wc -c
diff a.txt s.txt > /dev/null; echo "diff $?"
cmp a.txt a.txt && echo same
du -s sub > /dev/null && echo du
rm -r sub t.tar && ls
cd / && rm -rf "$d" && test ! -e "$d" && echo removed
