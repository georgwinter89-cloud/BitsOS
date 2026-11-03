#!/usr/bin/env python3
"""NW-10: a cable from a QEMU card (-netdev dgram) to a tap device, so the guest's TCP partner is the host
kernel's own TCP (QEMU's user network terminates TCP in slirp: the sender's behaviour would be slirp's).
Runs only inside a private user + network namespace (`unshare -rn`, see 70-node-tap.sh): the tap and every
address on it exist only there, the host's network is never touched.

Usage: tap_cable.py <tap name> <local udp port> <qemu udp port> <server ip> <guest ip>
Frames from the tap go to 127.0.0.1:<qemu port>; datagrams to 127.0.0.1:<local port> go into the tap. The cable
answers the guest's DHCP itself (dnsmasq cannot drop its group in a user namespace): <guest ip>/24 from <server
ip>, no router and no DNS, so the guest's default route stays on its other card.
"""
import fcntl
import os
import select
import socket
import struct
import sys

TUNSETIFF = 0x400454CA
IFF_TAP = 0x0002
IFF_NO_PI = 0x1000

name, local, qemu = sys.argv[1], int(sys.argv[2]), int(sys.argv[3])
SERVER, GUEST = socket.inet_aton(sys.argv[4]), socket.inet_aton(sys.argv[5])
MAC = bytes.fromhex("020000775701")


def csum(b):
    if len(b) % 2:
        b += b"\0"
    s = sum(struct.unpack(f"!{len(b) // 2}H", b))
    while s >> 16:
        s = (s & 0xFFFF) + (s >> 16)
    return ~s & 0xFFFF


def dhcp_reply(f):
    """The OFFER/ACK for a DISCOVER/REQUEST frame, or None."""
    if len(f) < 42 or f[12:14] != b"\x08\x00" or f[23] != 17:
        return None
    h = f[14:]
    ihl = (h[0] & 15) * 4
    d = h[ihl:]
    if len(d) < 8 + 240 or struct.unpack("!H", d[2:4])[0] != 67:
        return None
    b = d[8:]
    o, i = {}, 240
    while i + 1 < len(b) and b[i] != 255:
        if b[i] == 0:
            i += 1
            continue
        o[b[i]] = b[i + 2:i + 2 + b[i + 1]]
        i += 2 + b[i + 1]
    kind = o.get(53, b"\0")[0]
    if b[0] != 1 or kind not in (1, 3):
        return None
    r = bytes([2, 1, 6, 0]) + b[4:8] + b"\0\0" + b[10:12] + b[12:16] + GUEST + SERVER + b"\0" * 4 + b[28:44]
    r += b"\0" * 192 + bytes([0x63, 0x82, 0x53, 0x63, 53, 1, 2 if kind == 1 else 5, 54, 4]) + SERVER
    r += bytes([51, 4]) + struct.pack("!I", 3600) + bytes([1, 4, 255, 255, 255, 0, 255])
    bc = b"\xff" * 4
    u = bytearray(struct.pack("!HHHH", 67, 68, 8 + len(r), 0) + r)
    u[6:8] = struct.pack("!H", csum(SERVER + bc + struct.pack("!BBH", 0, 17, len(u)) + bytes(u)) or 0xFFFF)
    ip = bytearray(struct.pack("!BBHHHBBH4s4s", 0x45, 0, 20 + len(u), 0, 0, 64, 17, 0, SERVER, bc))
    ip[10:12] = struct.pack("!H", csum(bytes(ip)))
    return b"\xff" * 6 + MAC + b"\x08\x00" + bytes(ip) + bytes(u)


tap = os.open("/dev/net/tun", os.O_RDWR)
fcntl.ioctl(tap, TUNSETIFF, struct.pack("16sH", name.encode(), IFF_TAP | IFF_NO_PI))
u = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
u.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, 8 << 20)
u.setsockopt(socket.SOL_SOCKET, socket.SO_SNDBUF, 8 << 20)
u.bind(("127.0.0.1", local))
peer = ("127.0.0.1", qemu)
print(f"tap_cable: {name} <-> udp {local}/{qemu}", flush=True)
while True:
    r, _, _ = select.select([tap, u], [], [])
    if tap in r:
        f = os.read(tap, 65536)
        try:
            u.sendto(f, peer)
        except OSError:
            pass  # QEMU not up yet: the frame is lost, as on a cable without a partner
    if u in r:
        f = u.recv(65536)
        reply = dhcp_reply(f)
        if reply is not None:
            u.sendto(reply, peer)
            print(f"tap_cable: DHCP {'OFFER' if f.find(bytes([53, 1, 1])) > 0 else 'ACK'} {sys.argv[5]}", flush=True)
            continue
        try:
            os.write(tap, f)
        except OSError:
            pass
