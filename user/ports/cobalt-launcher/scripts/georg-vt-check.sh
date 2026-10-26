#!/usr/bin/env bash
# [Georg] check only: YouTube TV through the V2 backend on a real CachyOS text
# console (fbdev /dev/fb0, keyboard/mouse/pad from /dev/input). Agents never
# run this. Start it on a text console (Ctrl+Alt+F3, log in), not in the
# desktop. Reading /dev/input needs the "input" group or root, hence sudo.
# Ctrl+Alt+Backspace ends Cobalt (the keyboard is held exclusively while it
# runs, so Ctrl+Alt+F2 back to the desktop works only afterwards).
set -euo pipefail
case "$(tty)" in /dev/tty[0-9]*) ;; *) echo "please start this on a text console (Ctrl+Alt+F3)"; exit 1;; esac
OUT=/var/tmp/claude-v2/yt/cobalt/cobalt/src/out/evergreen-x64-bitsos_qa
H=/var/tmp/claude-v2/yt/cobalt/runs-yt/georg-vt; mkdir -p "$H"
# However Cobalt ends (even a crash or kill), the console goes back to text
# mode afterwards: KDSETMODE (0x4B3A) KD_TEXT (0) on this console. Cobalt
# does this itself at a normal end and on most crashes; this is the backstop.
text_mode() {
  python3 -I -c 'import fcntl, os; fd = os.open("/dev/tty", os.O_RDWR); fcntl.ioctl(fd, 0x4B3A, 0)' 2>/dev/null || true
}
trap text_mode EXIT
cd "$OUT"
rc=0
sudo env HOME="$H" BITSOS_BACKEND=fb BITSOS_FB=/dev/fb0 BITSOS_KD_GRAPHICS=1 \
  ./loader_app --evergreen_lite --url=https://www.youtube.com/tv > "$H/cobalt.log" 2>&1 || rc=$?
text_mode
echo "Cobalt ended (code $rc). Log: $H/cobalt.log"
exit $rc
