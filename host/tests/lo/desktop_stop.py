#!/usr/bin/env python3
"""TS-1, host/tests/lo/20-desktop-stop.sh: the steps of ktests.py's bare run against QEMU. Usage: desktop_stop.py <host:port>"""
import os
import sys
import time

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "minipc"))
import ktests  # noqa: E402
import mp  # noqa: E402

PROBE = ktests.BARE_PROBE


def probe(o):
    """The class rule lets the test run under QEMU whoever holds the screen: its own answer says who does."""
    err, text, _ = mp.call(o, "ktest", {"filter": PROBE, "timeout_ms": 60000})
    if err:
        raise SystemExit(f"RED: ktest: {text}")
    return text


def until(what, secs, cond):
    end = time.time() + secs
    while time.time() < end:
        try:
            if cond():
                print(f"ok: {what}", flush=True)
                return
        except SystemExit:
            pass
        time.sleep(2)
    raise SystemExit(f"RED: {what}: not within {secs} s")


def main():
    o = mp.options(sys.argv[1:], positional=1)
    until("the desktop holds the screen (the test is Busy)", 120, lambda: f"FAIL {PROBE}" in probe(o) and "Busy" in probe(o))
    held = ktests.holders(o)
    print("holders with the desktop: " + "; ".join(held), flush=True)
    if not any(" bitscomp " in f" {l} " and "/dev/" in l for l in held):
        raise SystemExit("RED: the look through /proc does not see bitscomp holding its devices: the check after the stop would prove nothing")
    err, text, _ = ktests.sh(o, "exec bitscomp ctl quit")
    print(f"bitscomp ctl quit: {'error ' if err else ''}{text.strip()}", flush=True)
    until("the console has the screen back (the test passes)", 60, lambda: f"ok {PROBE}" in probe(o))
    left = ktests.holders(o)
    if left:
        raise SystemExit("RED: still held after the stop: " + "; ".join(left))
    print("ok: no process holds the screen's or the input's devices", flush=True)
    err, text, _ = mp.call(o, "send_keys", {"text": "bitscomp\n"})
    if err:
        raise SystemExit(f"RED: send_keys: {text}")
    until("the desktop typed into the shell holds the screen again", 60, lambda: f"FAIL {PROBE}" in probe(o))
    # The script's own words for the same steps, as the bare run uses them.
    ktests.stop_desktop(o)
    if ktests.screen_held(o):
        raise SystemExit("RED: ktests.screen_held says held with the console on the screen")
    ktests.start_desktop(o)
    if not ktests.screen_held(o):
        raise SystemExit("RED: ktests.start_desktop ended without the desktop on the screen")
    ktests.stop_desktop(o)


if __name__ == "__main__":
    main()
