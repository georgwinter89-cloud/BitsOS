#!/bin/bash
# slow
# BT-2 screen mode before the kernel (V2-PLAN §7 "Picture"): OVMF's std VGA starts in 1280x800 and offers
# 2560x1440, so bitsboot switches to it by default; screen=WxH overrides; a size the firmware does not offer,
# an unreadable value and screen=firmware keep the firmware's mode (with a loader log event); a display
# without 2560x1440 (cirrus) keeps its mode. The kernel's display line proves what it got.
. "$(dirname "$0")/lib.bash"

# 1. Default: native 2560x1440.
make_image bt-scr-native
boot bt-scr-native s-native 60 --net none
want_re s-native "bitsboot screen.switched from mode [0-9]+ \(1280x800\) to mode [0-9]+ \(2560x1440\)" "switched to the native 2560x1440"
want s-native "screen 2560x1440 stride 2560 BGR at 0x" "the FRAMEBUFFER record has 2560x1440"
want_re s-native "screen modes: .*\(30 modes, current [0-9]+\)" "the mode list names the new current mode"
want s-native "display device name=gopfb width=2560 height=1440" "the kernel draws at 2560x1440"
want_kernel_ok s-native

# 2. screen=1024x768 overrides.
make_image bt-scr-1024 "screen=1024x768"
boot bt-scr-1024 s-1024 60 --net none
want_re s-1024 "bitsboot screen.switched from mode [0-9]+ \(1280x800\) to mode [0-9]+ \(1024x768\)" "screen= overrides the native mode"
want s-1024 "display device name=gopfb width=1024 height=768" "the kernel draws at 1024x768"
want_kernel_ok s-1024

# 3. A size the firmware does not offer: event, the firmware's mode stays.
make_image bt-scr-missing "screen=1234x567"
boot bt-scr-missing s-missing 60 --net none
want_re s-missing "bitsboot screen.fallback wanted=1234x567 firmware_mode=[0-9]+ \(1280x800\): the firmware offers no usable 1234x567 mode" "a missing mode is an event"
want s-missing "display device name=gopfb width=1280 height=800" "the kernel draws in the firmware's mode"
want_kernel_ok s-missing

# 4. An unreadable value keeps the firmware's mode; screen=firmware too.
make_image bt-scr-bad "screen=huge"
boot bt-scr-bad s-bad 60 --net none
want s-bad "bitsboot screen.config_invalid: screen=huge is not WxH, native, largest or firmware" "the bad value is named"
want_re s-bad "bitsboot screen.kept wanted=firmware mode=[0-9]+ \(1280x800\)" "the firmware's mode stays"
want_kernel_ok s-bad
make_image bt-scr-fw "screen=firmware"
boot bt-scr-fw s-fw 60 --net none
want_re s-fw "bitsboot screen.kept wanted=firmware mode=[0-9]+ \(1280x800\)" "screen=firmware keeps the mode"
want s-fw "display device name=gopfb width=1280 height=800" "the kernel draws at 1280x800"

# 5. cirrus offers no 2560x1440: the firmware's mode stays, the kernel draws in it.
boot bt-scr-native s-cirrus 60 --net none -- -vga cirrus
want_re s-cirrus "screen modes: " "cirrus has a mode list"
want_re s-cirrus "bitsboot screen.fallback wanted=2560x1440 firmware_mode=[0-9]+ \([0-9]+x[0-9]+\): the firmware offers no usable 2560x1440 mode" "no native mode on cirrus: event"
want_re s-cirrus "display device name=gopfb width=[0-9]+" "the kernel draws on the firmware's mode"
want_kernel_ok s-cirrus
finish
