#!/bin/sh
# Runs every case (*.sh) with our busybox on the host (CachyOS = the oracle) the way the ktests run it on V2:
# `sh -c <case> sh <scratch dir>`, env HOME=/ PATH=<dir of applet links>, stdin empty. Compares stdout with
# <case>.out (plus a last line "exit N", the status of sh -c, as the ktest adds it); --update writes the .out files instead.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
bb=$here/../out/busybox
[ -x "$bb" ] || { echo "run-host: no $bb (run user/ports/busybox/build.sh)" >&2; exit 1; }
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
mkdir "$tmp/bin"
while read -r a; do ln -s "$bb" "$tmp/bin/$a"; done < "$here/../out/applets"
ln -s "$bb" "$tmp/bin/busybox"
fail=0
for c in "$here"/*.sh; do
  name=$(basename "$c" .sh)
  [ "$name" = run-host ] && continue
  code=0
  # "# as: root" cases (V2 runs them as uid 0): root of new user, mount, net and uts namespaces, so the host is untouched.
  # A new net namespace starts with lo down (V2's lo is always up): the case itself sets no interface.
  as=; pre=
  if [ "$(head -n 1 "$c")" = "# as: root" ]; then
    as="unshare --user --map-root-user --mount --net --uts"
    pre="ip link set lo up 2> /dev/null
"
  fi
  got=$($as env -i HOME=/ PATH="$tmp/bin" "$tmp/bin/sh" -c "$pre$(cat "$c")" sh "$tmp/scratch-$name" < /dev/null 2> "$tmp/$name.err") || code=$?
  got="$got
exit $code"
  if [ "${1:-}" = --update ]; then
    printf '%s\n' "$got" > "$here/$name.out"
    echo "run-host: wrote $name.out"
  elif printf '%s\n' "$got" | diff -u "$here/$name.out" - > "$tmp/$name.diff"; then
    echo "run-host: $name same"
  else
    echo "run-host: $name differs:"; cat "$tmp/$name.diff"; echo "--- stderr:"; cat "$tmp/$name.err"; fail=1
  fi
done
exit $fail
