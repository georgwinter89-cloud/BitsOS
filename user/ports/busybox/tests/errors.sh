# Failures and exit statuses: missing command, failing pipeline stage, error texts, signalled children, status
# wrapping, and the script's own exit status (case() and run-host.sh add it as the last line "exit N").
nosuchcmd 2>&1; echo "missing $?"
true | false; echo "pipe $?"
cat /nonexistent 2>&1; echo "cat $?"
ls /nonexistent 2>&1; echo "ls $?"
/ 2>&1; echo "dir $?"
sh -c 'kill -TERM $$'; echo "term $?"
sh -c 'kill -KILL $$'; echo "kill $?"
sh -c 'exit 300'; echo "wrap $?"
( exit 3 ) || echo "or $?"
false && echo unreachable; echo "and $?"
echo end
exit 7
