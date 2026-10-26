# Shell semantics: exit codes, subshells, background jobs, signals and traps, functions, here-documents.
( exit 3 ); echo "subshell $?"
false | true; echo "pipe $?"
sleep 10 & p=$!; kill $p; wait $p; echo "killed $?"
sleep 0.1 & wait; echo "waited $?"
trap 'echo trapped' USR1; kill -USR1 $$; echo after
f() { local v=$1; echo "func $v"; }; f 7
for i in 1 2 3; do printf '%s ' $i; done; echo
case abc in a*) echo match ;; esac
echo $(( 7 * 6 ))
read -r line <<EOT
heredoc line
EOT
echo "$line"
sh -c 'exit 5'; echo "child $?"
timeout 1 sleep 5; echo "timeout $?"
env -i A=1 env
i=0; while [ $i -lt 20 ]; do i=$((i + 1)) & wait; i=$((i + 1)); done; echo "loop $i"
echo "$(echo nested $(echo deep))"
exec 3> /dev/null; echo hidden >&3; exec 3>&-; echo fd3
sleep 5 & p=$!; kill -STOP $p; sleep 0.2; grep -q '^State:.*T' /proc/$p/status && echo stopped; kill -CONT $p; kill $p; wait $p; echo "stopcont $?"
echo end
