#!/usr/bin/env bash
# Host tests of the Cobalt framebuffer backend (YT-1), on CachyOS directly:
# the backend's core is plain C++17 + Linux headers, no Cobalt tree needed.
# Builds twice: optimised, and with AddressSanitizer + UBSan.
# Output and temp files stay under <worktree>/target/yt.
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
wt=$(cd "$here/../../.." && pwd)
src=$wt/user/ports/cobalt-launcher/src/fb
out=$wt/target/yt/host
mkdir -p "$out/include/starboard/shared/bitsos" "$out/tmp"
ln -sfn "$src" "$out/include/starboard/shared/bitsos/fb"
cxx=${CXX:-g++}
files=("$here/fb_test.cc" "$src"/fb_pixels.cc "$src"/fb_config.cc "$src"/fb_device.cc
       "$src"/evdev_decoder.cc "$src"/evdev_devices.cc "$src"/fb_compositor.cc "$src"/fb_yuv.cc "$src"/console_guard.cc "$src"/av_stats.cc)
flags=(-std=c++17 -Wall -Wextra -Werror -pthread -I "$out/include")
nice -n 10 "$cxx" "${flags[@]}" -O2 -o "$out/fb_test" "${files[@]}"
nice -n 10 "$cxx" "${flags[@]}" -O1 -g -fsanitize=address,undefined -fno-sanitize-recover=all \
  -o "$out/fb_test_asan" "${files[@]}"
export YT_TEST_TMP=$out/tmp
# YT-5: the A/V numbers, their own small binary.
av=("$here/av_stats_test.cc" "$src"/av_stats.cc)
nice -n 10 "$cxx" "${flags[@]}" -O1 -g -fsanitize=address,undefined -fno-sanitize-recover=all \
  -o "$out/av_stats_test" "${av[@]}"
# The writer thread once more under ThreadSanitizer (cannot be combined with ASan).
nice -n 10 "$cxx" "${flags[@]}" -O1 -g -fsanitize=thread -o "$out/av_stats_test_tsan" "${av[@]}"
# The two rules patch 0007 takes from src/fb (video size cap, ALSA played count).
nice -n 10 "$cxx" "${flags[@]}" -O1 -g -fsanitize=address,undefined -fno-sanitize-recover=all \
  -o "$out/starboard_rules_test" "$here/starboard_rules_test.cc"
echo "== optimised"; "$out/fb_test"
echo "== sanitizers"; "$out/fb_test_asan"
echo "== A/V numbers (sanitizers)"; "$out/av_stats_test"
echo "== A/V numbers (ThreadSanitizer)"; "$out/av_stats_test_tsan"
echo "== Starboard rules (sanitizers)"; "$out/starboard_rules_test"
echo "== speed (host, optimised)"; "$out/fb_test" --bench
rm -rf "$out"/tmp/fbtest-* "$out"/tmp/avtest-*
echo "== PNG dump tool"
mkdir -p "$out/tmp/shot"
python3 -I "$here/fbshot_test.py" "$wt/user/ports/cobalt-launcher/scripts" "$out/tmp/shot"
rm -rf "$out/tmp/shot"
