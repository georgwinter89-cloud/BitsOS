# Pipelines and text applets (no files): ash forks a process per stage and joins them with pipes.
echo "hello busybox" | tr a-z A-Z
printf 'b\na\nc\nb\n' | sort | uniq -c | sed 's/^ *//'
seq 1 100 | awk '{ s += $1 } END { print s }'
echo one two three | cut -d' ' -f2
seq 5 | sort -rn | head -n 2 | tail -n 1
echo abc | wc -c
expr 6 \* 7
basename /a/b/c.txt .txt
x=$(echo bus | rev); echo "$x"
printf abc | md5sum
printf abc | sha256sum
echo "a:b:c" | awk -F: '{ print $3 $2 $1 }'
seq 3 | xargs echo args
yes | head -n 3 | tr '\n' ' '; echo
echo end
