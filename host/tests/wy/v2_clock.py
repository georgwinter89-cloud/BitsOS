#!/usr/bin/env python3
"""WY-17b scenario client (43-v2-clock.sh): the status bar's clock on V2, over MCP.

Usage: v2_clock.py <mcp host port> <token> <qmp socket> <run dir> <local|utc|undeclared: what QEMU's RTC holds, and whether the kernel knows>
Exit 0 = green."""
import datetime
import os
import re
import sys
import time
import zoneinfo

sys.dont_write_bytecode = True  # no __pycache__ in the tree
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
RTC = sys.argv[5]
sys.argv = sys.argv[:5] + ["-"]
import v2_check as v  # noqa: E402
from v2_check import Job, call, connect, need  # noqa: E402

SOCK = "wayland-1"
# The guest's clock and this script's may part by a little (QEMU's timer; a slow start). Far below the hour of CET.
SLACK_MIN = 5


def ctl(c, *words):
    _, text, _ = call(c, "run", {"path": "/bin/bitscomp", "args": ["ctl", "--socket", SOCK, *words], "env": v.ENV, "timeout_ms": 20000})
    return text


def apart(a, b):
    """Minutes between two times of day, around midnight."""
    d = abs(a - b) % (24 * 60)
    return min(d, 24 * 60 - d)


def main():
    c = connect(300)
    Job(c, "/bin/bitscomp", ["--socket", SOCK], sock=SOCK)
    clock = None
    for _ in range(60):
        m = re.search(r"clock (\S+ \d\d\.\d\d\. · (\d\d):(\d\d)( UTC)?) (local=.*)", ctl(c, "clock"))
        if m:
            clock = m
            break
        time.sleep(1)
    need(clock is not None, "`ctl clock` answers with a time", ctl(c, "clock"))
    text, hh, mm, utc, why = clock.groups()
    print(f"clock: {text!r} {why}", flush=True)
    need(utc is None and "local=true" in why, f"the clock is not marked UTC: {text!r}", why)
    need("localtime=/usr/share/zoneinfo/Europe/Berlin" in why and "localtime_tzif=true" in why, "/etc/localtime is the shipped zone", why)
    # local: QEMU's RTC holds this machine's local time, the kernel takes it for Berlin's: the clock shows the RTC.
    # utc (CONFIG tz=UTC): the RTC holds UTC, the kernel takes it so: the clock shows Berlin's time.
    # undeclared (the Mini-PC today, wy-WY-17b.md): the RTC holds UTC, the kernel takes it for Berlin's: every program's
    # CLOCK_REALTIME is an hour or two early, libc adds the zone back, and the clock shows UTC's time, unmarked.
    utc_now = datetime.datetime.now(datetime.timezone.utc)
    berlin = datetime.datetime.now(zoneinfo.ZoneInfo("Europe/Berlin"))
    now = {"utc": berlin, "undeclared": utc_now}.get(RTC, datetime.datetime.now())
    shown, host = int(hh) * 60 + int(mm), now.hour * 60 + now.minute
    gap = apart(shown, host)
    need(gap <= SLACK_MIN, f"RTC in {RTC} time: the clock shows {hh}:{mm}, expected {now.hour:02}:{now.minute:02}", why)
    # Today in Berlin: +60 or +120. Zero is the old bug: UTC shown as if local.
    off = re.search(r"offset_min=(-?\d+)", why)
    need(off is not None and int(off.group(1)) in (60, 120), "libc answers Berlin's offset", why)

    # What every program reads, as the kernel reports it. Only where the RTC's zone is known: `local` depends on this host.
    if RTC in ("utc", "undeclared"):
        _, info, _ = call(c, "hw_info", {})
        w = re.search(r"time wall utc=(\d+-\d+-\d+T\d+:\d+:\d+)Z", info)
        need(w is not None, "hw_info names the wall clock", info[-800:])
        wall = datetime.datetime.strptime(w.group(1), "%Y-%m-%dT%H:%M:%S").replace(tzinfo=datetime.timezone.utc)
        ahead = utc_now if RTC == "utc" else utc_now - berlin.utcoffset()
        late = abs((wall - ahead).total_seconds()) / 60
        need(late <= SLACK_MIN, f"RTC {RTC}: the kernel's UTC is {w.group(1)}, expected {ahead:%H:%M}", info[-800:])
        print(f"wall: {w.group(1)}Z at {utc_now:%H:%M}Z", flush=True)


if __name__ == "__main__":
    main()
