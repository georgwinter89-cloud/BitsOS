# usage: evsend.py <fifo> [--keymap de|us] [--delay S] token...
# Writes raw Linux input_event records (24 bytes each) into the fake evdev FIFO
# of the framebuffer backend. Tokens:
#   Up Down Left Right Return Escape BackSpace space a..z 0..9   one key (press + release)
#   type:some text      letters, digits and spaces
#   mouse:DX,DY         relative pointer move (screen pixels)   click / rclick   wheel:N
#   pad:a|b|x|start     gamepad buttons   hat:up|down|left|right   stick:up|down|left|right
#   sleep:SECONDS
import os, struct, sys, time
KEY = {"Escape": 1, "BackSpace": 14, "Tab": 15, "Return": 28, "space": 57, "Up": 103, "Left": 105,
       "Right": 106, "Down": 108, "Home": 102, "End": 107, "PageUp": 104, "PageDown": 109}
ROWS = {16: "qwertyuiop", 30: "asdfghjkl", 44: "zxcvbnm"}
for base, row in ROWS.items():
    for i, ch in enumerate(row):
        KEY[ch] = base + i
for i, ch in enumerate("1234567890"):
    KEY[ch] = 2 + i
EV_SYN, EV_KEY, EV_REL, EV_ABS = 0, 1, 2, 3
PAD = {"a": 0x130, "b": 0x131, "x": 0x133, "y": 0x134, "start": 0x13b, "select": 0x13a}

def rec(t, c, v):
    return struct.pack("<qqHHi", 0, 0, t, c, v)

def main():
    args = sys.argv[1:]
    fifo = args.pop(0)
    keymap, delay = "de", 0.0
    while args and args[0].startswith("--"):
        opt = args.pop(0)
        if opt == "--keymap":
            keymap = args.pop(0)
        elif opt == "--delay":
            delay = float(args.pop(0))
    fd = os.open(fifo, os.O_WRONLY | os.O_NONBLOCK)

    def send(*records):
        os.write(fd, b"".join(records) + rec(EV_SYN, 0, 0))

    def key(code):
        send(rec(EV_KEY, code, 1))
        send(rec(EV_KEY, code, 0))

    def letter(ch):
        if keymap == "de" and ch in "yz":  # QWERTZ: the key labelled Z sits where US has Y
            ch = "z" if ch == "y" else "y"
        return KEY[ch]

    for tok in args:
        if tok.startswith("sleep:"):
            time.sleep(float(tok[6:]))
            continue
        if tok.startswith("type:"):
            for ch in tok[5:]:
                lower = ch.lower()
                if ch == " ":
                    key(KEY["space"])
                elif lower in KEY:
                    code = letter(lower) if lower.isalpha() else KEY[lower]
                    if ch.isupper():
                        send(rec(EV_KEY, 42, 1))
                        key(code)
                        send(rec(EV_KEY, 42, 0))
                    else:
                        key(code)
                time.sleep(max(delay, 0.15))
            continue
        if tok.startswith("mouse:"):
            dx, dy = (int(v) for v in tok[6:].split(","))
            send(rec(EV_REL, 0, dx), rec(EV_REL, 1, dy))
        elif tok in ("click", "rclick"):
            b = 0x110 if tok == "click" else 0x111
            send(rec(EV_KEY, b, 1))
            send(rec(EV_KEY, b, 0))
        elif tok.startswith("wheel:"):
            send(rec(EV_REL, 8, int(tok[6:])))
        elif tok.startswith("pad:"):
            key(PAD[tok[4:]])
        elif tok.startswith("hat:") or tok.startswith("stick:"):
            d = tok.split(":")[1]
            hat = tok.startswith("hat:")
            axis = (0x10 if hat else 0) + (1 if d in ("up", "down") else 0)
            val = (1 if hat else 32000) * (-1 if d in ("up", "left") else 1)
            send(rec(EV_ABS, axis, val))
            send(rec(EV_ABS, axis, 0))
        elif tok in KEY:
            key(letter(tok) if len(tok) == 1 and tok.isalpha() else KEY[tok])
        else:
            sys.exit(f"evsend: unknown token {tok!r}")
        if delay:
            time.sleep(delay)
    os.close(fd)

main()
