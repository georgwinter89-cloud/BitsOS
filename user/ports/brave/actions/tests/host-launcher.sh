#!/bin/sh
# brave-act's host test (tests/host_brave.rs): the launcher's interface `brave [URL] [-- ARGS]` on the staged tree
# (BRAVE_STAGE), headless (no window on the desktop), a fresh profile in AC3_HOME. Its fds 3/4 (the DevTools
# pipe) go through to Brave as through the real launcher.
S=${BRAVE_STAGE:?}
URL=
if [ -n "$1" ] && [ "$1" != "--" ]; then URL=$1; shift; fi
[ "$1" != "--" ] || shift
echo "brave: starting ${URL:-the start page}"
exec env -i PATH=/usr/bin HOME="$AC3_HOME" LD_LIBRARY_PATH="$S/lib" "$S/brave" --headless=new --no-sandbox --disable-gpu \
  --no-first-run --no-default-browser-check --password-store=basic --disable-breakpad --user-data-dir="$AC3_HOME/profile" \
  --window-size=1280,800 "$@" ${URL:+"$URL"}
