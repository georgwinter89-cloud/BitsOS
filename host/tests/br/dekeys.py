#!/usr/bin/env python3
"""BR-6: text -> the evdev key codes `bitscomp ctl key` takes, on bitscomp's German layout (state.rs: xkb "de").
"+N" presses, "-N" releases, "N" taps. Used by 50-sandbox.sh (tools/reference.sh REF_KEYS on the host) and by
v2_sandbox.py (V2), so both type the same keys.
  dekeys.py [--ctrl KEY] [TEXT]   prints the codes (Ctrl+KEY first, then TEXT, then Enter when TEXT ends in \\n)"""
import sys

CTRL, SHIFT, ENTER = 29, 42, 28
# linux-6.6/include/uapi/linux/input-event-codes.h, placed as on a German keyboard (Y and Z swapped).
_ROW = {**{c: k for c, k in zip("qwertzuiop", range(16, 26))}, **{c: k for c, k in zip("asdfghjkl", range(30, 39))},
        **{c: k for c, k in zip("yxcvbnm", range(44, 51))}, **{c: k for c, k in zip("1234567890", range(2, 12))}}
_SHIFTED = {":": 52, "/": 8, "?": 12, "_": 53, "=": 11, "&": 7, "!": 2, "%": 6}
_PLAIN = {".": 52, ",": 51, "-": 53, " ": 57, "\n": ENTER}


def codes(text):
    out = []
    for ch in text:
        if ch in _ROW:
            out.append(str(_ROW[ch]))
        elif ch.lower() in _ROW:
            out += [f"+{SHIFT}", str(_ROW[ch.lower()]), f"-{SHIFT}"]
        elif ch in _SHIFTED:
            out += [f"+{SHIFT}", str(_SHIFTED[ch]), f"-{SHIFT}"]
        elif ch in _PLAIN:
            out.append(str(_PLAIN[ch]))
        else:
            raise ValueError(f"no key for {ch!r} on the German layout table")
    return out


def ctrl(ch):
    return [f"+{CTRL}", str(_ROW[ch]), f"-{CTRL}"]


if __name__ == "__main__":
    a = sys.argv[1:]
    words = []
    if a[:1] == ["--ctrl"]:
        words += ctrl(a[1])
        a = a[2:]
    if a:
        words += codes(a[0].replace("\\n", "\n"))
    print(" ".join(words))
