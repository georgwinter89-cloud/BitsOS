#!/usr/bin/env python3
"""NW-10: reads a QEMU filter-dump capture (pcap, Ethernet) and summarises each TCP download towards the guest:
SYN options, segment sizes, ACK rate, the guest's advertised window, flight size, resends and the longest
pauses of the sender with what was in flight then. No tcpdump/tshark needed.

Usage: pcap_tcp.py <file.pcap> [server port]
"""
import struct
import sys
from collections import Counter


def frames(path):
    with open(path, "rb") as f:
        g = f.read(24)
        if len(g) < 24:
            return
        endian = "<" if g[:4] == b"\xd4\xc3\xb2\xa1" else ">"
        nano = g[:4] in (b"\x4d\x3c\xb2\xa1", b"\xa1\xb2\x3c\x4d")
        while True:
            h = f.read(16)
            if len(h) < 16:
                return
            s, frac, incl, _ = struct.unpack(endian + "IIII", h)
            yield s + frac / (1e9 if nano else 1e6), f.read(incl)


def tcp(fr):
    if len(fr) < 34 or fr[12:14] != b"\x08\x00":
        return None
    ip = fr[14:]
    ihl = (ip[0] & 15) * 4
    if ip[9] != 6:
        return None
    tot = struct.unpack(">H", ip[2:4])[0]
    t = ip[ihl:tot]
    sp, dp, seq, ack, off, fl, win = struct.unpack(">HHIIBBH", t[:16])
    doff = (off >> 4) * 4
    opts = t[20:doff]
    # The capture keeps only the headers (filter-dump maxlen): the payload length comes from the IP header.
    return dict(src=bytes(ip[12:16]), dst=bytes(ip[16:20]), sp=sp, dp=dp, seq=seq, ack=ack, fl=fl, win=win,
                opts=opts, plen=tot - ihl - doff)


def options(o):
    out, i = {}, 0
    while i < len(o):
        k = o[i]
        if k == 0:
            break
        if k == 1:
            i += 1
            continue
        if i + 1 >= len(o):
            break
        n = o[i + 1]
        v = o[i + 2:i + n]
        if k == 2:
            out["mss"] = struct.unpack(">H", v)[0]
        elif k == 3:
            out["ws"] = v[0]
        elif k == 4:
            out["sack_ok"] = True
        elif k == 5:
            out["sack"] = len(v) // 8
        elif k == 8:
            out["ts"] = True
        i += max(n, 2)
    return out


def main():
    path = sys.argv[1]
    port = int(sys.argv[2]) if len(sys.argv) > 2 else None
    conns = {}
    for ts, fr in frames(path):
        p = tcp(fr)
        if not p:
            continue
        if port is not None and port not in (p["sp"], p["dp"]):
            continue
        srv_first = port is not None and p["sp"] == port
        key = (p["sp"], p["dp"]) if srv_first or (port is None and p["sp"] < p["dp"]) else (p["dp"], p["sp"])
        c = conns.setdefault(key, [])
        c.append((ts, p, p["sp"] == key[0]))
    for (sport, cport), pk in sorted(conns.items(), key=lambda x: x[1][0][0]):
        report(sport, cport, pk)


def report(sport, cport, pk):
    t0 = pk[0][0]
    syn = {True: {}, False: {}}
    for ts, p, from_srv in pk:
        if p["fl"] & 2:
            syn[from_srv] = options(p["opts"])
    ws_guest = syn[False].get("ws", 0) if "ws" in syn[True] else 0
    data = [(ts, p) for ts, p, s in pk if s and p["plen"] > 0]
    acks = [(ts, p) for ts, p, s in pk if not s and p["fl"] & 16]
    total = sum(p["plen"] for _, p in data)
    if not data:
        return
    dur = data[-1][0] - data[0][0]
    sizes = Counter(p["plen"] for _, p in data)
    mss = max(sizes)
    small = sum(n for s, n in sizes.items() if s < mss)
    print(f"== server :{sport} -> guest :{cport}: {total} bytes in {dur * 1000:.1f} ms "
          f"({total / max(dur, 1e-9) / 1e6:.1f} MB/s), {len(data)} data segments, {small} below {mss}")
    print(f"   SYN server {syn[True]}  guest {syn[False]}")
    top = ", ".join(f"{s}x{n}" for s, n in sizes.most_common(6))
    print(f"   sizes {top}")
    wins = sorted(p["win"] << ws_guest for _, p in acks)
    if wins:
        zero = sum(1 for w in wins if w == 0)
        print(f"   guest ACKs {len(acks)} ({len(acks) / len(data):.2f} per data segment); window min {wins[0]} "
              f"median {wins[len(wins) // 2]} max {wins[-1]}, zero {zero}")
    # Resends: a data segment that starts below the highest end seen so far.
    hi, resent = None, 0
    for _, p in data:
        end = (p["seq"] + p["plen"]) & 0xFFFFFFFF
        if hi is not None and ((p["seq"] - hi) & 0xFFFFFFFF) > 0x7FFFFFFF:
            resent += 1
        if hi is None or ((end - hi) & 0xFFFFFFFF) < 0x7FFFFFFF:
            hi = end
    print(f"   resent segments {resent}")
    # Pauses of the sender: what was in flight and what the guest offered at the time.
    events = sorted([(ts, "d", p) for ts, p in data] + [(ts, "a", p) for ts, p in acks], key=lambda e: e[0])
    last_ack, last_win, snd_nxt, prev = None, None, None, None
    gaps = []
    ack_gaps = []
    last_ack_ts = None
    for ts, kind, p in events:
        if kind == "a":
            last_ack, last_win = p["ack"], p["win"] << ws_guest
            if last_ack_ts is not None:
                ack_gaps.append(ts - last_ack_ts)
            last_ack_ts = ts
            continue
        if prev is not None and snd_nxt is not None and last_ack is not None:
            flight = (snd_nxt - last_ack) & 0xFFFFFFFF
            gaps.append((ts - prev, prev - t0, flight, last_win))
        prev = ts
        end = (p["seq"] + p["plen"]) & 0xFFFFFFFF
        if snd_nxt is None or ((end - snd_nxt) & 0xFFFFFFFF) < 0x7FFFFFFF:
            snd_nxt = end
    gaps.sort(reverse=True)
    hist = Counter()
    for g, *_ in gaps:
        hist["<1ms" if g < 1e-3 else "1-5ms" if g < 5e-3 else "5-20ms" if g < 20e-3 else "20-100ms" if g < 0.1 else ">=100ms"] += 1
    print(f"   pauses between data segments: {dict(hist)}; sum of pauses >= 5 ms: {sum(g for g, *_ in gaps if g >= 5e-3) * 1000:.0f} ms")
    for g, at, flight, win in gaps[:6]:
        print(f"     {g * 1000:7.2f} ms at {at * 1000:9.2f} ms: in flight {flight}, guest window {win}")
    if ack_gaps:
        ack_gaps.sort()
        print(f"   guest ACK spacing median {ack_gaps[len(ack_gaps) // 2] * 1e3:.2f} ms, max {ack_gaps[-1] * 1e3:.2f} ms")


if __name__ == "__main__":
    main()
